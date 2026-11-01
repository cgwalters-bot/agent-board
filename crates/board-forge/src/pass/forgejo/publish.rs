//! The usage and hold snapshots on Forgejo
//! (docs/forges.md#comments-recorders-and-editors): each a new bot comment
//! on an issue of a private repository of this instance, never edited, and
//! posted only where what it says changed. The newest of the bot's
//! unedited comments with the snapshot's marker is the one published: anyone
//! with write access may edit a comment, so an edited one is passed over.
use super::*;
use crate::forgejo::unedited;

/// The usage snapshot's figures that move with the clock alone, which do
/// not make it a change ([`content`]).
const CLOCKED: [&str; 3] = ["allowed_percent", "limit_percent", "ahead"];

/// Refuses `usage.repository` and `holds.repository` where this instance
/// does not say they are private: the snapshots say what the subscriptions
/// are used for and what the board's issues wait on.
pub(super) fn private(
    client: &impl ForgejoTransport,
    board: &ForgejoConfig,
    config: &Config,
) -> Result<()> {
    let usage = (config.usage.iter())
        .filter(|u| !u.repository.is_empty())
        .map(|u| ("usage", u.repository.as_str()));
    let holds = (config.holds.iter()).map(|h| ("holds", h.repository.as_str()));
    for (what, repository) in usage.chain(holds) {
        let url = board.api_root()?.join(&format!("repos/{repository}"))?;
        let read = client.get(&url)?.body;
        let named = text(&read, "/full_name");
        ensure!(
            named.eq_ignore_ascii_case(repository),
            "{what}.repository {repository} read as {named:?}"
        );
        ensure!(
            read["private"] == true,
            "{what}.repository {repository} is not private on this instance; its snapshot is not published there"
        );
    }
    Ok(())
}

/// The bot's newest unedited comment on issue `on` (repository and number)
/// whose body starts with `marker`, from the issue's whole thread.
fn newest(
    client: &impl ForgejoTransport,
    board: &ForgejoConfig,
    bot: u64,
    (repository, number): (&str, u64),
    marker: &str,
) -> Result<Option<Value>> {
    let issue = client
        .get(&api(board, repository, &format!("issues/{number}"), &[])?)?
        .body;
    ensure!(
        issue["number"] == number
            && issue["pull_request"].is_null()
            && text(&issue, "/repository/full_name").eq_ignore_ascii_case(repository),
        "{repository}#{number} is not an issue of {repository}"
    );
    // This endpoint returns the whole thread, not offset pages.
    let path = format!("issues/{number}/comments");
    let comments = client.get(&api(board, repository, &path, &[])?)?.body;
    let comments = comments.as_array().context("comments are not a list")?;
    ensure!(
        issue["comments"].as_u64() == Some(comments.len() as u64),
        "the comments of {repository}#{number} could not all be read"
    );
    Ok((comments.iter())
        .filter(|c| c["user"]["id"] == bot && unedited(c))
        .filter(|c| text(c, "/body").starts_with(marker))
        .max_by_key(|c| c["id"].as_u64())
        .cloned())
}

/// Appends `body` as the bot's comment on issue `on` and reads it back,
/// unedited and as written: its address.
fn post(
    client: &impl ForgejoTransport,
    board: &ForgejoConfig,
    bot: u64,
    (repository, number): (&str, u64),
    body: &str,
) -> Result<String> {
    let url = api(board, repository, &format!("issues/{number}/comments"), &[])?;
    let answer = client.write("POST", &url, &json!({"body": body}))?;
    let id = (answer["id"].as_u64())
        .filter(|id| *id > 0)
        .context("the comment's write is ambiguous: no id")?;
    let path = format!("issues/comments/{id}");
    let after = client.get(&api(board, repository, &path, &[])?)?.body;
    ensure!(
        after["id"] == id
            && after["body"].as_str() == Some(body)
            && after["user"]["id"] == bot
            && unedited(&after),
        "comment {id} did not read back as written"
    );
    Ok(format!(
        "{}/{repository}/issues/{number}#issuecomment-{id}",
        instance(board)
    ))
}

/// Publishes the `name` snapshot on issue `on` where `body`, given the
/// newest published one's body, says what to post: none where it is
/// unchanged. Its line, and whether it failed.
fn publish(
    client: &impl ForgejoTransport,
    board: &ForgejoConfig,
    (bot, caps, mode): (u64, &Caps, Mode<'_>),
    (name, marker, on): (&str, &str, (&str, u64)),
    body: impl FnOnce(Option<&str>) -> Result<Option<String>>,
) -> (String, bool) {
    let said = format!("{name}: publish to {}#{}", on.0, on.1);
    let decided = newest(client, board, bot, on, marker)
        .and_then(|published| body(published.as_ref().map(|c| text(c, "/body"))));
    let body = match decided {
        Ok(Some(body)) => body,
        Ok(None) => return (format!("{name}: unchanged"), false),
        Err(error) => return (format!("{name} error: {said}: {error:#}"), true),
    };
    if let Some(held) = caps.check(Kind::Write) {
        return (format!("{held}: {said}"), false);
    }
    if matches!(mode, Mode::Preview) {
        caps.count(Kind::Write);
        return (format!("{said}; a preview does not write it"), false);
    }
    match post(client, board, bot, on, &body) {
        Ok(url) => (format!("{name}: published {url}"), false),
        Err(error) => (format!("{name} error: {said}: {error:#}"), true),
    }
}

/// What the usage snapshot says apart from the clock: without its times
/// (`…_at`) and the [`CLOCKED`] figures, so that a reading only the clock
/// moved is not posted again.
fn content(snapshot: &Value) -> Value {
    match snapshot {
        Value::Object(map) => (map.iter())
            .filter(|(k, _)| !k.ends_with("_at") && !CLOCKED.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), content(v)))
            .collect::<serde_json::Map<_, _>>()
            .into(),
        Value::Array(items) => items.iter().map(content).collect(),
        other => other.clone(),
    }
}

/// Stage 1's `snapshot`, where `usage.repository` is set.
pub(super) fn usage(
    client: &impl ForgejoTransport,
    board: &ForgejoConfig,
    config: &Config,
    (ids, caps, mode): (Ids, &Caps, Mode<'_>),
    snapshot: &Value,
    report: &mut Report,
) {
    let Some(at) = (config.usage.as_ref()).filter(|u| !u.repository.is_empty()) else {
        return;
    };
    let on = (at.repository.as_str(), at.issue);
    let (line, failed) = publish(
        client,
        board,
        (ids.bot, caps, mode),
        ("usage", USAGE_MARKER, on),
        |published| {
            let previous = published.and_then(usage_snapshot);
            match previous.is_some_and(|p| content(&p) == content(snapshot)) {
                true => Ok(None),
                false => usage::render(snapshot).map(Some),
            }
        },
    );
    report.failures += usize::from(failed);
    report.lines.push(line);
}

impl<C: ForgejoTransport> Schedule<'_, C> {
    /// Stages 3 and 4's holds, where `holds` is set, as the GitHub pass
    /// builds them ([`holds::snapshot`], [`holds::carry`], [`holds::alarm`],
    /// [`holds::fit`]), each alarm a `stall …` line.
    pub(super) fn publish_holds(&mut self) {
        let Some(at) = &self.config.holds else {
            return;
        };
        let on = ItemRef {
            host: instance(self.board),
            repository: at.repository.clone(),
            number: at.issue,
        };
        let mut alarms = Vec::new();
        let (line, failed) = publish(
            self.client,
            self.board,
            (self.ids.bot, self.caps, self.mode),
            ("holds", holds::HOLDS_MARKER, (&at.repository, at.issue)),
            |published| {
                let previous = published.and_then(holds::published);
                let snapshot = self.hold_snapshot(previous.as_ref(), &on, &mut alarms);
                let (snapshot, body) = holds::fit(snapshot)?;
                Ok(holds::changed(published, &snapshot)?.then_some(body))
            },
        );
        self.report.lines.extend(alarms);
        self.report.failures += usize::from(failed);
        self.line(line);
    }

    /// The hold snapshot at this pass's clock, with what carries over from
    /// `previous`, the published one's JSON, and its stall alarms, each
    /// added to `alarms` as a line; with `holds.needsYou`, what waits on the
    /// operator from the epics, the items and the pass's own alarms on the
    /// holds issue `on`. The reviews, proposals and digest the GitHub pass
    /// lists are off: their sources are GitHub's.
    fn hold_snapshot(
        &self,
        previous: Option<&Value>,
        on: &ItemRef,
        alarms: &mut Vec<String>,
    ) -> holds::Snapshot {
        let stamp = self.now.format(&Rfc3339).unwrap_or_default();
        let entries = &self.report.holds;
        let mut snapshot = holds::snapshot(entries, stamp, self.report.tally.clone());
        let considered = snapshot.pass.candidates.is_some();
        let progressed = !matches!(self.mode, Mode::Preview) && self.report.tally.1.started > 0;
        holds::carry(&mut snapshot, previous, (progressed, considered));
        let top = DEFAULT_TOP.map(str::to_owned);
        let default = holds::Stall::default();
        let watch = holds::Watch {
            stall: self.config.stall.as_ref().unwrap_or(&default),
            top: &top,
            on,
            log: None,
            free: holds::free(self.ranked.as_deref(), entries),
            blocked: &BTreeSet::new(),
        };
        alarms.extend(holds::alarm(&mut snapshot, previous, &watch));
        if self.config.holds.as_ref().is_some_and(|h| h.needs_you) {
            let options = PRIORITIES.map(str::to_owned);
            let reading = holds::Reading {
                epics: self.config.epics.is_some(),
                considered: self.unconsidered == 0,
                options: &options,
                on,
                reviews: holds::Collected::off(),
                proposals: holds::Collected::off(),
                digest: holds::Collected::off(),
                summaries: (holds::Read::Off, BTreeMap::new()),
            };
            snapshot.needs_you = Some(holds::needs_you(&snapshot, previous, &reading));
        }
        snapshot
    }
}
