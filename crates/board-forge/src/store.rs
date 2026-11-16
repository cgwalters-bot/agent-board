//! The pass's machine-state store (docs/pass.md#the-state-store): a SQLite
//! database the caller carries between applying passes, in the Actions
//! cache with an artifact fallback (agent-board#1052).
//!
//! It indexes the dispatch records, triage verdicts and accepted plans the
//! pass makes or reads, and its latest hold snapshot. The comments stay the
//! source of those decisions: they are written, never read for one, so a
//! lost, stale or forged database cannot change them. A missing or unusable
//! database is replaced by an empty one, which refills from the comments
//! each later pass reads.
use crate::github::DEFAULT_HOST;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, params};
use std::collections::BTreeMap;
use std::path::Path;
use time::OffsetDateTime;

/// The schema, one migration a version: `user_version` counts those
/// applied. Append; never edit one that shipped.
const MIGRATIONS: &[&str] = &[
    "CREATE TABLE dispatches (
        record TEXT PRIMARY KEY,
        item TEXT NOT NULL,
        kind TEXT,
        pool TEXT,
        plan TEXT,
        run INTEGER,
        seen INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX dispatches_item ON dispatches (item);
    CREATE TABLE verdicts (
        comment TEXT PRIMARY KEY,
        item TEXT NOT NULL,
        verdict TEXT NOT NULL,
        operator TEXT,
        at INTEGER NOT NULL,
        seen INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX verdicts_item ON verdicts (item);",
    "CREATE TABLE plans (
        item TEXT PRIMARY KEY,
        plan TEXT NOT NULL,
        kind TEXT NOT NULL,
        seen INTEGER NOT NULL
    ) STRICT;
    CREATE TABLE holds (
        item TEXT,
        epic INTEGER NOT NULL,
        kind TEXT NOT NULL,
        reason TEXT NOT NULL,
        operator INTEGER NOT NULL,
        since TEXT NOT NULL
    ) STRICT;",
];

/// The schema version this build writes.
pub const STORE_SCHEMA: usize = MIGRATIONS.len();

/// A larger file is not opened but rebuilt: the cache restores whatever
/// it holds, and a pass should not spend its time on a runaway database.
pub const STORE_BYTES: u64 = 64 * 1024 * 1024;

/// Rows no pass has seen for this long are dropped, so the database stays
/// bounded; a later read of their comments records them again.
pub const STORE_RETENTION_DAYS: i64 = 90;

/// A dispatch record comment: one the pass posted, whose kind, pool and
/// run it knows, or one it read, whose plan it knows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dispatch {
    /// The issue's key ([`crate::pass::key`]).
    pub item: String,
    pub kind: Option<String>,
    pub pool: Option<String>,
    /// The plan id its marker names ([`crate::plan::plan_id`]).
    pub plan: Option<String>,
    pub run: Option<u64>,
}

/// A triage verdict the pass trusted: the bot's, unedited but by a writer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub item: String,
    pub verdict: String,
    pub operator: Option<String>,
    /// When its comment was posted.
    pub at: OffsetDateTime,
}

/// A plan stage 4 accepted for an issue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The plan id ([`crate::plan::plan_id`]).
    pub id: String,
    pub kind: String,
}

/// One entry of the pass's hold snapshot ([`crate::pass::holds::Published`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hold {
    /// `OWNER/REPO#N`; none for a hold of every new run.
    pub item: Option<String>,
    pub epic: bool,
    pub kind: String,
    pub reason: String,
    pub operator: bool,
    pub since: String,
}

/// What one pass saw, keyed by comment URL, plans by their issue's key;
/// written by [`save`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Records {
    pub dispatches: BTreeMap<String, Dispatch>,
    pub verdicts: BTreeMap<String, Verdict>,
    pub plans: BTreeMap<String, Plan>,
    /// The pass's hold snapshot, which replaces the last; none leaves it.
    pub holds: Option<Vec<Hold>>,
}

impl Records {
    /// Add `dispatch` of the comment `record`, filling in what an earlier
    /// sighting this pass left unknown.
    pub fn dispatch(&mut self, record: &str, dispatch: Dispatch) {
        let known = self.dispatches.entry(record.into()).or_default();
        *known = Dispatch {
            item: dispatch.item,
            kind: dispatch.kind.or(known.kind.take()),
            pool: dispatch.pool.or(known.pool.take()),
            plan: dispatch.plan.or(known.plan.take()),
            run: dispatch.run.or(known.run),
        };
    }

    /// Add the triage `verdict` of the comment at `url`, where its URL names
    /// the issue.
    pub fn verdict(
        &mut self,
        url: &str,
        verdict: &str,
        operator: Option<String>,
        at: OffsetDateTime,
    ) {
        if let Some(item) = comment_item(url) {
            let verdict = verdict.to_owned();
            let entry = Verdict {
                item,
                verdict,
                operator,
                at,
            };
            self.verdicts.insert(url.into(), entry);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.dispatches.is_empty()
            && self.verdicts.is_empty()
            && self.plans.is_empty()
            && self.holds.is_none()
    }
}

/// The issue key of a comment's `url`, `…/OWNER/REPO/issues/N#…` (or
/// `pull`), as [`crate::pass::key`] has it.
fn comment_item(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    let segments: Vec<_> = url.path_segments()?.collect();
    let [.., owner, repo, "issues" | "pull", number] = segments[..] else {
        return None;
    };
    let number: u64 = number.parse().ok()?;
    let repo = format!("{owner}/{repo}#{number}");
    let key = match url.host_str()? {
        DEFAULT_HOST => repo,
        host => format!("{host}/{repo}"),
    };
    Some(key.to_ascii_lowercase())
}

/// What [`save`] found and left.
#[derive(Debug, PartialEq, Eq)]
pub struct Saved {
    /// Why the database was replaced by an empty one, where it was.
    pub rebuilt: Option<String>,
    pub dispatches: u64,
    pub verdicts: u64,
    pub plans: u64,
    pub holds: u64,
}

/// Record `records` at `now` in the database at `path`, migrating it to
/// [`STORE_SCHEMA`], or rebuilding it where it is missing or unusable.
/// Any failure to open it rebuilds it, which suits an index that the
/// comments refill; state with no other copy will need to tell a damaged
/// database from a transient error first.
pub fn save(path: &Path, records: &Records, now: OffsetDateTime) -> Result<Saved> {
    let (mut connection, rebuilt) = match open(path) {
        Ok(connection) => (connection, None),
        Err(why) => {
            replace(path).with_context(|| format!("{why:#}"))?;
            // A cold start, with no cache to restore, has no directory yet.
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            let connection = Connection::open(path)
                .with_context(|| format!("create store {}", path.display()))?;
            migrate(&connection)?;
            (connection, Some(format!("{why:#}")))
        }
    };
    let seen = now.unix_timestamp();
    let tx = connection.transaction().context("store transaction")?;
    for (record, d) in &records.dispatches {
        // SQLite's integers are signed; no run id comes near the bound.
        let run = d.run.and_then(|run| i64::try_from(run).ok());
        tx.execute(
            "INSERT INTO dispatches (record, item, kind, pool, plan, run, seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (record) DO UPDATE SET item = excluded.item,
               kind = coalesce(excluded.kind, kind), pool = coalesce(excluded.pool, pool),
               plan = coalesce(excluded.plan, plan), run = coalesce(excluded.run, run),
               seen = excluded.seen",
            params![record, d.item, d.kind, d.pool, d.plan, run, seen],
        )
        .context("record a dispatch")?;
    }
    for (comment, v) in &records.verdicts {
        tx.execute(
            "INSERT INTO verdicts (comment, item, verdict, operator, at, seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (comment) DO UPDATE SET item = excluded.item,
               verdict = excluded.verdict, operator = excluded.operator,
               at = excluded.at, seen = excluded.seen",
            params![
                comment,
                v.item,
                v.verdict,
                v.operator,
                v.at.unix_timestamp(),
                seen
            ],
        )
        .context("record a verdict")?;
    }
    for (item, p) in &records.plans {
        tx.execute(
            "INSERT INTO plans (item, plan, kind, seen) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (item) DO UPDATE SET plan = excluded.plan,
               kind = excluded.kind, seen = excluded.seen",
            params![item, p.id, p.kind, seen],
        )
        .context("record a plan")?;
    }
    if let Some(holds) = &records.holds {
        tx.execute("DELETE FROM holds", [])
            .context("replace the holds")?;
        for h in holds {
            tx.execute(
                "INSERT INTO holds (item, epic, kind, reason, operator, since)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![h.item, h.epic, h.kind, h.reason, h.operator, h.since],
            )
            .context("record a hold")?;
        }
    }
    let expired = seen - STORE_RETENTION_DAYS * 24 * 60 * 60;
    for table in ["dispatches", "verdicts", "plans"] {
        tx.execute(&format!("DELETE FROM {table} WHERE seen < ?1"), [expired])
            .context("expire old rows")?;
    }
    let count = |table: &str| -> Result<u64> {
        let sql = format!("SELECT count(*) FROM {table}");
        let count: i64 = tx.query_row(&sql, [], |row| row.get(0))?;
        Ok(count.unsigned_abs())
    };
    let saved = Saved {
        rebuilt,
        dispatches: count("dispatches")?,
        verdicts: count("verdicts")?,
        plans: count("plans")?,
        holds: count("holds")?,
    };
    tx.commit().context("commit the store")?;
    Ok(saved)
}

/// An existing, intact database of a schema this build knows, migrated.
/// A symlink is refused: a restored cache could otherwise aim the pass's
/// writes at another file. What a restored cache holds is checked from the
/// cheapest up: its file header, then that its schema is exactly the
/// migrations' at its version, and only then its pages, as an integrity
/// check evaluates the schema's expressions (a planted CHECK constraint's
/// among them) on every row.
fn open(path: &Path) -> Result<Connection> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!("no database"),
        Err(error) => return Err(error).context("inspect the database"),
    };
    ensure!(metadata.is_file(), "not a regular file");
    ensure!(
        metadata.len() <= STORE_BYTES,
        "larger than {STORE_BYTES} bytes"
    );
    // SQLite would play a journal back into the file on its first read,
    // past the bounds checked here; this build never leaves one behind.
    for suffix in JOURNALS {
        let journal = sibling(path, suffix);
        ensure!(
            std::fs::symlink_metadata(&journal).is_err(),
            "a journal beside it: {}",
            journal.display()
        );
    }
    let version = header(path, metadata.len())?;
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .context("open the database")?;
    // Exactly the migrations' tables and indexes at its version: no missing
    // column, and no trigger or constraint planted in a restored cache.
    let reference = Connection::open_in_memory().context("open a reference schema")?;
    migrate_to(&reference, version)?;
    ensure!(
        schema(&connection)? == schema(&reference)?,
        "schema differs from version {version}'s"
    );
    let check: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .context("check the database")?;
    ensure!(check == "ok", "integrity check: {check}");
    migrate(&connection)?;
    Ok(connection)
}

/// The schema version in the 100-byte header of the database file at
/// `path`, `len` bytes long, once the header describes a rollback-journal
/// database of whole pages that this build's schema can be (see
/// <https://sqlite.org/fileformat.html#the_database_header>).
fn header(path: &Path, len: u64) -> Result<usize> {
    use std::io::Read;
    let mut header = [0; 100];
    let file = std::fs::File::open(path).context("read the database's header")?;
    let read = file
        .take(header.len() as u64)
        .read(&mut header)
        .context("read the database's header")?;
    ensure!(
        read == header.len() && header.starts_with(SQLITE_HEADER),
        "not a SQLite database header"
    );
    let u16_at = |at: usize| u16::from_be_bytes([header[at], header[at + 1]]);
    let u32_at = |at: usize| u32::from_be_bytes(header[at..at + 4].try_into().unwrap_or_default());
    let page = match u16_at(16) {
        1 => 65536,
        size => u64::from(size),
    };
    ensure!(
        page.is_power_of_two() && (512..=65536).contains(&page),
        "page size {page} is not one SQLite writes"
    );
    // Rollback journal (1), not WAL (2), whose log a cache does not carry.
    ensure!(
        header[18..20] == [1, 1] && header[21..24] == [64, 32, 32],
        "not a rollback-journal database of this format"
    );
    ensure!(
        len.is_multiple_of(page),
        "{len} bytes is not a whole number of {page}-byte pages"
    );
    // The header's page count holds where it was written with the change
    // counter it records; an older writer may leave it stale.
    let pages = u64::from(u32_at(28));
    ensure!(
        u32_at(24) != u32_at(92) || pages == len / page,
        "the header counts {pages} pages, the file holds {}",
        len / page
    );
    let version = usize::try_from(u32_at(60)).unwrap_or(usize::MAX);
    ensure!(
        version <= STORE_SCHEMA,
        "schema {version} is newer than this build's {STORE_SCHEMA}"
    );
    Ok(version)
}

fn schema(connection: &Connection) -> Result<Vec<(String, String, Option<String>)>> {
    let mut statement = connection
        .prepare("SELECT type, name, sql FROM sqlite_schema ORDER BY type, name")
        .context("read the schema")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Remove what is at `path`, with SQLite's journals, for a fresh database;
/// a regular file must be empty or a SQLite database, so that a mistaken
/// `store` path never deletes another file.
fn replace(path: &Path) -> Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.is_file()
    {
        use std::io::Read;
        let mut header = Vec::new();
        let file = std::fs::File::open(path).context("read the database's header")?;
        file.take(SQLITE_HEADER.len() as u64)
            .read_to_end(&mut header)?;
        ensure!(
            header.is_empty() || header == SQLITE_HEADER,
            "not replacing {}: not a SQLite database",
            path.display()
        );
    }
    for suffix in std::iter::once("").chain(JOURNALS) {
        let file = sibling(path, suffix);
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("remove {}", file.display()));
            }
        }
    }
    Ok(())
}

/// The suffixes of the files SQLite keeps beside a database.
const JOURNALS: [&str; 3] = ["-journal", "-wal", "-shm"];

fn sibling(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut file = path.as_os_str().to_owned();
    file.push(suffix);
    file.into()
}

/// The first bytes of every SQLite database file.
const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";

/// Apply the migrations past the database's `user_version`; a newer
/// schema than this build's is refused, never guessed at.
fn migrate(connection: &Connection) -> Result<()> {
    migrate_to(connection, STORE_SCHEMA)
}

/// Apply the migrations past the database's `user_version` up to `target`.
fn migrate_to(connection: &Connection, target: usize) -> Result<()> {
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("read the schema version")?;
    let version = usize::try_from(version).unwrap_or(usize::MAX);
    ensure!(
        version <= STORE_SCHEMA,
        "schema {version} is newer than this build's {STORE_SCHEMA}"
    );
    for (n, migration) in MIGRATIONS.iter().enumerate().take(target).skip(version) {
        connection
            .execute_batch(&format!(
                "BEGIN; {migration} PRAGMA user_version = {}; COMMIT;",
                n + 1
            ))
            .with_context(|| format!("migrate the schema to {}", n + 1))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + seconds).unwrap()
    }

    fn records() -> Records {
        let mut records = Records::default();
        let started = Dispatch {
            item: "o/r#1".into(),
            kind: Some("triage".into()),
            pool: Some("claude".into()),
            run: Some(7),
            ..Default::default()
        };
        records.dispatch("https://github.com/o/r/issues/1#issuecomment-2", started);
        let url = "https://github.com/O/R/issues/1#issuecomment-3";
        records.verdict(url, "done", Some("no".into()), at(0));
        let plan = Plan {
            id: "9@2027".into(),
            kind: "triage".into(),
        };
        records.plans.insert("o/r#1".into(), plan);
        records
    }

    fn rows(path: &Path) -> Vec<String> {
        let connection = Connection::open(path).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT record || ' ' || item || ' ' || ifnull(kind, '-') || ' '
                   || ifnull(pool, '-') || ' ' || ifnull(plan, '-') || ' '
                   || ifnull(run, '-') FROM dispatches
                 UNION ALL SELECT comment || ' ' || item || ' ' || verdict FROM verdicts
                 ORDER BY 1",
            )
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn comment_items() {
        for (url, item) in [
            (
                "https://github.com/O/R/issues/5#issuecomment-1",
                Some("o/r#5"),
            ),
            (
                "https://github.com/o/r/pull/6#issuecomment-1",
                Some("o/r#6"),
            ),
            (
                "https://forge.example/o/r/issues/7#issuecomment-1",
                Some("forge.example/o/r#7"),
            ),
            ("https://github.com/o/r/issues/x#issuecomment-1", None),
            ("https://github.com/o/r", None),
            ("not a url", None),
        ] {
            assert_eq!(comment_item(url).as_deref(), item, "{url}");
        }
    }

    /// A pass's later sighting fills in what an earlier one left unknown,
    /// here and across passes, and the newest pass's values win. The first
    /// pass is a cold start, whose directory does not exist yet.
    #[test]
    fn records_merge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".state/state.db");
        let record = "https://github.com/o/r/issues/1#issuecomment-2";
        let mut first = records();
        let read = Dispatch {
            item: "o/r#1".into(),
            plan: Some("9@2027".into()),
            ..Default::default()
        };
        first.dispatch(record, read.clone());
        assert_eq!(first.dispatches[record].kind.as_deref(), Some("triage"));
        assert_eq!(first.dispatches[record].plan.as_deref(), Some("9@2027"));
        let saved = save(&path, &first, at(0)).unwrap();
        assert_eq!(
            saved,
            Saved {
                rebuilt: Some("no database".into()),
                dispatches: 1,
                verdicts: 1,
                plans: 1,
                holds: 0,
            }
        );
        let mut second = Records::default();
        second.dispatch(record, read);
        let saved = save(&path, &second, at(60)).unwrap();
        assert_eq!(saved.rebuilt, None);
        assert_eq!(
            rows(&path),
            [
                "https://github.com/O/R/issues/1#issuecomment-3 o/r#1 done",
                "https://github.com/o/r/issues/1#issuecomment-2 o/r#1 triage claude 9@2027 7",
            ]
        );
    }

    /// An evicted, damaged, oversized, altered or newer database is rebuilt,
    /// never fatal; one of an older schema is migrated in place; a file that
    /// is no SQLite database is left alone, as is a symlink's target.
    #[test]
    fn rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let other = dir.path().join("other");
        std::fs::write(&other, "kept").unwrap();
        let sql = |sql: &str| Connection::open(&path).unwrap().execute_batch(sql).unwrap();
        // Overwrite the header of a fresh store at `at`.
        let patch = |offset: usize, bytes: &[u8]| {
            save(&path, &Records::default(), at(0)).unwrap();
            let mut file = std::fs::read(&path).unwrap();
            file[offset..offset + bytes.len()].copy_from_slice(bytes);
            std::fs::write(&path, file).unwrap();
        };
        for (name, expected) in [
            ("evicted", Ok(Some("no database"))),
            ("damaged", Ok(Some("integrity check: "))),
            ("oversized", Ok(Some("larger than"))),
            ("newer", Ok(Some("schema 99 is newer than this build's 2"))),
            ("tableless", Ok(Some("schema differs from version 1's"))),
            ("trigger", Ok(Some("schema differs from version 2's"))),
            // A constraint that every row fails, slowly: refused by its
            // schema before an integrity check would evaluate it.
            ("constraint", Ok(Some("schema differs from version 1's"))),
            ("journal", Ok(Some("a journal beside it: "))),
            ("headless", Ok(Some("not a SQLite database header"))),
            (
                "page size",
                Ok(Some("page size 3 is not one SQLite writes")),
            ),
            ("wal", Ok(Some("not a rollback-journal database"))),
            (
                "partial page",
                Ok(Some("40961 bytes is not a whole number")),
            ),
            (
                "page count",
                Ok(Some("the header counts 9 pages, the file")),
            ),
            // Version 0: an empty database from before any migration.
            ("older", Ok(None)),
            // The first slice's, migrated in place.
            ("version 1", Ok(None)),
            ("symlink", Ok(Some("not a regular file"))),
            (
                "foreign",
                Err("not a SQLite database header: not replacing"),
            ),
            ("directory", Err("not a regular file: remove ")),
        ] {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_dir(&path);
            match name {
                "damaged" => {
                    save(&path, &records(), at(0)).unwrap();
                    let mut bytes = std::fs::read(&path).unwrap();
                    bytes[4096..].fill(0xff);
                    std::fs::write(&path, bytes).unwrap();
                }
                "oversized" => {
                    save(&path, &Records::default(), at(0)).unwrap();
                    let file = std::fs::File::options().append(true).open(&path);
                    file.unwrap().set_len(STORE_BYTES + 1).unwrap();
                }
                "newer" => sql("PRAGMA user_version = 99; VACUUM"),
                "tableless" => sql("PRAGMA user_version = 1; VACUUM"),
                "trigger" => {
                    save(&path, &Records::default(), at(0)).unwrap();
                    sql(
                        "CREATE TRIGGER t AFTER INSERT ON verdicts BEGIN DELETE FROM verdicts; END",
                    );
                }
                "constraint" => sql("PRAGMA ignore_check_constraints = ON;
                     CREATE TABLE t (x CHECK (length(randomblob(1000000)) < 0));
                     INSERT INTO t VALUES (1), (2), (3);
                     PRAGMA user_version = 1"),
                "journal" => {
                    save(&path, &Records::default(), at(0)).unwrap();
                    std::fs::write(sibling(&path, "-journal"), "played back").unwrap();
                }
                "headless" => std::fs::write(&path, SQLITE_HEADER).unwrap(),
                "page size" => patch(16, &[0, 3]),
                "wal" => {
                    save(&path, &Records::default(), at(0)).unwrap();
                    sql("PRAGMA journal_mode = WAL");
                }
                "partial page" => {
                    save(&path, &Records::default(), at(0)).unwrap();
                    let file = std::fs::File::options().append(true).open(&path);
                    std::io::Write::write_all(&mut file.unwrap(), b"x").unwrap();
                }
                "page count" => patch(28, &[0, 0, 0, 9]),
                "older" => sql("VACUUM"),
                "version 1" => migrate_to(&Connection::open(&path).unwrap(), 1).unwrap(),
                "symlink" => std::os::unix::fs::symlink(&other, &path).unwrap(),
                "foreign" => std::fs::write(&path, "{\"not\": \"sqlite\"}").unwrap(),
                "directory" => std::fs::create_dir(&path).unwrap(),
                _ => {}
            }
            let saved = save(&path, &records(), at(0)).map_err(|e| format!("{e:#}"));
            match (expected, saved) {
                (Ok(rebuilt), Ok(saved)) => {
                    let why = saved.rebuilt.as_deref();
                    assert_eq!(why.is_some(), rebuilt.is_some(), "{name}: {why:?}");
                    let why = why.unwrap_or_default();
                    assert!(
                        why.starts_with(rebuilt.unwrap_or_default()),
                        "{name}: {why}"
                    );
                    let counts = (saved.dispatches, saved.verdicts, saved.plans);
                    assert_eq!(counts, (1, 1, 1), "{name}");
                }
                (Err(error), Err(got)) => assert!(got.starts_with(error), "{name}: {got}"),
                (expected, got) => panic!("{name}: expected {expected:?}, got {got:?}"),
            }
            assert!(!sibling(&path, "-journal").exists(), "{name}");
            if name == "foreign" {
                let kept = std::fs::read_to_string(&path).unwrap();
                assert_eq!(kept, "{\"not\": \"sqlite\"}");
            }
        }
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "kept");
    }

    /// Rows unseen for [`STORE_RETENTION_DAYS`] go; rows seen since stay.
    #[test]
    fn expires() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        save(&path, &records(), at(0)).unwrap();
        let day = 24 * 60 * 60;
        let kept = save(&path, &Records::default(), at(STORE_RETENTION_DAYS * day)).unwrap();
        assert_eq!((kept.dispatches, kept.verdicts, kept.plans), (1, 1, 1));
        let gone = save(
            &path,
            &Records::default(),
            at(STORE_RETENTION_DAYS * day + 1),
        )
        .unwrap();
        assert_eq!((gone.dispatches, gone.verdicts, gone.plans), (0, 0, 0));
    }

    /// A pass's hold snapshot replaces the last pass's; one without it
    /// leaves the stored one.
    #[test]
    fn holds_replace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let hold = |item: Option<&str>| Hold {
            item: item.map(Into::into),
            epic: false,
            kind: "needs-operator".into(),
            reason: "why".into(),
            operator: true,
            since: "2027-01-15T08:00:00Z".into(),
        };
        for (holds, expected) in [
            (Some(vec![hold(Some("o/r#1")), hold(None)]), 2),
            (None, 2),
            (Some(vec![hold(Some("o/r#2"))]), 1),
            (Some(vec![]), 0),
        ] {
            let records = Records {
                holds,
                ..Default::default()
            };
            assert_eq!(save(&path, &records, at(0)).unwrap().holds, expected);
        }
    }
}
