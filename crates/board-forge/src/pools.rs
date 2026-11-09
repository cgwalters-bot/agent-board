//! Deployment-owned pools.json resolution, without network or inference.

use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value};

const MODEL_LIMIT: usize = 200;

/// Shared pools-file shape used by resolution and tier parsing. Extra pool
/// settings belong to their consumers and are deliberately preserved.
pub(crate) fn object(value: &Value) -> Result<&Map<String, Value>> {
    value.as_object().context("pools file is not an object")
}

fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= MODEL_LIMIT
        && model.as_bytes()[0].is_ascii_alphanumeric()
        && model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:+-".contains(&b))
}

/// A validated deployment file. Heavy models are checked only when requested,
/// so an unused variant cannot change a normal dispatch's behavior.
pub struct Pools<'a>(&'a Map<String, Value>);

impl<'a> Pools<'a> {
    pub fn from_json(value: &'a Value) -> Result<Self> {
        let pools = object(value)?;
        for (name, entry) in pools {
            ensure!(
                entry["model"].as_str().is_some_and(valid_model),
                "pool {name} has no configured valid model"
            );
            ensure!(
                entry["budget"].as_str().is_some_and(|budget| {
                    budget.starts_with(|c: char| c.is_ascii_digit() && c != '0')
                        && budget.bytes().all(|b| b.is_ascii_digit())
                }),
                "pool {name} budget must be a positive integer string"
            );
        }
        Ok(Self(pools))
    }

    /// Resolve first, then return the entire output so errors emit no partial
    /// workflow outputs. Inherited heavy selection changes only the model;
    /// the wrapper retains its original agent and budget fallbacks.
    pub fn resolve(
        &self,
        pool: &str,
        effort: &str,
        kind: &str,
        fallback_agent: &str,
        fallback_model: &str,
    ) -> Result<String> {
        ensure!(
            effort.is_empty() || effort == "heavy",
            "effort must be empty or heavy"
        );
        ensure!(
            effort.is_empty() || kind != "review",
            "heavy effort is not supported for reviews"
        );
        let explicit = !pool.is_empty();
        ensure!(
            !explicit || self.0.contains_key(pool),
            "unknown pool '{pool}'"
        );
        let mut selected = self.0.get_key_value(pool).filter(|_| explicit);
        if !explicit && effort == "heavy" {
            let mut matches = self.0.iter().filter(|(_, entry)| {
                entry["agent"].as_str() == Some(fallback_agent)
                    && entry["model"].as_str() == Some(fallback_model)
            });
            selected = matches.next();
            ensure!(
                !fallback_model.is_empty() && selected.is_some() && matches.next().is_none(),
                "heavy requires an explicit pool or a unique fallback agent/model pool"
            );
        }
        let actual_agent = selected.map_or(Ok(fallback_agent), |(name, entry)| {
            entry["agent"]
                .as_str()
                .with_context(|| format!("pool {name} agent is not a string"))
        })?;
        // A newline in a trusted agent setting would inject workflow outputs.
        ensure!(
            !actual_agent.contains(['\r', '\n']),
            "pool agent must be one line"
        );
        let mut model = selected
            .and_then(|(_, entry)| entry["model"].as_str())
            .unwrap_or_default();
        if effort == "heavy"
            && let Some((name, entry)) = selected
        {
            model = entry["heavy"]["model"]
                .as_str()
                .filter(|model| valid_model(model))
                .with_context(|| format!("pool {name} has no configured valid heavy.model"))?;
        }
        let agent = if explicit { actual_agent } else { "" };
        let budget = if explicit {
            selected
                .and_then(|(_, entry)| entry["budget"].as_str())
                .unwrap_or_default()
        } else {
            ""
        };
        let skilled = matches!(actual_agent, "claude" | "opencode" | "fake");
        Ok(format!(
            "agent={agent}\nmodel={model}\nbudget={budget}\nskilled={skilled}\n"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_alphabet_and_length() {
        for (model, valid) in [
            ("A_./:+-".to_string(), true),
            ("x".repeat(MODEL_LIMIT), true),
            ("x".repeat(MODEL_LIMIT + 1), false),
            ("_model".to_string(), false),
            ("model\n".to_string(), false),
        ] {
            assert_eq!(valid_model(&model), valid, "{model:?}");
        }
    }

    #[test]
    fn malformed_files_fail_loading() {
        for value in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!({"example": null}),
            serde_json::json!({"example": {}}),
        ] {
            assert!(Pools::from_json(&value).is_err(), "{value}");
        }
    }
}
