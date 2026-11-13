//! Infrastructure routing is not permission to change posture. Only typed
//! controller outcomes authorize it; diagnostic text is never classification.

pub const SUPERVISOR_LABEL: &str = "needs-supervisor";
pub const SUPERVISOR_PREFIX: &str = "Held for the supervisor: ";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    StaleBase,
    EmptyFix,
}

impl Cause {
    pub fn name(self) -> &'static str {
        match self {
            Self::StaleBase => "stale-base",
            Self::EmptyFix => "empty-fix",
        }
    }

    /// Read only the fixed cause tag written by the controller, not check
    /// names, checkpoint prose, logs or other repository-controlled text.
    pub fn from_tag(reason: &str) -> Option<Self> {
        [Self::StaleBase, Self::EmptyFix]
            .into_iter()
            .find(|cause| reason.starts_with(&format!("[{}] ", cause.name())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_stop_classification() {
        for (reason, expected) in [
            (
                "[stale-base] rebase rounds exhausted",
                Some(Cause::StaleBase),
            ),
            (
                "[empty-fix] fix round pushed nothing",
                Some(Cause::EmptyFix),
            ),
            ("operator review requested", None),
            ("upstream posting", None),
            ("failed check [stale-base] missing runner", None),
            ("checkpoint says stale base", None),
            ("unknown refusal", None),
        ] {
            assert_eq!(Cause::from_tag(reason), expected, "{reason}");
        }
    }
}
