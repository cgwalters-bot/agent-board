//! The release train, moved from tracker's `.github/release-train/train.cjs`:
//! cuts a release of the configured repository and moves its callers' pins,
//! one step a pass, as a person would by hand: the version bump PR, the
//! release workflow once it merges, the wrappers' pin PR once the release is
//! out, then each caller's pin PR once that merges. A release cut outside
//! the train is followed from the wrappers' pin bump on, so the "due" rule
//! governs only cutting releases. Every PR goes through its repository's
//! review and queue like any other.
//!
//! A pass keeps no state of its own: it reads the stage from GitHub, from
//! the one open tracking issue, its comments and the PRs on its branch, so a
//! lost or repeated pass neither skips nor repeats a step. Without `apply`
//! it only reads and prints each write as `would: ...`, line for line as
//! train.cjs printed it but for the release and wrappers steps it checks
//! more, so the two can be diffed on the same state.
use crate::github::{API_ROOT, Client, is_github_login};
use crate::poll::{Rest, check_keys, validate_repository, validate_workflow};
use crate::supervise::encode;
use crate::supervise::health::base64_decode;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Write;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Requests a pass may make, however the API answers.
pub const REQUEST_LIMIT: u32 = 200;
/// Every listing is one page of this many; the train's are far shorter.
pub const PAGE_SIZE: u64 = 100;
/// Release notes carried into a pin bump's description, in characters.
const NOTES_LIMIT: usize = 30_000;
const HOUR_MS: f64 = 3_600_000.0;
/// How long a dispatched release run may take to show in the listing.
const DISPATCH_GRACE: time::Duration = time::Duration::HOUR;
const KEYS: &[&str] = &[
    "bot",
    "release",
    "tracking",
    "threshold",
    "intervalHours",
    "labels",
    "wrappers",
    "callers",
];

/// A problem with what is to be edited rather than with the API: retrying
/// would not help, so the train stalls for a person instead.
#[derive(Debug)]
pub struct Stall(pub String);

impl std::fmt::Display for Stall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Stall {}

fn stall<T>(message: impl Into<String>) -> Result<T> {
    Err(Stall(message.into()).into())
}

/// A response outside 2xx: GitHub answered, about what is or is not there.
#[derive(Debug)]
pub struct Http {
    pub method: String,
    pub path: String,
    pub status: u16,
}

impl std::fmt::Display for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: HTTP {}", self.method, self.path, self.status)
    }
}

impl std::error::Error for Http {}

fn status(error: &anyhow::Error) -> Option<u16> {
    error.downcast_ref::<Http>().map(|e| e.status)
}

/// The REST API, by path such as `/repos/O/R/issues`. A status outside 2xx
/// is an [`Http`] error; anything else that fails (the network, the token)
/// is another error.
pub trait Api {
    fn request(&self, method: &str, path: &str, body: &Value) -> Result<Value>;
}

impl Api for Client {
    fn request(&self, method: &str, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{API_ROOT}{}", path.trim_start_matches('/'));
        let (status, body) = Rest::send(self, method, &url, body)?;
        if !(200..300).contains(&status) {
            return Err(Http {
                method: method.into(),
                path: path.split('?').next().unwrap_or_default().into(),
                status,
            }
            .into());
        }
        Ok(body)
    }
}

/// A repository the train opens PRs on, from the bot's fork of it.
#[derive(Clone, Debug)]
pub struct Target {
    pub repository: String,
    /// Configured, as forks need not share their parent's name, and checked
    /// before a push: a fork of another repository could never be its PR.
    pub fork: String,
    /// The files whose pin the train moves (none for the release itself).
    pub files: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Labels {
    /// On a merged PR: release at once.
    pub urgent: String,
    /// On a merged PR: a minor rather than a patch release.
    pub breaking: String,
    /// The tracking issues'.
    pub tracking: String,
    /// Halts a train until a person removes it.
    pub stalled: String,
}

/// The train's configuration file, from the caller's default branch.
#[derive(Clone, Debug)]
pub struct Config {
    /// The bot whose token runs the pass: only its tracking issues count,
    /// as anyone may open one with the same title.
    pub bot: String,
    pub release: Target,
    /// The release repository's workflow that tags and publishes a version
    /// given as its `version` input.
    pub workflow: String,
    /// Where the tracking issues are.
    pub tracking: String,
    /// Merged PRs that make a release due, at most once per `interval_hours`.
    pub threshold: usize,
    pub interval_hours: f64,
    pub labels: Labels,
    /// The repository whose workflows wrap the release's, pinned by the
    /// release tag's commit: a tag can move after its review.
    pub wrappers: Target,
    /// Repositories that pin the wrappers by commit.
    pub callers: Vec<Target>,
}

fn text(value: &Value, key: &str, what: &str) -> Result<String> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("{what} needs {key}, a string"))
}

/// A path in a repository, which goes into an API path with the bot's token.
fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".."),
        "invalid file {path:?}: expected a path such as .github/workflows/review.yml"
    );
    Ok(())
}

fn target(value: &Value, what: &str, keys: &[&str]) -> Result<Target> {
    check_keys(&[keys], value, what)?;
    let repository = text(value, "repository", what)?;
    let fork = text(value, "fork", what)?;
    validate_repository(&repository)?;
    validate_repository(&fork)?;
    let mut files = vec![];
    if keys.contains(&"files") {
        for file in value["files"]
            .as_array()
            .filter(|f| !f.is_empty())
            .with_context(|| format!("{what} needs files, a list of paths"))?
        {
            let file = file.as_str().context("a file is not a string")?;
            validate_path(file)?;
            files.push(file.to_owned());
        }
    }
    Ok(Target {
        repository,
        fork,
        files,
    })
}

impl Config {
    pub fn from_json(value: &Value) -> Result<Self> {
        let what = "release-train config";
        check_keys(&[KEYS], value, what)?;
        let bot = text(value, "bot", what)?;
        ensure!(is_github_login(&bot), "invalid bot {bot:?}");
        let release = target(
            &value["release"],
            "release",
            &["repository", "fork", "workflow"],
        )?;
        let workflow = text(&value["release"], "workflow", "release")?;
        validate_workflow(&workflow)?;
        let tracking = text(value, "tracking", what)?;
        validate_repository(&tracking)?;
        let threshold = value["threshold"]
            .as_u64()
            .context("threshold is not a count")?;
        let interval_hours = value["intervalHours"]
            .as_f64()
            .filter(|h| *h >= 0.0)
            .context("intervalHours is not a number of hours")?;
        let labels = &value["labels"];
        check_keys(
            &[&["urgent", "breaking", "tracking", "stalled"]],
            labels,
            "labels",
        )?;
        let label = |key| text(labels, key, "labels");
        let labels = Labels {
            urgent: label("urgent")?,
            breaking: label("breaking")?,
            tracking: label("tracking")?,
            stalled: label("stalled")?,
        };
        let files = &["repository", "fork", "files"];
        let wrappers = target(&value["wrappers"], "wrappers", files)?;
        let callers = value["callers"]
            .as_array()
            .context("callers is not a list")?
            .iter()
            .map(|c| target(c, "caller", files))
            .collect::<Result<_>>()?;
        Ok(Self {
            bot,
            release,
            workflow,
            tracking,
            threshold: usize::try_from(threshold).unwrap_or(usize::MAX),
            interval_hours,
            labels,
            wrappers,
            callers,
        })
    }

    /// The release repository's name, which names its versions in titles
    /// and beside commit pins ("agentic-job v0.6.4").
    fn project(&self) -> &str {
        self.release
            .repository
            .split_once('/')
            .map_or("", |(_, name)| name)
    }
}

/// `MAJOR.MINOR.PATCH`, each decimal digits.
fn semver(version: &str) -> Option<[u64; 3]> {
    let mut parts = version.split('.').map(|p| {
        p.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| p.parse().ok())
            .flatten()
    });
    let v = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(v)
}

fn parse_version(version: &str) -> Result<[u64; 3]> {
    semver(version).map_or_else(|| stall(format!("not a version: {version}")), Ok)
}

/// The version after `version`: a minor bump if `breaking`, else a patch.
pub fn next_version(version: &str, breaking: bool) -> Result<String> {
    let [major, minor, patch] = parse_version(version)?;
    Ok(match breaking {
        true => format!("{major}.{}.0", minor + 1),
        false => format!("{major}.{minor}.{}", patch + 1),
    })
}

/// Whether version `a` is older than `b`.
pub fn older(a: &str, b: &str) -> Result<bool> {
    Ok(parse_version(a)? < parse_version(b)?)
}

/// A PR merged since the last release.
#[derive(Clone, Debug)]
pub struct Merged {
    pub number: u64,
    pub title: String,
    pub labels: Vec<String>,
}

/// Why a release is due, or none.
pub fn due(
    merged: &[Merged],
    published: OffsetDateTime,
    now: OffsetDateTime,
    config: &Config,
) -> Option<String> {
    let urgent = &config.labels.urgent;
    if merged.is_empty() {
        return None;
    }
    if merged.iter().any(|p| p.labels.contains(urgent)) {
        return Some(format!("a {urgent} fix merged"));
    }
    let since = (now - published).whole_milliseconds() as f64;
    if merged.len() < config.threshold || since < config.interval_hours * HOUR_MS {
        return None;
    }
    Some(format!("{} pull requests merged", merged.len()))
}

/// A PR of the train, as its listing shows it.
#[derive(Clone, Debug, Default)]
pub struct Pull {
    pub state: String,
    pub merged_at: Option<String>,
    pub html_url: String,
    pub merge_commit_sha: Option<String>,
    /// Why the PR on the train's branch is not its step: it is from another
    /// repository or goes into a branch other than the default, whose
    /// review gate it would then have skipped.
    pub stray: Option<String>,
}

impl Pull {
    fn merged(&self) -> bool {
        self.merged_at.is_some()
    }

    fn closed_unmerged(&self) -> bool {
        self.state == "closed" && !self.merged()
    }
}

/// The newest run of the release workflow that the train dispatched: by the
/// bot, at the commit its dispatch comment names.
#[derive(Clone, Debug)]
pub struct Run {
    pub status: String,
    pub conclusion: Option<String>,
}

/// What GitHub shows of an open train; a PR is none until it is opened.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub bump: Option<Pull>,
    /// The release of the train's version, if it is out.
    pub release: Option<Value>,
    pub run: Option<Run>,
    /// How long ago the train recorded its dispatch of the release workflow
    /// on the tracking issue, if it did.
    pub dispatched: Option<time::Duration>,
    pub wrappers: Option<Pull>,
    /// Each caller's repository and PR.
    pub callers: Vec<(String, Option<Pull>)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Bump,
    Release,
    Wrappers,
    /// The callers without a PR yet.
    Callers(Vec<String>),
    Done,
    Wait(String),
    Stall(String),
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Step::Bump => f.write_str("bump"),
            Step::Release => f.write_str("release"),
            Step::Wrappers => f.write_str("wrappers"),
            Step::Callers(_) => f.write_str("callers"),
            Step::Done => f.write_str("done"),
            Step::Wait(reason) => write!(f, "wait ({reason})"),
            Step::Stall(reason) => write!(f, "stall ({reason})"),
        }
    }
}

/// The next step of an open train.
pub fn next(f: &Facts) -> Step {
    let closed = |pr: &Pull, what: &str| match &pr.stray {
        Some(why) => Some(Step::Stall(format!("{what} {why}"))),
        None => pr
            .closed_unmerged()
            .then(|| Step::Stall(format!("{what} was closed unmerged"))),
    };
    let bump_open = f.bump.as_ref().is_some_and(|b| b.state == "open");
    // A release that is out, whoever cut it, needs no bump: the train
    // follows it from the wrappers on. But not past its own bump still
    // open: merged later, that would raise main to a version already
    // released.
    if f.release.is_some() && bump_open {
        return Step::Stall(
            "the version was released while its version bump is still open; close the bump".into(),
        );
    }
    if f.release.is_none() {
        let Some(bump) = &f.bump else {
            return Step::Bump;
        };
        if let Some(stall) = closed(bump, "the version bump") {
            return stall;
        }
        if !bump.merged() {
            return Step::Wait("the version bump".into());
        }
        return match (&f.run, f.dispatched) {
            (None, None) => Step::Release,
            // Dispatched, but its run is not listed yet: never dispatched
            // twice, and given up on once it should long have shown.
            (None, Some(ago)) if ago < DISPATCH_GRACE => {
                Step::Wait("release.yml to start".into())
            }
            (None, Some(_)) => Step::Stall(
                "no run of release.yml shows since the train dispatched it; release by hand, or delete the dispatch comment to dispatch again".into(),
            ),
            (Some(run), _) if run.status != "completed" => Step::Wait("release.yml".into()),
            // Not dispatched again: a failed release needs a look first.
            (Some(run), _) => Step::Stall(format!(
                "release.yml ended {} without a release",
                run.conclusion.as_deref().unwrap_or("null")
            )),
        };
    }
    let Some(wrappers) = &f.wrappers else {
        return Step::Wrappers;
    };
    if let Some(stall) = closed(wrappers, "the wrappers' pin bump") {
        return stall;
    }
    if !wrappers.merged() {
        return Step::Wait("the wrappers' pin bump".into());
    }
    let missing: Vec<String> = f
        .callers
        .iter()
        .filter(|(_, pr)| pr.is_none())
        .map(|(repo, _)| repo.clone())
        .collect();
    if !missing.is_empty() {
        return Step::Callers(missing);
    }
    for (repo, pr) in &f.callers {
        if let Some(stall) = pr
            .as_ref()
            .and_then(|pr| closed(pr, &format!("{repo}'s pin bump")))
        {
            return stall;
        }
    }
    if f.callers
        .iter()
        .all(|(_, pr)| pr.as_ref().is_some_and(Pull::merged))
    {
        return Step::Done;
    }
    Step::Wait("the review.yml pin bumps".into())
}

/// The start of each line in `toml`'s `[workspace.package]` table, the
/// header excluded, until the next table.
fn workspace_lines(toml: &str) -> Vec<usize> {
    const HEADER: &str = "[workspace.package]\n";
    let mut lines = vec![];
    let starts = std::iter::once(0).chain(toml.match_indices('\n').map(|(i, _)| i + 1));
    for start in starts.filter(|&s| toml[s..].starts_with(HEADER)) {
        let mut at = start + HEADER.len();
        while at <= toml.len() {
            lines.push(at);
            let rest = &toml[at..];
            if rest.starts_with('[') {
                break;
            }
            match rest.find('\n') {
                Some(end) => at += end + 1,
                None => break,
            }
        }
    }
    lines
}

/// The workspace's version in Cargo.toml.
pub fn workspace_version(toml: &str) -> Option<&str> {
    workspace_lines(toml).into_iter().find_map(|at| {
        let value = toml[at..].strip_prefix("version = \"")?;
        let end = value.find('"').filter(|&end| end > 0)?;
        Some(&value[..end])
    })
}

/// Raises the workspace version in Cargo.toml and every path package (no
/// `source`) in Cargo.lock from `from` to `to`, as the release
/// repository's own version bumps change them.
pub fn bump_cargo(toml: &str, lock: &str, from: &str, to: &str) -> Result<(String, String)> {
    let old = format!("version = \"{from}\"");
    let new = format!("version = \"{to}\"");
    let raised = workspace_lines(toml)
        .into_iter()
        .find(|&at| toml[at..].starts_with(&old))
        .map(|at| format!("{}{new}{}", &toml[..at], &toml[at + old.len()..]));
    let lock_raised = lock
        .split("\n\n")
        .map(|block| {
            let lines: Vec<&str> = block.split('\n').collect();
            if !block.starts_with("[[package]]") || lines.iter().any(|l| l.starts_with("source = "))
            {
                return block.to_owned();
            }
            let mut done = false;
            let lines: Vec<&str> = lines
                .into_iter()
                .map(|l| match !done && l == old {
                    true => {
                        done = true;
                        new.as_str()
                    }
                    false => l,
                })
                .collect();
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    match raised {
        Some(raised) if lock_raised != lock => Ok((raised, lock_raised)),
        _ => stall(format!("no workspace version {from} to raise")),
    }
}

/// After `s`, which starts with `TARGET/.github/workflows/` of length
/// `prefix`: the length through the `NAME.yml@` that follows, if one does.
fn workflow_at(s: &str, prefix: usize) -> Option<usize> {
    let name = s[prefix..]
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || b"_.-".contains(b))
        .count();
    let end = prefix + name;
    (name > 4 && s[..end].ends_with(".yml") && s[end..].starts_with('@')).then_some(end + 1)
}

/// The leading `X.Y.Z` of `s`, each decimal digits, as long as they go.
fn version_at(s: &str) -> Option<&str> {
    let mut at = 0;
    for i in 0..3 {
        if i > 0 {
            s[at..].strip_prefix('.')?;
            at += 1;
        }
        let digits = s[at..].bytes().take_while(u8::is_ascii_digit).count();
        (digits > 0).then_some(())?;
        at += digits;
    }
    Some(&s[..at])
}

/// The leading `vX.Y.Z` tag or 40-hex commit of `s`.
fn pin_at(s: &str) -> Option<&str> {
    if let Some(version) = s.strip_prefix('v').and_then(version_at) {
        return Some(&s[..version.len() + 1]);
    }
    let hex = s
        .bytes()
        .take(40)
        .take_while(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        .count();
    (hex == 40).then(|| &s[..40])
}

/// After `s`, which starts with `PROJECT ` of length `prefix`: the length
/// through the `v` of `PROJECT v1.2.3` or `PROJECT **v1.2.3`, if one follows.
fn named_at(s: &str, prefix: usize) -> Option<usize> {
    let stars = s[prefix..].bytes().take_while(|&b| b == b'*').count();
    s[prefix + stars..]
        .starts_with('v')
        .then_some(prefix + stars + 1)
}

/// The version of the first `PROJECT vX.Y.Z` in `text`, `named` being
/// `PROJECT `.
fn named_version<'t>(text: &'t str, named: &str) -> Option<&'t str> {
    let at = |s: &str| {
        let v = named_at(s, named.len())?;
        Some(v + version_at(&s[v..])?.len())
    };
    let (start, len) = matches(text, named, at).next()?;
    let found = &text[start..start + len];
    let v = named_at(found, named.len())?;
    Some(&found[v..])
}

/// Each place in `text` where `needle` starts a match of `at`, left to
/// right without overlap; `at` gives the length matched.
fn matches<'a>(
    text: &'a str,
    needle: &'a str,
    at: impl Fn(&str) -> Option<usize> + 'a,
) -> impl Iterator<Item = (usize, usize)> + 'a {
    let mut from = 0;
    std::iter::from_fn(move || {
        while let Some(found) = text.get(from..)?.find(needle) {
            let start = from + found;
            if let Some(len) = at(&text[start..]) {
                from = start + len.max(1);
                return Some((start, len));
            }
            from = start + text[start..].chars().next().map_or(1, char::len_utf8);
        }
        None
    })
}

/// `text` with `old` after each match of (`needle`, `at`), where it is not
/// followed by a digit, replaced by `new`.
fn replace_after(
    text: &str,
    needle: &str,
    at: impl Fn(&str) -> Option<usize>,
    old: &str,
    new: &str,
) -> String {
    let whole = |s: &str| {
        let keep = at(s)?;
        let rest = s[keep..].strip_prefix(old)?;
        (!rest.starts_with(|c: char| c.is_ascii_digit())).then_some(keep + old.len())
    };
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (start, len) in matches(text, needle, whole) {
        out.push_str(&text[copied..start + len - old.len()]);
        out.push_str(new);
        copied = start + len;
    }
    out.push_str(&text[copied..]);
    out
}

/// A pin of a repository's workflows and the version it is.
#[derive(Clone, Debug, PartialEq)]
pub struct Pin {
    /// A `vX.Y.Z` tag or a commit.
    pub pin: String,
    pub version: String,
}

/// The one pin of `target`'s workflows in `files`, a tag or a commit, and
/// the version it is: the tag's, or the `<project> vX.Y.Z` beside a commit
/// on its line.
pub fn pin_of(files: &[(String, String)], target: &str, project: &str) -> Result<Pin> {
    let uses = format!("{target}/.github/workflows/");
    let named = format!("{project} ");
    let mut pins = BTreeSet::new();
    let mut versions = BTreeSet::new();
    for (_, text) in files {
        let line_end = |s: &str| s.find(['\n', '\r']).unwrap_or(s.len());
        let found = matches(text, &uses, |s| {
            let at = workflow_at(s, uses.len())?;
            let pin = pin_at(&s[at..])?;
            Some(at + pin.len() + line_end(&s[at + pin.len()..]))
        });
        for (start, len) in found {
            let s = &text[start..start + len];
            let at = workflow_at(s, uses.len()).unwrap_or_default();
            let pin = pin_at(&s[at..]).unwrap_or_default();
            let rest = &s[at + pin.len()..];
            let version = match pin.strip_prefix('v') {
                Some(version) => Some(version),
                None => named_version(rest, &named),
            };
            pins.insert(pin.to_owned());
            versions.insert(version.map(str::to_owned));
        }
    }
    match (
        pins.len(),
        versions.into_iter().collect::<Vec<_>>().as_slice(),
    ) {
        (1, [Some(version)]) => Ok(Pin {
            pin: pins.into_iter().next().unwrap_or_default(),
            version: version.clone(),
        }),
        _ => stall(format!("no single pin of {target} with its version")),
    }
}

/// Moves every pin of `target`'s workflows in `files` to `to.pin`, and the
/// `<project> vX.Y.Z` that names the version beside it, in plain or
/// regex-escaped form (a test asserting the pin), to `to.version`. Every
/// file must change: a file that no longer has the pin is a layout this
/// cannot follow, and a PR missing it would be half a bump.
pub fn repin(
    files: &[(String, String)],
    target: &str,
    project: &str,
    to: &Pin,
) -> Result<Vec<(String, String)>> {
    let from = pin_of(files, target, project)?;
    if from.pin == to.pin {
        return stall(format!("{target} is already pinned at {}", from.pin));
    }
    let uses = format!("{target}/.github/workflows/");
    let named = format!("{project} ");
    let escaped = |v: &str| v.replace('.', "\\.");
    // A tag moved to a commit keeps its version beside it, as a commit pin
    // must for the next train to read it.
    let pinned = match to.pin.starts_with('v') {
        true => to.pin.clone(),
        false => format!("{} # {named}v{}", to.pin, to.version),
    };
    let mut out = vec![];
    for (path, text) in files {
        // A commit is unique wherever it appears; a tag is moved only in a pin.
        let moved = match from.pin.starts_with('v') {
            true => replace_after(
                text,
                &uses,
                |s| workflow_at(s, uses.len()),
                &from.pin,
                &pinned,
            ),
            false => text.replace(&from.pin, &to.pin),
        };
        let at = |s: &str| named_at(s, named.len());
        let moved = replace_after(&moved, &named, at, &from.version, &to.version);
        let moved = replace_after(
            &moved,
            &named,
            at,
            &escaped(&from.version),
            &escaped(&to.version),
        );
        if moved == *text {
            return stall(format!("{path} has no pin of {target} to move"));
        }
        out.push((path.clone(), moved));
    }
    Ok(out)
}

/// The release notes for a pin bump's reviewers, fenced as data: they come
/// from merged PR titles, and four backticks would close the fence.
pub fn notes(version: &str, release: &Value, issue: &str) -> String {
    let body: String = release["body"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .take(NOTES_LIMIT)
        .collect();
    let mut fenced = String::with_capacity(body.len());
    let mut ticks = 0;
    for c in body.chars().map(Some).chain([None]) {
        if c == Some('`') {
            ticks += 1;
            continue;
        }
        fenced.push_str(&"`".repeat(ticks.min(3)));
        ticks = 0;
        fenced.extend(c);
    }
    let url = release["html_url"].as_str().unwrap_or_default();
    format!(
        "The train only moves the pin: wire through, in this PR, any interface change of [v{version}]({url}) that callers must pass on. Tracked on {issue}. Its release notes, as data:\n\n````text\n{fenced}\n````"
    )
}

fn parse_time(value: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).with_context(|| format!("invalid time {value:?}"))
}

fn string(value: &Value, pointer: &str) -> Result<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("missing {pointer}"))
}

fn list(value: &Value) -> Result<&Vec<Value>> {
    value.as_array().context("the listing is not a list")
}

/// A tracking issue.
struct Issue {
    number: u64,
    html_url: String,
    title: String,
    labels: Vec<String>,
    created_at: Option<String>,
}

impl Issue {
    fn from_json(value: &Value) -> Result<Self> {
        Ok(Self {
            number: value["number"].as_u64().context("issue number")?,
            html_url: string(value, "/html_url")?,
            title: string(value, "/title")?,
            labels: value["labels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| l["name"].as_str().or(l.as_str()).map(str::to_owned))
                .collect(),
            created_at: value["created_at"].as_str().map(str::to_owned),
        })
    }
}

/// What a PR changes, said in its commit and description.
struct Text {
    title: String,
    why: String,
    body: String,
}

/// The release repository's state since its last release.
struct Pending {
    tag: String,
    published: OffsetDateTime,
    current: String,
    workspace: Option<String>,
    merged: Vec<Merged>,
}

struct Pass<'a> {
    api: &'a dyn Api,
    config: &'a Config,
    apply: bool,
    now: OffsetDateTime,
    out: &'a mut dyn Write,
    requests: u32,
}

/// One pass: prints its decisions to `out`, a line each, and with `apply`
/// takes the step they lead to.
pub fn pass(
    api: &dyn Api,
    config: &Config,
    apply: bool,
    now: OffsetDateTime,
    out: &mut dyn Write,
) -> Result<()> {
    Pass {
        api,
        config,
        apply,
        now,
        out,
        requests: 0,
    }
    .run()
}

impl Pass<'_> {
    fn request(&mut self, method: &str, path: &str, body: &Value) -> Result<Value> {
        // Bounds a pass however the API answers.
        ensure!(self.requests < REQUEST_LIMIT, "request budget exhausted");
        self.requests += 1;
        self.api.request(method, path, body)
    }

    fn get(&mut self, path: &str) -> Result<Value> {
        self.request("GET", path, &Value::Null)
    }

    fn log(&mut self, line: &str) -> Result<()> {
        writeln!(self.out, "{line}").context("write the decisions")
    }

    /// Prints a write; whether to make it.
    fn announce(&mut self, what: &str) -> Result<bool> {
        let would = if self.apply { "" } else { "would: " };
        self.log(&format!("{would}{what}"))?;
        Ok(self.apply)
    }

    fn comment(&mut self, issue: &Issue, body: &str) -> Result<()> {
        if self.announce(&format!("comment on {}: {body}", issue.html_url))? {
            let path = format!(
                "/repos/{}/issues/{}/comments",
                self.config.tracking, issue.number
            );
            self.request("POST", &path, &json!({ "body": body }))?;
        }
        Ok(())
    }

    fn title(&self, version: &str) -> String {
        format!("{} v{version} release train", self.config.project())
    }

    /// The version of a train titled `title`.
    fn titled<'t>(&self, title: &'t str) -> Option<&'t str> {
        let version = title
            .strip_prefix(self.config.project())?
            .strip_prefix(" v")?
            .strip_suffix(" release train")?;
        semver(version).map(|_| version)
    }

    /// The bot's tracking issues in `state`; anyone may open one with this
    /// title, so only the bot's count.
    fn trains(&mut self, state: &str) -> Result<Vec<Value>> {
        let c = self.config;
        let path = format!(
            "/repos/{}/issues?state={state}&labels={}&creator={}&per_page={PAGE_SIZE}",
            c.tracking,
            encode(&c.labels.tracking),
            encode(&c.bot)
        );
        Ok(list(&self.get(&path)?)?
            .iter()
            .filter(|i| i.get("pull_request").is_none_or(Value::is_null))
            .filter(|i| i["user"]["login"].as_str() == Some(c.bot.as_str()))
            .cloned()
            .collect())
    }

    /// The PR on `branch` from `target`'s fork, opened since this train
    /// began: a PR of a dropped train for the same version is not this
    /// train's.
    fn pull(&mut self, target: &Target, branch: &str, since: Option<&str>) -> Result<Option<Pull>> {
        let path = format!(
            "/repos/{}/pulls?state=all&head={}&per_page={PAGE_SIZE}",
            target.repository,
            encode(&format!("{}:{branch}", owner(&target.fork)))
        );
        let since = since.map(parse_time).transpose()?;
        for pr in list(&self.get(&path)?)? {
            let created = pr["created_at"].as_str().filter(|c| !c.is_empty());
            let ours = match (since, created) {
                (Some(since), Some(created)) => parse_time(created)? >= since,
                _ => true,
            };
            if ours {
                let side = |side: &str| {
                    let side = &pr[side];
                    let repo = side["repo"]["full_name"].as_str().unwrap_or("?");
                    format!("{repo}:{}", side["ref"].as_str().unwrap_or("?"))
                };
                let (from, into) = (side("head"), side("base"));
                let default = pr["base"]["repo"]["default_branch"].as_str();
                let stray = (from != format!("{}:{branch}", target.fork)
                    || default.is_none_or(|d| into != format!("{}:{d}", target.repository)))
                .then(|| {
                    format!(
                        "is from {from} into {into}, not from {}:{branch} into {}'s default branch",
                        target.fork, target.repository
                    )
                });
                return Ok(Some(Pull {
                    state: string(pr, "/state")?,
                    merged_at: pr["merged_at"]
                        .as_str()
                        .filter(|m| !m.is_empty())
                        .map(str::to_owned),
                    html_url: string(pr, "/html_url")?,
                    merge_commit_sha: pr["merge_commit_sha"].as_str().map(str::to_owned),
                    stray,
                }));
            }
        }
        Ok(None)
    }

    fn default_branch(&mut self, repo: &str) -> Result<String> {
        string(&self.get(&format!("/repos/{repo}"))?, "/default_branch")
    }

    fn contents(
        &mut self,
        repo: &str,
        at: &str,
        paths: &[String],
    ) -> Result<Vec<(String, String)>> {
        let mut files = vec![];
        for path in paths {
            let encoded: Vec<String> = path.split('/').map(encode).collect();
            let file = self.get(&format!(
                "/repos/{repo}/contents/{}?ref={}",
                encoded.join("/"),
                encode(at)
            ))?;
            if file["encoding"] != "base64" {
                return stall(format!(
                    "{repo}:{path} is too large to edit through the API"
                ));
            }
            files.push((path.clone(), decode(&file["content"])?));
        }
        Ok(files)
    }

    /// The commit `tag` points at, which must be on the release
    /// repository's default branch: whoever can publish a release can tag
    /// a commit nobody reviewed, and the train would carry it into every
    /// caller's pins.
    fn reviewed(&mut self, tag: &str) -> Result<String> {
        let repo = &self.config.release.repository;
        let sha = string(
            &self.get(&format!("/repos/{repo}/commits/{}", encode(tag)))?,
            "/sha",
        )?;
        self.on_default(repo, &sha, &format!("{tag} is at {sha}"))?;
        Ok(sha)
    }

    /// Stalls unless commit `sha`, which `what` names, is on `repo`'s
    /// default branch and so passed its review gate.
    fn on_default(&mut self, repo: &str, sha: &str, what: &str) -> Result<()> {
        let base = self.default_branch(repo)?;
        let compare = self.get(&format!("/repos/{repo}/compare/{}...{sha}", encode(&base)))?;
        let status = compare["status"].as_str().unwrap_or_default();
        if status != "identical" && status != "behind" {
            return stall(format!("{what}, which is not on {repo}'s {base}"));
        }
        Ok(())
    }

    /// The workspace version in `repo`'s Cargo.toml at `at`.
    fn workspace(&mut self, repo: &str, at: &str) -> Result<Option<String>> {
        let toml = self.get(&format!(
            "/repos/{repo}/contents/Cargo.toml?ref={}",
            encode(at)
        ))?;
        Ok(workspace_version(&decode(&toml["content"])?).map(str::to_owned))
    }

    /// The bot's records on `issue` of dispatching the release workflow for
    /// `version`, earliest first: the comment's id, the commit dispatched
    /// and when.
    fn dispatches(
        &mut self,
        issue: &Issue,
        version: &str,
    ) -> Result<Vec<(u64, String, OffsetDateTime)>> {
        let c = self.config;
        let path = format!(
            "/repos/{}/issues/{}/comments?per_page={PAGE_SIZE}",
            c.tracking, issue.number
        );
        let comments = self.get(&path)?;
        let comments = list(&comments)?;
        // A record on a later page would be missed and the release
        // dispatched again.
        ensure!(
            (comments.len() as u64) < PAGE_SIZE,
            "too many comments on {}",
            issue.html_url
        );
        let marker = dispatch_marker(version);
        let mut found = vec![];
        for comment in comments {
            let body = comment["body"].as_str().unwrap_or_default();
            let sha = body.find(&marker).and_then(|at| {
                let rest = &body[at + marker.len()..];
                pin_at(rest).filter(|p| !p.starts_with('v') && rest[p.len()..].starts_with(" -->"))
            });
            if let Some(sha) = sha
                && comment["user"]["login"].as_str() == Some(c.bot.as_str())
            {
                let id = comment["id"].as_u64().context("comment id")?;
                let created = parse_time(&string(comment, "/created_at")?)?;
                found.push((id, sha.to_owned(), created));
            }
        }
        found.sort();
        Ok(found)
    }

    fn pending(&mut self) -> Result<Pending> {
        let repo = &self.config.release.repository;
        let latest = self.get(&format!("/repos/{repo}/releases/latest"))?;
        let tag = string(&latest, "/tag_name")?;
        let published = string(&latest, "/published_at")?;
        let base = self.default_branch(repo)?;
        let workspace = self.workspace(repo, &base)?;
        let q = format!("repo:{repo} is:pr is:merged base:{base} merged:>{published}");
        let found = self.get(&format!(
            "/search/issues?q={}&per_page={PAGE_SIZE}",
            encode(&q)
        ))?;
        let items = list(&found["items"])?;
        // Labels decide the bump and the urgency: all of them or none.
        ensure!(
            found["incomplete_results"] != true
                && found["total_count"].as_u64().unwrap_or_default() <= items.len() as u64,
            "merged pull requests truncated"
        );
        let merged = items
            .iter()
            .map(|i| {
                Ok(Merged {
                    number: i["number"].as_u64().context("pull request number")?,
                    title: string(i, "/title")?,
                    labels: list(&i["labels"])?
                        .iter()
                        .filter_map(|l| l["name"].as_str().map(str::to_owned))
                        .collect(),
                })
            })
            .collect::<Result<_>>()?;
        Ok(Pending {
            current: tag.strip_prefix('v').unwrap_or(&tag).to_owned(),
            tag,
            published: parse_time(&published)?,
            workspace,
            merged,
        })
    }

    /// Opens a PR from the bot's fork, as the bot's other PRs are, so the
    /// poll follows it to a fix or a merge like those. The commit is made on
    /// the fork through the API on the default branch's head; files keep
    /// mode 100644, which every file the train edits has.
    fn open(
        &mut self,
        target: &Target,
        branch: &str,
        text: &Text,
        paths: &[String],
        edit: impl FnOnce(&[(String, String)]) -> Result<Vec<(String, String)>>,
    ) -> Result<Option<String>> {
        let (repo, fork) = (&target.repository, &target.fork);
        let parent = self.get(&format!("/repos/{fork}"))?;
        if parent["parent"]["full_name"].as_str() != Some(repo) {
            return stall(format!("{fork} is not a fork of {repo}"));
        }
        let base = self.default_branch(repo)?;
        let head = self.get(&format!("/repos/{repo}/commits/{}", encode(&base)))?;
        let sha = string(&head, "/sha")?;
        let changed = edit(&self.contents(repo, &sha, paths)?)?;
        // A dropped train's PR still open on the branch: the force push
        // below would change what its reviewers saw.
        let head_ref = format!("{}:{branch}", owner(fork));
        let stale = self.get(&format!(
            "/repos/{repo}/pulls?state=open&head={}&per_page={PAGE_SIZE}",
            encode(&head_ref)
        ))?;
        if let Some(stale) = list(&stale)?.first() {
            return stall(format!(
                "{} from a dropped train is still open on {branch}; close it",
                stale["html_url"].as_str().unwrap_or_default()
            ));
        }
        let names: Vec<&str> = changed.iter().map(|(p, _)| p.as_str()).collect();
        let what = format!(
            "open a PR on {repo} from {fork}:{branch} changing {}: {}",
            names.join(", "),
            text.title
        );
        if !self.announce(&what)? {
            return Ok(None);
        }
        let tree: Vec<Value> = changed
            .iter()
            .map(|(path, content)| {
                json!({"path": path, "mode": "100644", "type": "blob", "content": content})
            })
            .collect();
        let base_tree = string(&head, "/commit/tree/sha")?;
        let tree = self.request(
            "POST",
            &format!("/repos/{fork}/git/trees"),
            &json!({"base_tree": base_tree, "tree": tree}),
        )?;
        let message = format!("{}\n\n{}\n", text.title, text.why);
        let commit = self.request(
            "POST",
            &format!("/repos/{fork}/git/commits"),
            &json!({"message": message, "tree": string(&tree, "/sha")?, "parents": [sha]}),
        )?;
        let commit = string(&commit, "/sha")?;
        // A branch left by a pass that failed before its PR is replaced:
        // without a PR on it, nobody has reviewed it.
        let created = self.request(
            "POST",
            &format!("/repos/{fork}/git/refs"),
            &json!({"ref": format!("refs/heads/{branch}"), "sha": commit}),
        );
        match created {
            Err(e) if status(&e) == Some(422) => {
                self.request(
                    "PATCH",
                    &format!("/repos/{fork}/git/refs/heads/{branch}"),
                    &json!({"sha": commit, "force": true}),
                )?;
            }
            created => {
                created?;
            }
        }
        let pr = self.request(
            "POST",
            &format!("/repos/{repo}/pulls"),
            &json!({"title": text.title, "body": text.body, "head": head_ref, "base": base}),
        )?;
        string(&pr, "/html_url").map(Some)
    }

    /// Opens the tracking issue, or in a preview stands in for it.
    fn open_issue(&mut self, title: String, body: String) -> Result<Issue> {
        if self.announce(&format!("open the tracking issue \"{title}\""))? {
            let path = format!("/repos/{}/issues", self.config.tracking);
            let labels = [&self.config.labels.tracking];
            let issue = self.request(
                "POST",
                &path,
                &json!({"title": title, "body": body, "labels": labels}),
            )?;
            return Issue::from_json(&issue);
        }
        Ok(Issue {
            number: 0,
            html_url: "(new tracking issue)".into(),
            title,
            labels: vec![],
            created_at: Some(self.now.format(&Rfc3339)?),
        })
    }

    /// The version the wrappers pin, when it is behind the latest release
    /// `tag` and so to be followed.
    fn behind(&mut self, tag: &str, current: &str) -> Result<Option<String>> {
        let c = self.config;
        if tag.strip_prefix('v').and_then(semver).is_none() {
            return stall(format!("{tag} is not a vX.Y.Z tag"));
        }
        let wrapped = self.default_branch(&c.wrappers.repository)?;
        let files = self.contents(&c.wrappers.repository, &wrapped, &c.wrappers.files)?;
        let pinned = pin_of(&files, &c.release.repository, c.project())?.version;
        if !older(&pinned, current)? {
            return Ok(None);
        }
        self.reviewed(tag)?;
        let title = self.title(current);
        if self.trains("closed")?.iter().any(|i| i["title"] == *title) {
            self.log(&format!(
                "the wrappers pin v{pinned}, behind {tag}, whose train was closed: not following"
            ))?;
            return Ok(None);
        }
        Ok(Some(pinned))
    }

    fn run(&mut self) -> Result<()> {
        let c = self.config;
        let open = self.trains("open")?;
        let trains: Vec<&Value> = open
            .iter()
            .filter(|i| {
                i["title"]
                    .as_str()
                    .is_some_and(|t| self.titled(t).is_some())
            })
            .collect();
        ensure!(trains.len() <= 1, "more than one open release train");
        let mut issue = trains.first().map(|i| Issue::from_json(i)).transpose()?;
        let stalled = &c.labels.stalled;
        let halt = format!("Label it `{stalled}` to halt the train");
        if issue.is_none() {
            let latest = self.get(&format!("/repos/{}/releases/latest", c.release.repository))?;
            let tag = string(&latest, "/tag_name")?;
            let current = tag.strip_prefix('v').unwrap_or(&tag).to_owned();
            // A release cut by hand, or by a train dropped after it, gets
            // no train of its own: one opened here follows it from the
            // wrappers' pin bump on. A closed train for that version means
            // it was dropped (or finished), so it is not followed again:
            // otherwise closing one would open the next an hour later, and
            // with it another PR. Either way, and when the pin or the tag
            // cannot be read or the tag is off the default branch, the pass
            // goes on to cutting: a release that is due is not held up by
            // one that is not followed. Only the budget, the network and
            // the token end the pass, as they would any other.
            let pinned = match self.behind(&tag, &current) {
                Ok(pinned) => pinned,
                Err(e) if e.is::<Stall>() || e.is::<Http>() => {
                    self.log(&format!("not following {tag}: {e}"))?;
                    None
                }
                Err(e) => return Err(e),
            };
            if let Some(pinned) = pinned {
                self.log(&format!(
                    "the wrappers pin v{pinned}, behind {tag}: following it"
                ))?;
                let body = format!(
                    "The release train's steps following {} {tag}, released outside the train, with the pin bumps (agent-board release-train). The wrappers pin v{pinned}. {halt}; close it to drop it.",
                    c.release.repository
                );
                issue = Some(self.open_issue(self.title(&current), body)?);
            }
        }
        let issue = match issue {
            Some(issue) => issue,
            None => {
                let p = self.pending()?;
                let tag = &p.tag;
                if p.workspace.as_deref() != Some(&p.current) {
                    let at = p.workspace.as_deref().unwrap_or("no workspace version");
                    return self.log(&format!(
                        "main is at {at}, not the last release {tag}: not starting"
                    ));
                }
                let Some(reason) = due(&p.merged, p.published, self.now, c) else {
                    let n = p.merged.len();
                    return self.log(&format!("not due: {n} pull requests merged since {tag}"));
                };
                let breaking = p
                    .merged
                    .iter()
                    .any(|m| m.labels.contains(&c.labels.breaking));
                let version = next_version(&p.current, breaking)?;
                self.log(&format!("due: {reason} since {tag}; next is v{version}"))?;
                let body = format!(
                    "The release train's steps for {} v{version}, one comment each (agent-board release-train). Due: {reason} since {tag}. {halt}; close it to drop this release.",
                    c.release.repository
                );
                self.open_issue(self.title(&version), body)?
            }
        };

        let version = self
            .titled(&issue.title)
            .with_context(|| format!("{} is not a release train", issue.html_url))?
            .to_owned();
        if issue.labels.contains(stalled) {
            return self.log(&format!("{} is labelled {stalled}: halted", issue.html_url));
        }
        let branch = format!("release-train/v{version}");
        let since = issue.created_at.as_deref();
        let mut facts = Facts {
            bump: self.pull(&c.release, &branch, since)?,
            ..Facts::default()
        };
        let path = format!("/repos/{}/releases/tags/v{version}", c.release.repository);
        facts.release = match self.get(&path) {
            Err(e) if status(&e) == Some(404) => None,
            release => Some(release?),
        };
        // Only the train's own dispatch, as recorded on the tracking issue,
        // is this release's run: anyone else's run of the workflow is not.
        let dispatched =
            match facts.bump.as_ref().is_some_and(Pull::merged) && facts.release.is_none() {
                true => self.dispatches(&issue, &version)?.into_iter().next(),
                false => None,
            };
        if let Some((_, sha, at)) = dispatched {
            facts.dispatched = Some(self.now - at);
            let runs = self.get(&format!(
                "/repos/{}/actions/workflows/{}/runs?event=workflow_dispatch&actor={}&head_sha={sha}&created={}&per_page={PAGE_SIZE}",
                c.release.repository,
                c.workflow,
                encode(&c.bot),
                encode(&format!(">={}", at.format(&Rfc3339)?))
            ))?;
            facts.run = match list(&runs["workflow_runs"])?.first() {
                Some(run) => Some(Run {
                    status: string(run, "/status")?,
                    conclusion: run["conclusion"].as_str().map(str::to_owned),
                }),
                None => None,
            };
        }
        facts.wrappers = self.pull(&c.wrappers, &branch, since)?;
        for caller in &c.callers {
            let pr = self.pull(caller, &branch, since)?;
            facts.callers.push((caller.repository.clone(), pr));
        }

        let step = next(&facts);
        self.log(&format!("{} v{version}: {step}", issue.html_url))?;
        match self.take(&step, &issue, &version, &branch, &facts) {
            Err(e) if e.is::<Stall>() => {
                // The label first: a pass that failed between the two would
                // otherwise say so again every hour.
                if self.announce(&format!("label {} {stalled}", issue.html_url))? {
                    let path = format!("/repos/{}/issues/{}/labels", c.tracking, issue.number);
                    self.request("POST", &path, &json!({ "labels": [stalled] }))?;
                }
                self.comment(
                    &issue,
                    &format!("Halted: {e}. Fix it, then remove the `{stalled}` label."),
                )
            }
            taken => taken,
        }
    }

    /// Where the release came from, for the reviewers of the pin bumps: the
    /// last gate on a release the train did not cut. Its tag is checked
    /// again, as a tag can move after the train began.
    /// Returns the tag's commit, which the wrappers pin, and the line.
    fn origin(&mut self, version: &str, facts: &Facts) -> Result<(String, String)> {
        let sha = self.reviewed(&format!("v{version}"))?;
        let origin = match facts.bump.as_ref().is_some_and(Pull::merged) {
            true => String::new(),
            false => format!(
                " v{version} was released outside the train, from {}@{sha}.",
                self.config.release.repository
            ),
        };
        Ok((sha, origin))
    }

    fn take(
        &mut self,
        step: &Step,
        issue: &Issue,
        version: &str,
        branch: &str,
        facts: &Facts,
    ) -> Result<()> {
        let c = self.config;
        let project = c.project();
        let release = facts.release.clone().unwrap_or_default();
        match step {
            Step::Bump => {
                let p = self.pending()?;
                let tag = &p.tag;
                if next_version(&p.current, true)? != version
                    && next_version(&p.current, false)? != version
                {
                    return stall(format!("v{version} does not follow {tag}"));
                }
                let why = format!(
                    "Releases what merged since {tag} so that callers can pin it, through the release train ({}).",
                    issue.html_url
                );
                let mut body = format!("{why}\n\nMerged since {tag}:");
                for m in &p.merged {
                    body.push_str(&format!("\n- #{} {}", m.number, m.title));
                    if !m.labels.is_empty() {
                        body.push_str(&format!(" ({})", m.labels.join(", ")));
                    }
                }
                let text = Text {
                    title: format!("release: Prepare {version}"),
                    why,
                    body,
                };
                let paths = ["Cargo.toml".to_owned(), "Cargo.lock".to_owned()];
                let url = self.open(&c.release, branch, &text, &paths, |files| {
                    let (toml, lock) = bump_cargo(&files[0].1, &files[1].1, &p.current, version)?;
                    Ok(vec![(paths[0].clone(), toml), (paths[1].clone(), lock)])
                })?;
                let url = url.as_deref().unwrap_or("(dry run)");
                self.comment(issue, &format!("Opened the version bump: {url}"))
            }
            Step::Release => {
                let repo = &c.release.repository;
                let bump = facts.bump.as_ref();
                let merge = bump.and_then(|b| b.merge_commit_sha.clone());
                let merge = merge.context("the version bump has no merge commit")?;
                self.on_default(repo, &merge, &format!("the version bump merged as {merge}"))?;
                // The workflow releases the default branch's head: unless
                // that has the train's version, its bump was reverted or
                // superseded while the train waited.
                let base = self.default_branch(repo)?;
                let sha = string(
                    &self.get(&format!("/repos/{repo}/commits/{}", encode(&base)))?,
                    "/sha",
                )?;
                let workspace = self.workspace(repo, &sha)?;
                if workspace.as_deref() != Some(version) {
                    let at = workspace.as_deref().unwrap_or("no workspace version");
                    return stall(format!(
                        "{repo}'s {base} is at {at}, not {version}: was its version bump reverted or superseded?"
                    ));
                }
                // What merged after the bump was opened ships too: a
                // breaking change among it needs a minor.
                let p = self.pending()?;
                let breaking = p
                    .merged
                    .iter()
                    .any(|m| m.labels.contains(&c.labels.breaking));
                if breaking && next_version(&p.current, true)? != version {
                    return stall(format!(
                        "a PR labelled {} merged, so v{version} would be the wrong version",
                        c.labels.breaking
                    ));
                }
                let workflow = &c.workflow;
                let bump = bump.map(|b| b.html_url.as_str()).unwrap_or_default();
                // Recorded before the dispatch, so that a later pass never
                // dispatches again, even before the run is listed. Of
                // passes that overlap, only the one whose record is the
                // earliest dispatches.
                let body = format!(
                    "Running {workflow} for {version} at {sha}, as the bump merged: {bump} {}{sha} -->",
                    dispatch_marker(version)
                );
                if self.announce(&format!("comment on {}: {body}", issue.html_url))? {
                    let path = format!("/repos/{}/issues/{}/comments", c.tracking, issue.number);
                    let posted = self.request("POST", &path, &json!({ "body": body }))?;
                    let id = posted["id"].as_u64().context("comment id")?;
                    let first = self.dispatches(issue, version)?.first().map(|d| d.0);
                    if first != Some(id) {
                        return self.log(&format!(
                            "{workflow} for {version} is another pass's to run: not dispatching"
                        ));
                    }
                }
                if self.announce(&format!("run {workflow} on {repo} for {version}"))? {
                    self.request(
                        "POST",
                        &format!("/repos/{repo}/actions/workflows/{workflow}/dispatches"),
                        &json!({"ref": base, "inputs": {"version": version}}),
                    )?;
                }
                Ok(())
            }
            Step::Wrappers => {
                let (sha, origin) = self.origin(version, facts)?;
                let why = format!(
                    "Moves the wrappers to {project} v{version} so callers get its changes, through the release train.{origin}"
                );
                let text = Text {
                    title: format!("wrappers: Pin {project} v{version}"),
                    body: format!("{why}\n\n{}", notes(version, &release, &issue.html_url)),
                    why,
                };
                // The commit the tag was checked at, not the tag: whoever can
                // move a tag later would change every caller's workflows.
                let to = Pin {
                    pin: sha,
                    version: version.into(),
                };
                let target = &c.release.repository;
                let url = self.open(&c.wrappers, branch, &text, &c.wrappers.files, |files| {
                    repin(files, target, project, &to)
                })?;
                let url = url.as_deref().unwrap_or("(dry run)");
                self.comment(issue, &format!("Opened the wrappers' pin bump: {url}"))
            }
            Step::Callers(repositories) => {
                let (_, origin) = self.origin(version, facts)?;
                let merge = facts
                    .wrappers
                    .as_ref()
                    .and_then(|w| w.merge_commit_sha.clone());
                let merge = merge.context("the wrappers' pin bump has no merge commit")?;
                let what = format!("the wrappers' pin bump merged as {merge}");
                self.on_default(&c.wrappers.repository, &merge, &what)?;
                let to = Pin {
                    pin: merge,
                    version: version.into(),
                };
                let mut urls = vec![];
                for repo in repositories {
                    let Some(caller) = c.callers.iter().find(|t| t.repository == *repo) else {
                        continue;
                    };
                    let why = format!(
                        "Moves review.yml to the wrappers' commit for {project} v{version}, so this repository's reviews run it, through the release train.{origin}"
                    );
                    let text = Text {
                        title: format!("review: Pin {project} v{version}"),
                        body: format!("{why}\n\n{}", notes(version, &release, &issue.html_url)),
                        why,
                    };
                    let target = &c.wrappers.repository;
                    let url = self.open(caller, branch, &text, &caller.files, |files| {
                        repin(files, target, project, &to)
                    })?;
                    urls.push(url.unwrap_or_else(|| format!("(dry run on {repo})")));
                }
                self.comment(
                    issue,
                    &format!("Opened the review.yml pin bumps: {}", urls.join(", ")),
                )
            }
            Step::Stall(reason) => stall(reason.clone()),
            Step::Done => {
                // The close first: a pass that failed between the two would
                // otherwise say "Done" again every hour; a closed train is
                // not followed again, so a failed comment is only lost.
                if self.announce(&format!("close {}", issue.html_url))? {
                    let path = format!("/repos/{}/issues/{}", c.tracking, issue.number);
                    let closed = json!({"state": "closed", "state_reason": "completed"});
                    self.request("PATCH", &path, &closed)?;
                }
                self.comment(
                    issue,
                    &format!(
                        "Done: v{version} is released and pinned by the wrappers and every caller."
                    ),
                )
            }
            Step::Wait(_) => Ok(()),
        }
    }
}

/// What starts the record, in a tracking comment, of the train dispatching
/// the release workflow for `version`; the commit and ` -->` follow.
fn dispatch_marker(version: &str) -> String {
    format!("<!-- release-train dispatched v{version} at ")
}

fn owner(repository: &str) -> &str {
    repository.split('/').next().unwrap_or_default()
}

fn decode(content: &Value) -> Result<String> {
    let bytes = base64_decode(content.as_str().context("file content")?)?;
    String::from_utf8(bytes).context("file content is not UTF-8")
}

#[cfg(test)]
mod tests;
