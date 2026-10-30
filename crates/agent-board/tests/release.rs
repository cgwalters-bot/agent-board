//! The v1 readiness checklist (docs/release.md): every gate is listed once,
//! a met gate links its evidence, and a workspace version of 1.0.0 or later
//! needs every gate met and its own changelog section, so a version bump
//! cannot merge while the evidence is missing.

const CHECKLIST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/release.md");

const CHANGELOG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../CHANGELOG.md");

/// The gates of tracker#939 and agent-board#466, in the checklist's order.
const GATES: &[&str] = &[
    "config-contract",
    "readme-trial",
    "fresh-deploy",
    "deploy-recurrence",
    "deployment-logic",
    "shared-logic",
    "backlog",
    "release-path",
    "changelog",
];

const MET: &str = "met";

const BLOCKED: &str = "blocked";

/// The section of the checklist whose table rows are the gates.
const GATES_HEADING: &str = "## Gates";

/// Live evidence of a gate with nothing live to show, in place of a link.
const NOT_APPLICABLE: &str = "n/a";

/// Gate, requirement, merged implementation, live evidence, status.
const COLUMNS: usize = 5;

/// Why `version` could not be released as `checklist` and `changelog`
/// stand; empty when nothing keeps it back.
fn problems(checklist: &str, changelog: &str, version: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = Vec::new();
    let mut blocked = Vec::new();
    let section = checklist
        .lines()
        .skip_while(|l| *l != GATES_HEADING)
        .skip(1)
        .take_while(|l| !l.starts_with("## "));
    for line in section.filter(|l| l.starts_with("| `")) {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        // The split has an empty cell before the first `|` and after the last.
        if cells.len() != COLUMNS + 2 {
            problems.push(format!("gate row without {COLUMNS} cells: {line}"));
            continue;
        }
        let [gate, _, implementation, live, status] = [1, 2, 3, 4, 5].map(|i| cells[i]);
        let gate = gate.trim_matches('`');
        if !GATES.contains(&gate) {
            problems.push(format!("unknown gate `{gate}`"));
        } else if seen.contains(&gate) {
            problems.push(format!("gate `{gate}` listed twice"));
        }
        seen.push(gate);
        match status {
            MET => {
                for (column, cell, optional) in [
                    ("merged implementation", implementation, false),
                    ("live evidence", live, true),
                ] {
                    let shown = cell.contains("](https://") || (optional && cell == NOT_APPLICABLE);
                    if !shown {
                        problems.push(format!(
                            "gate `{gate}` is met without a link to its {column}"
                        ));
                    }
                }
            }
            BLOCKED => blocked.push(gate),
            _ => problems.push(format!(
                "gate `{gate}` has status `{status}`, not `{MET}` or `{BLOCKED}`"
            )),
        }
    }
    for gate in GATES.iter().filter(|g| !seen.contains(g)) {
        problems.push(format!("gate `{gate}` is missing"));
    }
    match version.split('.').next().map(str::parse::<u64>) {
        Some(Ok(0)) => {}
        Some(Ok(_)) => {
            if !blocked.is_empty() {
                problems.push(format!(
                    "version {version} with blocked gates: {}",
                    blocked.join(", ")
                ));
            }
            if !changelog.lines().any(|l| l == format!("## v{version}")) {
                problems.push(format!(
                    "version {version} without a `## v{version}` changelog section"
                ));
            }
        }
        _ => problems.push(format!("version {version} has no numeric major")),
    }
    problems
}

#[test]
fn release_readiness_holds() {
    let read = |path| std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let problems = problems(
        &read(CHECKLIST),
        &read(CHANGELOG),
        env!("CARGO_PKG_VERSION"),
    );
    assert!(problems.is_empty(), "docs/release.md: {problems:#?}");
}

#[test]
fn release_readiness_refusals() {
    let link = "[#1](https://example.com/1)";
    let row = |gate: &str, implementation: &str, live: &str, status: &str| {
        format!("| `{gate}` | requirement | {implementation} | {live} | {status} |\n")
    };
    let all = |status: &str| -> String {
        GATES
            .iter()
            .map(|g| row(g, link, NOT_APPLICABLE, status))
            .collect()
    };
    let without = |gate: &str| all(MET).replace(&row(gate, link, NOT_APPLICABLE, MET), "");
    let v1 = "## v1.0.0\n";
    let cases: &[(&str, String, &str, &str, &[&str])] = &[
        ("all blocked before v1", all(BLOCKED), "", "0.1.0", &[]),
        ("all met at v1", all(MET), v1, "1.0.0", &[]),
        (
            "blocked at v1",
            all(BLOCKED),
            v1,
            "1.0.0",
            &["version 1.0.0 with blocked gates: config-contract, readme-trial"],
        ),
        (
            "no changelog section at v1",
            all(MET),
            "## Unreleased\n",
            "1.0.0",
            &["version 1.0.0 without a `## v1.0.0` changelog section"],
        ),
        (
            "missing gate",
            without("backlog"),
            "",
            "0.1.0",
            &["gate `backlog` is missing"],
        ),
        (
            "unknown and repeated gates",
            all(BLOCKED) + &row("tag", link, link, BLOCKED) + &row("backlog", link, link, BLOCKED),
            "",
            "0.1.0",
            &["unknown gate `tag`", "gate `backlog` listed twice"],
        ),
        (
            "met without evidence",
            without("fresh-deploy") + &row("fresh-deploy", "None", "issue closed", MET),
            "",
            "0.1.0",
            &[
                "gate `fresh-deploy` is met without a link to its merged implementation",
                "gate `fresh-deploy` is met without a link to its live evidence",
            ],
        ),
        (
            "met with no implementation",
            without("backlog") + &row("backlog", NOT_APPLICABLE, NOT_APPLICABLE, MET),
            "",
            "0.1.0",
            &["gate `backlog` is met without a link to its merged implementation"],
        ),
        (
            "a table after the gates",
            all(MET) + "## Later\n" + &row("tag", "None", "None", "done"),
            "",
            "0.1.0",
            &[],
        ),
        (
            "other status",
            without("changelog") + &row("changelog", link, link, "done"),
            "",
            "0.1.0",
            &["gate `changelog` has status `done`, not `met` or `blocked`"],
        ),
        (
            "short row",
            without("changelog") + "| `changelog` | met |\n",
            "",
            "0.1.0",
            &["gate row without 5 cells", "gate `changelog` is missing"],
        ),
        (
            "bad version",
            all(MET),
            "",
            "x.1",
            &["version x.1 has no numeric major"],
        ),
    ];
    for (name, checklist, changelog, version, expected) in cases {
        let found = problems(&format!("{GATES_HEADING}\n{checklist}"), changelog, version);
        assert_eq!(found.len(), expected.len(), "{name}: {found:#?}");
        for (found, expected) in found.iter().zip(*expected) {
            assert!(
                found.starts_with(expected),
                "{name}: {found} is not {expected}"
            );
        }
    }
}
