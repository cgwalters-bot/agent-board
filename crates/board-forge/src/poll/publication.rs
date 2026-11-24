//! Publication CI belongs upstream; repair authority and round accounting stay
//! on the approved fork PR, even when publication created another branch.
//! With `promote.followUpstream` the fork PR is handed over and closed
//! instead, and the bot's own upstream PR takes its rounds
//! ([`Pass::hand_over`]). The fork PR's authenticated receipt alone records
//! the hand-over, for both sides of the pair.
use super::*;
use crate::promote::handoff::{handover_marker, posted, promoted_head};
use std::cell::RefCell;

/// What a fork PR's authenticated publication means for stage 2.
pub(super) enum Promoted {
    /// Upstream CI, which feeds the fork PR's own bounded rounds.
    Checks(Checks),
    /// The current publication the fork PR hands over to, with
    /// [`PollPolicy::follow_promoted`], or has handed over to.
    Upstream(Handover),
}

pub(super) struct Handover {
    /// The upstream PR: its repository and number, `OWNER/REPO#N`, and URL.
    repository: String,
    number: u64,
    at: String,
    url: String,
    /// The fork PR's source issue, which tracks the work.
    source: u64,
    /// Keys the source issue's comment to the fork PR and this publication
    /// ([`handover_marker`]).
    marker: String,
    /// The upstream PR's head and branch as read, which must still be its
    /// own before the receipt records the hand-over.
    head: String,
    branch: String,
    /// The receipt comment's id and the body that records the hand-over;
    /// none when it already does, and only the close is left.
    receipt: Option<(u64, String)>,
}

/// Stage 2's reads for [`crate::promote::handoff`], each spent from the
/// pass's request budget.
struct Reads<'p, 'a, R, G>(RefCell<&'p mut Pass<'a, R, G>>);

impl<R: Rest, G: Transport> Rest for Reads<'_, '_, R, G> {
    fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
        let mut pass = self.0.borrow_mut();
        pass.spend()?;
        pass.rest.get(url, None)
    }

    fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
        bail!("publication CI reads cannot write")
    }
}

impl<R: Rest, G: Transport> Transport for Reads<'_, '_, R, G> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let mut pass = self.0.borrow_mut();
        pass.spend()?;
        pass.graphql.graphql(query, variables)
    }
}

/// Why fork PR `pull`, as [`PULL_QUERY`] read it, is not handed over now: a
/// person's hold or stop, as for any write, or labels past those read, which
/// could be one.
pub(super) fn handover_hold(pull: &Value) -> Option<String> {
    if pull.pointer("/labels/pageInfo/hasNextPage") != Some(&Value::Bool(false)) {
        return Some("more labels than one query reads; not handed over on part of them".into());
    }
    if is_held(pull) {
        return Some(format!("labelled {HOLD_LABEL}"));
    }
    labelled(pull).then(|| format!("labelled {LABEL}"))
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    pub(super) fn publication_checks(
        &mut self,
        repo: &str,
        number: u64,
        pull: &Value,
        comments: &[Value],
    ) -> Result<Option<Promoted>> {
        ensure!(
            !self
                .config
                .policy
                .unavailable_publication_forks
                .iter()
                .any(|r| r.eq_ignore_ascii_case(repo)),
            "publication CI target discovery unavailable; fork repairs held"
        );
        let Some(target) = self
            .config
            .policy
            .publication_forks
            .iter()
            .find(|(at, _)| at.eq_ignore_ascii_case(repo))
            .map(|(_, target)| target.clone())
        else {
            return Ok(None);
        };
        // The fence is only a read hint, never repair authority. Visitors
        // cannot cause reads or select an upstream by quoting a receipt.
        if !comments.iter().any(|c| {
            c["user"]["login"].as_str() == Some(&self.config.bot)
                && (text(c, "/body").contains("```agent-board-promoted")
                    || text(c, "/body").contains(crate::promote::handoff::STATUS))
        }) {
            return Ok(None);
        }
        let fork = self.get(&format!("repos/{repo}/pulls/{number}"))?;
        ensure!(
            fork["number"] == number
                && fork["head"]["sha"] == pull["headRefOid"]
                && fork["head"]["ref"] == pull["headRefName"]
                && fork["state"] == "open",
            "fork changed while reading publication CI"
        );
        let bot = self.config.bot.clone();
        let reads = Reads(RefCell::new(&mut *self));
        let publication =
            crate::promote::handoff::published(&reads, &bot, repo, &target, &fork, comments)?;
        let Some(publication) = publication else {
            return Ok(None);
        };
        let crate::promote::handoff::Publication::Current(publication) = publication else {
            return Ok(Some(Promoted::Checks(Checks {
                failed: vec![],
                pending: vec!["repaired fork head awaits approved upstream publication".into()],
                queue_log: None,
                diagnosis: None,
                every: true,
            })));
        };
        let upstream = &publication.upstream;
        let n = upstream["number"]
            .as_u64()
            .context("missing upstream PR number")?;
        // A recorded hand-over only has the close left, whatever upstream
        // did since; otherwise upstream CI keeps feeding the fork PR's
        // rounds, as without the setting.
        let handed_over = publication.handed_over();
        if handed_over
            || self.config.policy.follow_promoted
                && upstream["state"] == "open"
                && upstream["merged"] == false
                && publication.rewritten()
                && self.follows_upstream(&target.upstream, n, &fork, upstream)?
        {
            let at = format!("{}#{n}", target.upstream);
            return Ok(Some(Promoted::Upstream(Handover {
                url: format!(
                    "https://{}/{}/pull/{n}",
                    crate::github::DEFAULT_HOST,
                    target.upstream
                ),
                marker: handover_marker(repo, number, &at, text(&fork, "/head/sha")),
                repository: target.upstream.clone(),
                number: n,
                at,
                source: crate::promote::handoff::source(repo, &fork)?,
                head: text(upstream, "/head/sha").into(),
                branch: text(upstream, "/head/ref").into(),
                receipt: (!handed_over).then(|| publication.handover()).transpose()?,
            })));
        }
        ensure!(
            upstream["state"] == "open" && upstream["merged"] == false,
            "publication is no longer open; no fork repair started"
        );
        let (owner, name) = target
            .upstream
            .split_once('/')
            .context("invalid upstream repository")?;
        let mut evidence = self.read_pull(owner, name, n)?;
        self.more_checks(owner, name, n, &mut evidence)?;
        ensure!(
            evidence["headRefOid"] == upstream["head"]["sha"] && evidence["state"] == "OPEN",
            "upstream changed while reading publication CI"
        );
        ensure!(
            !truncated(&evidence).contains(&"checks"),
            "incomplete upstream publication checks"
        );
        let base: String = url::form_urlencoded::byte_serialize(target.base.as_bytes()).collect();
        let rules = self.get(&format!("repos/{}/rules/branches/{base}", target.upstream))?;
        let branch = self.get(&format!("repos/{}/branches/{base}", target.upstream))?;
        let mut checks = classify_all_checks(&evidence, &required_checks(&rules, &branch)?);
        for (name, _) in &mut checks.failed {
            *name = format!("upstream {}#{n}: {name}", target.upstream);
        }
        Ok(Some(Promoted::Checks(checks)))
    }

    /// Whether stage 2 takes the rounds of `repo`'s PR `n`, `upstream`, the
    /// current, rewritten publication of fork PR `fork`, once the fork PR
    /// hands it over: on a branch other than the fork PR's, in a repository
    /// the pass follows and has not handed to its maintainers, whose head
    /// the bot pushes to ([`is_harness`]), and which neither
    /// [`Self::planned_upstream`] nor [`Self::round_refusal`] leaves to
    /// someone else. These are the upstream PR's own turn's checks.
    fn follows_upstream(
        &mut self,
        repo: &str,
        n: u64,
        fork: &Value,
        upstream: &Value,
    ) -> Result<bool> {
        let branch = text(upstream, "/head/ref");
        if branch == text(fork, "/head/ref")
            || self.config.policy.handed_over(repo)
            || !self.followed.contains(&repo.to_ascii_lowercase())
        {
            return Ok(false);
        }
        let (owner, name) = repo
            .split_once('/')
            .context("invalid upstream repository")?;
        let pull = self.read_pull(owner, name, n)?;
        ensure!(
            pull["headRefOid"] == upstream["head"]["sha"]
                && pull["headRefName"] == branch
                && pull["state"] == "OPEN",
            "upstream changed while reading its publication"
        );
        if !is_harness(&pull, self.config)
            || self.config.policy.fix_plans
                && !self.config.ours(repo)
                && self.planned_upstream(repo, n, &pull).is_some()
        {
            return Ok(false);
        }
        Ok(self.round_refusal(repo, n, &pull).is_none())
    }

    /// Why the bot's PR `repo#number`, `pull`, gets no round or branch update
    /// under `promote.followUpstream`: a fork PR's authenticated receipt
    /// names it as its publication without recording the hand-over
    /// ([`Self::hand_over`]), or, on a promotion branch or from a
    /// publication fork, none names it. Its push would move the publication
    /// off its receipt's head, after which neither the hand-over nor the
    /// pass's handoff check accepts it, or race the fork PR's own rounds. A
    /// read that fails counts the same. Only bot PRs in a repository that
    /// fork PRs publish to are read for this.
    pub(super) fn unhanded_publication(
        &mut self,
        repo: &str,
        number: u64,
        pull: &Value,
    ) -> Option<String> {
        let policy = &self.config.policy;
        if !policy.follow_promoted
            || !text(pull, "/author/login").eq_ignore_ascii_case(&self.config.bot)
        {
            return None;
        }
        let forks: BTreeMap<String, crate::pass::promote::Fork> = (policy.publication_forks.iter())
            .filter(|(_, target)| target.upstream.eq_ignore_ascii_case(repo))
            .map(|(fork, target)| (fork.clone(), target.clone()))
            .collect();
        let head_repo = text(pull, "/headRepository/nameWithOwner");
        let promoted = promoted_head(text(pull, "/headRefName")).is_some()
            || forks
                .keys()
                .any(|fork| fork.eq_ignore_ascii_case(head_repo));
        if let Some(fork) = (policy.unavailable_publication_forks.first()).filter(|_| promoted) {
            return Some(format!(
                "it may publish a fork PR, and {fork}'s publication target could not be read"
            ));
        }
        if forks.is_empty() && !promoted {
            return None;
        }
        match self.publisher(repo, number, &forks) {
            Ok(Some((_, true))) => None,
            Ok(Some((fork, false))) => Some(format!(
                "it publishes {fork}, which has not handed it over yet"
            )),
            Ok(None) => {
                promoted.then(|| "it may publish a fork PR, and no receipt names it".into())
            }
            Err(error) => Some(format!(
                "the fork PR it may publish could not be read ({error:#})"
            )),
        }
    }

    /// The fork PR, `FORK#N`, whose authenticated receipt names `repo#number`
    /// as its publication, and whether it records the hand-over; none when
    /// none does. The receipt links its upstream PR, so the candidates are
    /// the pull requests of `forks` among that PR's cross-references, as the
    /// pass's handoff check finds fork PRs through their source issues'.
    fn publisher(
        &mut self,
        repo: &str,
        number: u64,
        forks: &BTreeMap<String, crate::pass::promote::Fork>,
    ) -> Result<Option<(String, bool)>> {
        let timeline = self.paged(
            &format!("repos/{repo}/issues/{number}/timeline?per_page=100"),
            true,
        )?;
        let mut candidates = BTreeSet::new();
        for event in &timeline {
            let issue = &event["source"]["issue"];
            if event["event"] != "cross-referenced" || !issue["pull_request"].is_object() {
                continue;
            }
            let url = issue["html_url"]
                .as_str()
                .context("missing cross-reference URL")?;
            let at = crate::parse_github_item(url, None, crate::github::DEFAULT_HOST)?;
            if let Some(fork) =
                (forks.keys()).find(|fork| fork.eq_ignore_ascii_case(&at.repository))
            {
                candidates.insert((fork, at.number));
            }
        }
        let bot = self.config.bot.clone();
        let mut found = vec![];
        for (fork, n) in candidates {
            let target = &forks[fork];
            let pull = self.get(&format!("repos/{fork}/pulls/{n}"))?;
            if !pull["user"]["login"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&bot))
            {
                continue;
            }
            let comments = self.paged(
                &format!("repos/{fork}/issues/{n}/comments?per_page=100"),
                true,
            )?;
            let reads = Reads(RefCell::new(&mut *self));
            let names = crate::promote::handoff::publishes(
                &reads, &bot, fork, target, &pull, &comments, repo, number,
            )?;
            if let Some(handed_over) = names {
                found.push((format!("{fork}#{n}"), handed_over));
            }
        }
        ensure!(found.len() <= 1, "more than one fork PR's receipt names it");
        Ok(found.pop())
    }

    /// Why the hand-over of fork PR `repo#number`, published as `to`, stops
    /// before its next write: read again, the fork PR is closed, off `head`,
    /// or held ([`handover_hold`]); or, until its receipt records the
    /// hand-over, the upstream PR is closed or off the head and branch that
    /// [`Self::publication_checks`] read.
    fn handover_recheck(
        &mut self,
        repo: &str,
        number: u64,
        head: &str,
        to: &Handover,
    ) -> Result<Option<String>> {
        let (owner, name) = repo.split_once('/').context("invalid fork repository")?;
        let fresh = self.read_pull(owner, name, number)?;
        if fresh["state"] != "OPEN" || fresh["headRefOid"] != head {
            return Ok(Some(
                "the pull request closed or moved before its hand-over".into(),
            ));
        }
        if let Some(why) = handover_hold(&fresh) {
            return Ok(Some(why));
        }
        if to.receipt.is_none() {
            return Ok(None);
        }
        let (owner, name) =
            (to.repository.split_once('/')).context("invalid upstream repository")?;
        let upstream = self.read_pull(owner, name, to.number)?;
        Ok((upstream["state"] != "OPEN"
            || upstream["headRefOid"] != to.head.as_str()
            || upstream["headRefName"] != to.branch.as_str())
        .then(|| format!("{} closed or moved before the hand-over", to.at)))
    }

    /// Hand fork PR `pull` over to its publication `to` and close it, after
    /// saying on its source issue that work moves upstream: one write step
    /// of at most three writes. The receipt's edit is the hand-over, after
    /// which the upstream PR's rounds may move its head; a retry posts no
    /// second comment and, once the receipt says so, only closes the fork
    /// PR. The fork PR was just read open at the approved head
    /// ([`Self::publication_checks`]).
    pub(super) fn hand_over(
        &mut self,
        repo: &str,
        pull: &Value,
        to: &Handover,
    ) -> Result<(String, bool, Option<Wait>, Option<Stepped>)> {
        let number = pull["number"]
            .as_u64()
            .context("pull request has no number")?;
        let head = text(pull, "/headRefOid");
        let reason = format!("promoted to {}; work continues upstream", to.at);
        let capped = self.caps.and_then(|c| c.check(Kind::Write));
        let before = self.writes;
        let written = (|| -> Result<Option<String>> {
            if self.apply.is_none() || capped.is_some() {
                return Ok(None);
            }
            let bot = self.config.bot.clone();
            let marker = &to.marker;
            let rest = self.rest;
            // The fork PR was read before the publication and the source
            // issue's comments, and the upstream PR before its checks: a
            // hold, push or close since stops the step, checked again before
            // the receipt records the hand-over.
            if let Some(why) = self.handover_recheck(repo, number, head, to)? {
                return Ok(Some(why));
            }
            if let Some((receipt, body)) = &to.receipt {
                let tracking = format!("repos/{repo}/issues/{}/comments", to.source);
                if !posted(
                    &bot,
                    marker,
                    &self.paged(&format!("{tracking}?per_page=100"), true)?,
                ) {
                    let body = format!(
                        "{marker}\nWork on this moves upstream to {}: the poll hands {repo}#{number} over to it, then follows and fixes it there.",
                        to.url
                    );
                    self.send(rest, "POST", &tracking, json!({"body": body}), 0)?;
                }
                if let Some(why) = self.handover_recheck(repo, number, head, to)? {
                    return Ok(Some(why));
                }
                self.send(
                    rest,
                    "PATCH",
                    &format!("repos/{repo}/issues/comments/{receipt}"),
                    json!({"body": body}),
                    0,
                )?;
            }
            self.send(
                rest,
                "PATCH",
                &format!("repos/{repo}/pulls/{number}"),
                json!({"state": "closed"}),
                0,
            )?;
            Ok(None)
        })();
        let ran = capped.is_none() && (self.apply.is_none() || self.writes > before);
        if ran && let Some(caps) = self.caps {
            caps.count(Kind::Write);
        }
        // Until its receipt records the hand-over, the fork PR keeps its
        // publication from the upstream PR's rounds
        // ([`Self::unhanded_publication`]); a retry finishes the step.
        if let Some(why) = written? {
            return Ok((format!("{repo}#{number} skip: {why}"), false, None, None));
        }
        let done = match (&capped, &self.apply) {
            (Some(held), _) => format!(" -> {held}"),
            (None, Some(_)) => " -> closed".into(),
            (None, None) => String::new(),
        };
        Ok((
            format!("{repo}#{number} {} close: {reason}{done}", short(head)),
            false,
            None,
            Some(Stepped {
                repository: repo.into(),
                number,
                head: head.into(),
                action: "close",
                reason,
                ran,
                moved_at: moved_at(pull),
                waiting: WaitReason::Followed { since: None },
            }),
        ))
    }
}
