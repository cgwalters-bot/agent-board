//! Publication CI belongs upstream; repair authority and round accounting stay
//! on the approved fork PR, even when publication created another branch.
use super::*;
use std::cell::RefCell;

struct Read<R, G> {
    rest: R,
    graphql: G,
}

impl<R, G> Rest for Read<R, G>
where
    R: Fn(&str) -> Result<RestResponse>,
{
    fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
        (self.rest)(url)
    }

    fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
        bail!("publication CI reads cannot write")
    }
}

impl<R, G> Transport for Read<R, G>
where
    G: Fn(&str, Value) -> Result<Value>,
{
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        (self.graphql)(query, variables)
    }
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    pub(super) fn publication_checks(
        &mut self,
        repo: &str,
        number: u64,
        pull: &Value,
        comments: &[Value],
    ) -> Result<Option<Checks>> {
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
        let upstream = {
            let pass = RefCell::new(&mut *self);
            let client = Read {
                rest: |url: &str| {
                    let mut pass = pass.borrow_mut();
                    pass.spend()?;
                    pass.rest.get(url, None)
                },
                graphql: |query: &str, variables: Value| {
                    let mut pass = pass.borrow_mut();
                    pass.spend()?;
                    pass.graphql.graphql(query, variables)
                },
            };
            crate::promote::handoff::published(&client, &bot, repo, &target, &fork, comments)?
        };
        let Some(upstream) = upstream else {
            return Ok(None);
        };
        let crate::promote::handoff::Publication::Current(upstream) = upstream else {
            return Ok(Some(Checks {
                failed: vec![],
                pending: vec!["repaired fork head awaits approved upstream publication".into()],
                queue_log: None,
                diagnosis: None,
                every: true,
            }));
        };
        ensure!(
            upstream["state"] == "open" && upstream["merged"] == false,
            "publication is no longer open; no fork repair started"
        );
        let n = upstream["number"]
            .as_u64()
            .context("missing upstream PR number")?;
        let (owner, name) = target
            .upstream
            .split_once('/')
            .context("invalid upstream repository")?;
        let mut evidence = self.read_pull(owner, name, n)?;
        self.more_checks(owner, name, n, &mut evidence, false)?;
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
        Ok(Some(checks))
    }
}
