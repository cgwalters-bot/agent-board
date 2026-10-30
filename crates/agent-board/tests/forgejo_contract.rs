//! The Forgejo pass contract (docs/forges.md#a-forgejo-pass, agent-board#271):
//! every option of the pass's v1 configuration table (docs/pass.md) has one
//! Forgejo verdict, and every capability of each stage one, so an option or
//! a stage capability cannot be added without saying what a Forgejo pass
//! does with it. The contract is a specification; nothing here enables it.

use serde_json::{Value, json};

const CONTRACT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/forgejo/pass-contract.json"
);

const PASS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/pass.md");

const SCHEMA: &str = "agent-board/forgejo-pass-contract/v1";

/// The section of docs/pass.md whose table lists every option.
const OPTIONS_HEADING: &str = "## One configuration file";

/// Forge-independent: the option or capability means the same on Forgejo.
const SAME: &str = "same";

/// Needs the Forgejo mechanism the contract names, not yet built.
const NATIVE: &str = "native";

/// A Forgejo pass refuses it, and what depends on it holds, until a
/// reviewed native mechanism replaces the refusal.
const REFUSED: &str = "refused";

const VERDICTS: &[&str] = &[SAME, NATIVE, REFUSED];

/// The stages of docs/pass.md#order-and-effects, by number.
const STAGES: usize = 6;

/// Fewer options than this read from docs/pass.md means the table's
/// heading or layout changed under the reader, not that options went.
const MIN_OPTIONS: usize = 40;

/// The option names in the first cell of each row of `pass`'s option table.
fn documented_options(pass: &str) -> Vec<String> {
    let section = pass
        .lines()
        .skip_while(|l| *l != OPTIONS_HEADING)
        .skip(1)
        .take_while(|l| !l.starts_with("## "));
    let mut options = Vec::new();
    for line in section.filter(|l| l.starts_with("| `")) {
        let cell = line.split('|').nth(1).unwrap_or_default();
        // Odd pieces of a split on backticks are the quoted names.
        let names = cell.split('`').skip(1).step_by(2);
        options.extend(names.filter(|n| !n.is_empty()).map(str::to_string));
    }
    options
}

/// An entry's problems: an unknown verdict, a native one without its
/// `forgejo` mechanism, or a refusal without its `reason`.
fn entry_problems(at: &str, entry: &Value) -> Vec<String> {
    let verdict = entry["verdict"].as_str().unwrap_or_default();
    let needs = match verdict {
        SAME => None,
        NATIVE => Some("forgejo"),
        REFUSED => Some("reason"),
        _ => {
            let shown = &entry["verdict"];
            return vec![format!("{at}: verdict {shown} is not one of {VERDICTS:?}")];
        }
    };
    let missing = |key: &&str| entry[*key].as_str().is_none_or(|s| s.trim().is_empty());
    needs
        .filter(missing)
        .map(|key| format!("{at}: verdict {verdict} without `{key}`"))
        .into_iter()
        .collect()
}

/// `value`'s items, none where it is not a list.
fn list(value: &Value) -> Vec<Value> {
    value.as_array().cloned().unwrap_or_default()
}

/// Why `contract` does not cover `options` exactly once each with a
/// well-formed verdict, nor each stage's capabilities; empty when it does.
fn problems(contract: &Value, options: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    if contract["schema"] != SCHEMA {
        problems.push(format!("schema is not {SCHEMA}"));
    }
    // A contract, not a claim: flipping this needs the docs to say it is built.
    if contract["built"] != false {
        problems.push("built is not false".into());
    }
    let Some(listed) = contract["options"].as_object() else {
        problems.push("options is not an object".into());
        return problems;
    };
    for option in options {
        match listed.get(option) {
            Some(entry) => problems.extend(entry_problems(option, entry)),
            None => problems.push(format!("option `{option}` has no Forgejo verdict")),
        }
    }
    for option in listed.keys().filter(|k| !options.contains(k)) {
        problems.push(format!("option `{option}` is not in docs/pass.md's table"));
    }
    let stages = list(&contract["stages"]);
    if stages.len() != STAGES {
        problems.push(format!("{} stages, not {STAGES}", stages.len()));
    }
    for (n, stage) in (1..).zip(&stages) {
        if stage["stage"] != n {
            problems.push(format!("stage {n} is numbered {}", stage["stage"]));
        }
        let capabilities = list(&stage["capabilities"]);
        if capabilities.is_empty() {
            problems.push(format!("stage {n} lists no capabilities"));
        }
        for capability in &capabilities {
            let at = format!("stage {n} {}", capability["capability"]);
            problems.extend(entry_problems(&at, capability));
        }
    }
    problems
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn forgejo_contract_covers_every_option() {
    let contract: Value = serde_json::from_str(&read(CONTRACT)).unwrap();
    let options = documented_options(&read(PASS));
    assert!(
        options.len() >= MIN_OPTIONS,
        "read only {options:?} from docs/pass.md"
    );
    let problems = problems(&contract, &options);
    assert!(problems.is_empty(), "{CONTRACT}: {problems:#?}");
}

#[test]
fn forgejo_contract_refusals() {
    fn contract(options: Value, stages: Value) -> Value {
        json!({"schema": SCHEMA, "built": false, "options": options, "stages": stages})
    }
    let options = ["wip".to_string(), "leases".to_string()];
    let capability = json!({"capability": "c", "verdict": SAME});
    let stages: Vec<Value> = (1..=STAGES)
        .map(|n| json!({"stage": n, "capabilities": [capability]}))
        .collect();
    let stages = Value::from(stages);
    let same = json!({"wip": {"verdict": SAME}, "leases": {"verdict": SAME}});
    let cases: &[(&str, Value, &[&str])] = &[
        (
            "complete",
            contract(
                json!({
                    "wip": {"verdict": NATIVE, "forgejo": "runs listing"},
                    "leases": {"verdict": REFUSED, "reason": "no ref CAS"},
                }),
                stages.clone(),
            ),
            &[],
        ),
        (
            "missing and extra",
            contract(
                json!({"wip": {"verdict": SAME}, "old": {"verdict": SAME}}),
                stages.clone(),
            ),
            &[
                "option `leases` has no Forgejo verdict",
                "option `old` is not in docs/pass.md's table",
            ],
        ),
        (
            "malformed verdicts",
            contract(
                json!({"wip": {"verdict": NATIVE}, "leases": {"verdict": "maybe"}}),
                stages.clone(),
            ),
            &[
                "wip: verdict native without `forgejo`",
                "leases: verdict \"maybe\" is not one of [\"same\", \"native\", \"refused\"]",
            ],
        ),
        (
            "refusal without reason",
            contract(
                json!({"wip": {"verdict": SAME}, "leases": {"verdict": REFUSED, "reason": " "}}),
                stages.clone(),
            ),
            &["leases: verdict refused without `reason`"],
        ),
        (
            "no stages",
            contract(same.clone(), Value::Null),
            &["0 stages, not 6"],
        ),
        (
            "misnumbered empty stage",
            contract(same.clone(), json!([{"stage": 2, "capabilities": []}])),
            &[
                "1 stages, not 6",
                "stage 1 is numbered 2",
                "stage 1 lists no capabilities",
            ],
        ),
    ];
    for (name, contract, expected) in cases {
        assert_eq!(problems(contract, &options), *expected, "{name}");
    }
    let mut built = contract(same, stages);
    built["built"] = true.into();
    assert_eq!(problems(&built, &options), ["built is not false"]);
}

#[test]
fn documented_options_reads_every_name_of_a_row() {
    let pass = "## One configuration file\n\n| Key | Meaning |\n| --- | --- |\n\
        | `operator`, `bot` | x |\n| `pools.NAME.target` | `0.95` |\n## Next\n| `no` | x |\n";
    let expected = ["operator", "bot", "pools.NAME.target"];
    assert_eq!(documented_options(pass), expected);
}
