//! Contents SHA validation plus Forgejo v17's non-forced single-parent push
//! is the ownership boundary, not a post-write read-back. See docs/forges.md.
//! Slots must be provisioned on a private dedicated branch; no deletes, force,
//! new_branch, last_commit_id or alternate write protocol is used here.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

const SLOTS: u64 = crate::pass::WIP_LIMIT;
const LEASE_SECONDS: i64 = 24 * 60 * 60;
/// Base64 content `slot` reads back; a write beyond it could never be read.
const CONTENT_BYTES: usize = 16_384;
/// A serialized lease, small enough that its escaped free marker still fits.
const LEASE_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub run: u64,
    /// A globally unique durable plan-claim identity; never reuse a claim.
    pub claim: String,
    pub item: String,
    pub pool: String,
    pub taken_at: String,
    pub expires_at: String,
}

/// The immutable observation used as the expected ownership value.
/// Callers cannot manufacture a SHA or replace the observed owner.
pub struct Slot {
    branch: String,
    slot: u64,
    sha: String,
    body: Value,
    lease: Option<Lease>,
}

impl Slot {
    pub fn lease(&self) -> Option<&Lease> {
        self.lease.as_ref()
    }
}

impl<T: Transport> Forgejo<'_, T> {
    fn slot_url(&self, branch: &str, slot: u64) -> Result<url::Url> {
        Target {
            git_ref: branch.into(),
            ..self.target.clone()
        }
        .validate()?;
        ensure!(slot < SLOTS, "lease slot exceeds cap");
        let mut url = self.instance.api_root()?.join(&format!(
            "repos/{}/contents/leases/{slot}.json",
            self.target.caller
        ))?;
        url.query_pairs_mut().append_pair("ref", branch);
        Ok(url)
    }

    /// Missing/unreadable slots refuse admission rather than guessing free.
    /// Provision `{ "free": "initial" }` once in each slot before use.
    pub fn slot(&self, branch: &str, slot: u64) -> Result<Slot> {
        let url = self.slot_url(branch, slot)?;
        self.charge()?;
        let response = self.transport.get(&url)?.body;
        ensure!(response["type"] == "file", "lease slot is not a file");
        let sha = response["sha"]
            .as_str()
            .filter(|s| s.len() == 40 || s.len() == 64)
            .filter(|s| s.bytes().all(|c| c.is_ascii_hexdigit()))
            .context("invalid lease blob SHA")?
            .to_owned();
        let content = response["content"]
            .as_str()
            .context("missing lease content")?;
        ensure!(
            content.len() <= CONTENT_BYTES && response["encoding"] == "base64",
            "unsupported lease encoding or size"
        );
        let bytes = STANDARD.decode(content.replace(['\r', '\n'], ""))?;
        let body: Value = serde_json::from_slice(&bytes)?;
        let lease = if body
            .as_object()
            .is_some_and(|o| o.len() == 1 && o.get("free").is_some_and(Value::is_string))
        {
            None
        } else {
            Some(serde_json::from_value(body.clone()).context("invalid lease; held")?)
        };
        Ok(Slot {
            branch: branch.into(),
            slot,
            sha,
            body,
            lease,
        })
    }

    /// A 409 loses the race. A lost reply/500 is an error holding the slot,
    /// never permission to try another slot. Read-back can diagnose an unknown
    /// outcome but is not CAS and cannot turn an error into admission.
    fn write_slot(&self, observed: &Slot, body: Value) -> Result<bool> {
        self.require_private()?;
        self.put_slot(observed, body)
    }

    fn require_private(&self) -> Result<()> {
        self.charge()?;
        let repo = self
            .transport
            .get(
                &self
                    .instance
                    .api_root()?
                    .join(&format!("repos/{}", self.target.caller))?,
            )?
            .body;
        ensure!(
            repo["private"] == true,
            "leases require a private caller repository"
        );
        Ok(())
    }

    fn put_slot(&self, observed: &Slot, body: Value) -> Result<bool> {
        let mut url = self.slot_url(&observed.branch, observed.slot)?;
        url.set_query(None);
        let content = STANDARD.encode(serde_json::to_vec(&body)?);
        ensure!(
            content.len() <= CONTENT_BYTES,
            "lease slot content too large"
        );
        let request = json!({"branch": observed.branch, "sha": observed.sha, "content": content, "message": "Update admission slot"});
        self.charge()?;
        match self.transport.send("PUT", &url, &request)? {
            (200, _) => Ok(true),
            (409, _) => Ok(false),
            (status, _) => bail!("lease write unconfirmed: HTTP {status}; held, do not retry"),
        }
    }

    /// Expected owner and expiry are checked before a single conditional write.
    /// `final_check` preserves caller WIP/pool/budget caps immediately before
    /// reserving. An expired slot is reaped separately, never overwritten here.
    pub fn admit(
        &self,
        observed: &Slot,
        lease: &Lease,
        now: OffsetDateTime,
        mut final_check: impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        ensure!(
            observed.lease.is_none(),
            "slot occupied; reap separately if expired"
        );
        let taken = OffsetDateTime::parse(&lease.taken_at, &Rfc3339)?;
        let expires = OffsetDateTime::parse(&lease.expires_at, &Rfc3339)?;
        ensure!(
            taken == now && expires - taken == time::Duration::seconds(LEASE_SECONDS),
            "invalid lease lifetime"
        );
        ensure!(
            !lease.claim.is_empty() && lease.claim.len() <= 256 && !lease.pool.is_empty(),
            "lease needs unique claim and pool"
        );
        let (repo, item) = lease.item.rsplit_once('#').context("invalid lease item")?;
        crate::poll::validate_repository(repo)?;
        ensure!(
            item.parse::<u64>().is_ok_and(|n| n > 0),
            "invalid lease item number"
        );
        ensure!(
            serde_json::to_vec(lease)?.len() <= LEASE_BYTES,
            "lease too large"
        );
        // Every read precedes the final check, so nothing but the PUT follows it.
        self.require_private()?;
        final_check()?;
        self.put_slot(observed, serde_json::to_value(lease)?)
    }

    /// Bind a send-boundary reservation without changing its claim or expiry.
    pub fn bind(
        &self,
        observed: &Slot,
        claim: &str,
        run: u64,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let mut lease = observed.lease.clone().context("slot is free")?;
        ensure!(
            lease.claim == claim && lease.run == 0 && run > 0,
            "reservation ownership mismatch"
        );
        ensure!(
            OffsetDateTime::parse(&lease.expires_at, &Rfc3339)? > now,
            "reservation expired; held"
        );
        lease.run = run;
        self.write_slot(observed, serde_json::to_value(lease)?)
    }

    /// Release only this observed owner, or reap an observed expired lease.
    /// The old SHA prevents a late release/reap from freeing a replacement.
    pub fn release(
        &self,
        observed: &Slot,
        claim: &str,
        run: u64,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let lease = observed.lease.as_ref().context("slot is free")?;
        ensure!(
            lease.claim == claim && lease.run == run,
            "lease ownership mismatch"
        );
        let expires = OffsetDateTime::parse(&lease.expires_at, &Rfc3339)?;
        if expires > now {
            ensure!(
                run > 0 && !active(&self.status(run)?)?,
                "live or unreadable run keeps its lease"
            );
        }
        // Preserve all old owner data in the free marker: no ABA after a
        // replacement, even when run ids are reused by reruns.
        self.write_slot(
            observed,
            json!({"free": serde_json::to_string(&observed.body)?}),
        )
    }
}
