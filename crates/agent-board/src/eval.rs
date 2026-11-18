//! Offline summary of the three-arm role evaluation (docs/eval.md), ported
//! from cgwalters-forge/agenthome's src/eval.rs. No execution, pricing
//! inference, telemetry repair or exclusion of failed runs; it reads two JSON
//! documents and prints one.

use std::{
    cmp::Ordering,
    collections::HashSet,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use clap::{Args as ClapArgs, Subcommand};
use serde::{Serialize, Serializer, ser::Error as _};
use serde_json::{
    Value,
    ser::{CompactFormatter, Formatter, PrettyFormatter},
};
use sha2::{Digest, Sha256};

pub const ARMS: [&str; 3] = ["single-strong", "single-cheap", "tiered"];

const BASELINE: &str = ARMS[0];

pub const METRICS: [&str; 6] = [
    "inputTokens",
    "outputTokens",
    "cacheReadTokens",
    "cacheWriteTokens",
    "costUsd",
    "wallSeconds",
];

const COST: &str = "costUsd";

const WALL: &str = "wallSeconds";

pub const MAX_RUNS: usize = 54;

pub const MAX_INPUT_BYTES: u64 = 4 * 1024 * 1024;

const LIMITS: [(&str, f64); 4] = [
    ("wallSeconds", 1800.0),
    ("usd", 5.0),
    ("turns", 100.0),
    ("tokens", 200_000.0),
];

#[derive(ClapArgs)]
pub struct Eval {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarize offline evaluation runs against their frozen manifest.
    ///
    /// Both inputs are strict JSON of at most 4 MiB; RUNS is an array of at
    /// most 54 records. Nothing in either is executed or fetched. Prints the
    /// summary as JSON.stringify(summary, null, 2) would (docs/eval.md).
    Summarize {
        /// Pending or frozen evaluation manifest (eval/manifest.json).
        manifest: PathBuf,
        /// JSON array of run records matching the manifest.
        runs: PathBuf,
    },
}

pub fn run(args: &Eval, out: &mut impl Write) -> Result<()> {
    let Command::Summarize { manifest, runs } = &args.command;
    let (manifest, runs) = (read_json(manifest)?, read_json(runs)?);
    let summary = summarize(&manifest, &runs).context("eval")?;
    writeln!(out, "{}", pretty_json(&summary)?)?;
    Ok(())
}

/// Read one bounded JSON input; strict JSON, no comments.
pub fn read_json(path: &Path) -> Result<Value> {
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_INPUT_BYTES,
        "{}: input exceeds 4 MiB",
        path.display()
    );
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

/// ECMAScript's Number::toString, the spelling `JSON.stringify` writes:
/// shortest round-trip digits, `2` for 2.0, and exponents only below 1e-6 or
/// from 1e21 on.
fn js_number(value: f64) -> String {
    if value == 0.0 {
        return "0".to_owned();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let digits = mantissa.replace('.', "");
    let k = digits.len() as i32;
    let n = exponent.parse::<i32>().unwrap_or(0) + 1;
    let zeros = |count: i32| "0".repeat(count.max(0) as usize);
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", zeros(n - k))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", zeros(-n))
    } else {
        let (first, rest) = digits.split_at(1);
        let point = if rest.is_empty() { "" } else { "." };
        let exponent = n - 1;
        let exponent_sign = if exponent < 0 { "-" } else { "+" };
        format!("{first}{point}{rest}e{exponent_sign}{}", exponent.abs())
    };
    format!("{sign}{body}")
}

/// A JSON formatter that spells floating-point numbers as JavaScript does and
/// otherwise defers to `F`. serde_json already escapes strings as
/// `JSON.stringify` does and writes non-finite numbers as null, as it does.
struct Js<F>(F);

impl<F: Formatter> Formatter for Js<F> {
    fn write_f64<W: ?Sized + io::Write>(&mut self, writer: &mut W, value: f64) -> io::Result<()> {
        writer.write_all(js_number(value).as_bytes())
    }

    fn begin_array<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_array(writer)
    }

    fn end_array<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_array(writer)
    }

    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.0.begin_array_value(writer, first)
    }

    fn end_array_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_array_value(writer)
    }

    fn begin_object<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_object(writer)
    }

    fn end_object<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_object(writer)
    }

    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.0.begin_object_key(writer, first)
    }

    fn end_object_key<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_object_key(writer)
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_object_value(writer)
    }

    fn end_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_object_value(writer)
    }
}

fn to_json<F: Formatter>(value: &impl Serialize, formatter: F) -> Result<String> {
    let mut bytes = Vec::new();
    value.serialize(&mut serde_json::Serializer::with_formatter(
        &mut bytes,
        Js(formatter),
    ))?;
    String::from_utf8(bytes).context("serialize JSON")
}

/// `JSON.stringify(value, null, 2)` of a typed value: fields in declaration
/// order, as the JavaScript summarizer inserted them.
pub fn pretty_json(value: &impl Serialize) -> Result<String> {
    to_json(value, PrettyFormatter::with_indent(b"  "))
}

/// Whether JavaScript treats `key` as an array index, which its objects list
/// before every other key, in ascending order.
fn array_index(key: &str) -> Option<u32> {
    if key != "0" && key.starts_with('0') || !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    key.parse::<u32>().ok().filter(|&index| index < u32::MAX)
}

/// Keys sorted by UTF-16 code units, as JavaScript's default sort, then
/// listed as a JavaScript object enumerates them: array indices first.
fn javascript_key_order(left: &str, right: &str) -> Ordering {
    match (array_index(left), array_index(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.encode_utf16().cmp(right.encode_utf16()),
    }
}

/// A manifest with recursively sorted object keys and every number as a
/// JavaScript double; array order stays part of the frozen protocol.
struct Canonical<'a>(&'a Value);

impl Serialize for Canonical<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Number(number) => serializer.serialize_f64(
                number
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .ok_or_else(|| S::Error::custom("JSON number outside finite f64 range"))?,
            ),
            Value::Array(values) => serializer.collect_seq(values.iter().map(Canonical)),
            Value::Object(values) => {
                let mut keys: Vec<_> = values.keys().collect();
                keys.sort_by(|left, right| javascript_key_order(left, right));
                serializer.collect_map(keys.into_iter().map(|key| (key, Canonical(&values[key]))))
            }
            value => value.serialize(serializer),
        }
    }
}

/// SHA-256 of `JSON.stringify` of the manifest with recursively sorted keys,
/// so digests recorded by agenthome's JavaScript and Rust summarizers stay
/// valid.
pub fn manifest_digest(manifest: &Value) -> Result<String> {
    let json = to_json(&Canonical(manifest), CompactFormatter)?;
    Ok(Sha256::digest(json)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn text(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

fn hex(value: Option<&Value>, len: usize) -> bool {
    value.and_then(Value::as_str).is_some_and(|value| {
        value.len() == len
            && value
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

fn revision(value: Option<&Value>) -> bool {
    hex(value, 40)
}

fn digest(value: Option<&Value>) -> bool {
    hex(value, 64)
}

fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

/// JavaScript strict equality for the scalar fields compared here, and deep
/// equality for objects: numbers compare by value, not by spelling.
fn same(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(l, r)| same(l, r))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, l)| right.get(key).is_some_and(|r| same(l, r)))
        }
        _ => left == right,
    }
}

fn strings(value: Option<&Value>, expected: &[&str]) -> bool {
    value.and_then(Value::as_array).is_some_and(|values| {
        values.len() == expected.len() && values.iter().zip(expected).all(|(v, e)| v == e)
    })
}

/// A nonnegative value, or null with the reason it is unavailable.
fn metric(value: Option<&Value>, label: &str) -> Result<Option<f64>> {
    let value = value.filter(|value| value.is_object());
    let result = value.and_then(|metric| match metric.get("value") {
        Some(Value::Null) => text(metric.get("unavailable")).then_some(None),
        found => number(found).filter(|&n| n >= 0.0).map(Some),
    });
    result.with_context(|| {
        format!("{label}: expected nonnegative value or null with unavailable reason")
    })
}

pub fn validate_manifest(m: &Value) -> Result<()> {
    ensure!(
        number(m.get("version")) == Some(1.0),
        "unsupported manifest version"
    );
    ensure!(strings(m.get("arms"), &ARMS), "exactly three declared arms");
    ensure!(
        strings(m.get("cachePolicies"), &["cold", "warm"]),
        "declare cold and warm separately"
    );
    ensure!(
        number(m.get("repetitions")) == Some(3.0),
        "three repetitions required"
    );
    ensure!(
        m.get("decisionRule").and_then(Value::as_str) == Some("quality-cost-time-v1"),
        "unknown decision rule"
    );
    for (key, ceiling) in LIMITS {
        let limit = number(m.get("limits").and_then(|limits| limits.get(key)));
        ensure!(
            limit.is_some_and(|limit| limit > 0.0 && limit <= ceiling),
            "invalid limit {key}"
        );
    }
    let tasks = m
        .get("tasks")
        .and_then(Value::as_array)
        .context("tasks must be an array")?;
    let status = m.get("status").and_then(Value::as_str);
    let pending = status == Some("pending");
    if pending {
        ensure!(
            text(m.get("pending")) && m.get("configuration") == Some(&Value::Null),
            "pending sample needs a reason and null configuration"
        );
        if tasks.is_empty() {
            ensure!(
                m.get("preflight") == Some(&Value::Null),
                "empty pending sample must have null preflight"
            );
            return Ok(());
        }
    }
    ensure!(
        pending || status == Some("frozen"),
        "status must be pending or frozen"
    );
    let preflight = m.get("preflight");
    ensure!(
        text(preflight.and_then(|p| p.get("authorization")))
            && text(preflight.and_then(|p| p.get("evidence"))),
        "authorized sizing preflight required"
    );
    ensure!((2..=3).contains(&tasks.len()), "freeze two or three tasks");
    let mut ids = HashSet::new();
    for task in tasks {
        let id = task.get("id");
        ensure!(
            text(id) && ids.insert(id.and_then(Value::as_str)),
            "unique task id required"
        );
        ensure!(
            text(task.get("repository"))
                && text(task.get("history"))
                && revision(task.get("startCommit")),
            "task needs history and immutable start"
        );
        let checks = task.get("checks");
        ensure!(
            text(task.get("taskText"))
                && digest(checks.and_then(|c| c.get("sha256")))
                && text(checks.and_then(|c| c.get("artifact"))),
            "common task text and pinned independent checks required"
        );
        ensure!(
            text(task.get("sizingEvidence")),
            "task sizing evidence required"
        );
    }
    // A prepared historical sample is not execution authority: configuration
    // and run admission still require the separate frozen transition.
    if pending {
        return Ok(());
    }
    let c = m
        .get("configuration")
        .filter(|c| c.is_object())
        .context("pin configuration, harness, role interface and environment")?;
    ensure!(
        revision(c.get("configRevision"))
            && revision(c.get("harnessRevision"))
            && text(c.get("roleInterface"))
            && text(c.get("environment")),
        "pin configuration, harness, role interface and environment"
    );
    let basis = c.get("costBasis").and_then(Value::as_object);
    ensure!(
        basis.is_some_and(|basis| if text(basis.get("unavailable")) {
            basis.len() == 1
        } else {
            text(basis.get("artifact")) && digest(basis.get("sha256"))
        }),
        "pin one common cost basis or declare unavailable"
    );
    let arms = c
        .get("arms")
        .and_then(Value::as_object)
        .filter(|arms| arms.len() == ARMS.len() && ARMS.iter().all(|arm| arms.contains_key(*arm)))
        .context("arm render references required")?;
    for arm in ARMS {
        let a = &arms[arm];
        ensure!(
            text(a.get("artifact")) && digest(a.get("sha256")) && text(a.get("resolvedModels")),
            "{arm}: pin rendered artifact and resolved models"
        );
    }
    Ok(())
}

fn validate_run(r: &Value, m: &Value, manifest_sha256: &str) -> Result<()> {
    ensure!(
        number(r.get("version")) == Some(1.0)
            && text(r.get("id"))
            && text(r.get("runUrl"))
            && text(r.get("artifactUrl")),
        "run identity and artifact links required"
    );
    ensure!(
        r.get("manifestSha256").and_then(Value::as_str) == Some(manifest_sha256),
        "mixed comparison manifests"
    );
    let task = m["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| r.get("taskId").is_some_and(|id| same(id, &t["id"])));
    let arm = r
        .get("arm")
        .and_then(Value::as_str)
        .filter(|arm| ARMS.contains(arm));
    let policy = r.get("cachePolicy").and_then(Value::as_str);
    let repetition = number(r.get("repetition"));
    let repetitions = m["repetitions"].as_f64().unwrap_or(0.0);
    let in_matrix = policy.is_some_and(|policy| {
        m["cachePolicies"]
            .as_array()
            .is_some_and(|policies| policies.iter().any(|p| p == policy))
    }) && repetition
        .is_some_and(|n| n.fract() == 0.0 && n >= 1.0 && n <= repetitions);
    let (task, arm) = task
        .zip(arm)
        .filter(|_| in_matrix)
        .context("run outside declared matrix")?;
    let field = |key: &str| r.get(key).unwrap_or(&Value::Null);
    ensure!(
        same(field("startCommit"), &task["startCommit"])
            && same(field("taskText"), &task["taskText"])
            && same(field("checksSha256"), &task["checks"]["sha256"]),
        "mixed task revisions"
    );
    let c = &m["configuration"];
    let render = &c["arms"][arm];
    ensure!(
        same(field("configRevision"), &c["configRevision"])
            && same(field("harnessRevision"), &c["harnessRevision"])
            && same(field("environment"), &c["environment"])
            && same(field("renderSha256"), &render["sha256"])
            && same(field("resolvedModels"), &render["resolvedModels"]),
        "mixed config revisions, models, environment or cost basis"
    );
    ensure!(
        same(field("costBasis"), &c["costBasis"]),
        "mixed cost basis"
    );
    let telemetry = r
        .get("telemetry")
        .and_then(|t| t.get("status"))
        .and_then(Value::as_str);
    ensure!(
        telemetry.is_some_and(|status| ["complete", "missing", "truncated"].contains(&status)),
        "telemetry status required"
    );
    if telemetry != Some("complete") {
        ensure!(
            text(r["telemetry"].get("reason")),
            "incomplete telemetry needs reason"
        );
    }
    let id = r["id"].as_str().unwrap_or_default();
    let metrics = r.get("metrics");
    for name in METRICS {
        metric(metrics.and_then(|m| m.get(name)), &format!("{id}.{name}"))?;
    }
    // Any truthy marker, even beside a pinned artifact, means no comparable cost.
    let unavailable = match c["costBasis"].get("unavailable") {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::String(reason)) => !reason.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        Some(_) => true,
    };
    if unavailable {
        ensure!(
            r["metrics"][COST].get("value") == Some(&Value::Null),
            "unavailable cost basis requires unavailable cost"
        );
    }
    let stages = r
        .get("stageSeconds")
        .and_then(Value::as_object)
        .context("stage timing record required")?;
    if stages.contains_key("unavailable") {
        ensure!(
            text(stages.get("unavailable")) && stages.len() == 1,
            "invalid unavailable stages"
        );
    } else {
        ensure!(!stages.is_empty(), "empty stage timing is not unavailable");
        for (stage, value) in stages {
            metric(Some(value), &format!("stage {stage}"))?;
        }
    }
    let outcome = r.get("outcome").and_then(Value::as_str);
    ensure!(
        outcome.is_some_and(|o| ["passed", "failed", "timeout", "harness-error"].contains(&o)),
        "invalid outcome"
    );
    let failures = r.get("failures").and_then(Value::as_array);
    ensure!(
        failures.is_some_and(|failures| {
            failures.iter().all(|f| text(Some(f)))
                && (outcome == Some("passed") || !failures.is_empty())
        }),
        "failed run needs failure details"
    );
    ensure!(
        text(r.get("testEvidence")),
        "independent test evidence required"
    );
    Ok(())
}

/// The summary, in the field order the JavaScript summarizer wrote: a
/// pending summary has `reason`, any other `manifestSha256`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary<'a> {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_sha256: Option<String>,
    groups: Vec<Group<'a>>,
    decisions: Vec<Decision>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Group<'a> {
    cache_policy: &'a str,
    arm: &'static str,
    runs: usize,
    expected: usize,
    passed: usize,
    failures: Vec<Failure<'a>>,
    metrics: Metrics,
}

impl Group<'_> {
    fn metric(&self, name: &str) -> Option<f64> {
        METRICS
            .iter()
            .position(|metric| *metric == name)
            .and_then(|index| self.metrics.0[index].value)
    }
}

#[derive(Debug, Serialize)]
struct Failure<'a> {
    id: &'a str,
    outcome: &'a str,
    failures: Vec<&'a str>,
}

/// A total, or null: with the reason, or without one where the sum overflowed,
/// as `JSON.stringify` writes an infinite total.
#[derive(Debug, Serialize)]
struct Metric {
    value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable: Option<String>,
}

/// One total per entry of `METRICS`, in that order.
#[derive(Debug)]
struct Metrics([Metric; METRICS.len()]);

impl Serialize for Metrics {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(METRICS.iter().zip(&self.0))
    }
}

#[derive(Debug, Serialize)]
struct Decision {
    arm: &'static str,
    verdict: &'static str,
    reason: &'static str,
}

fn value(run: &Value, name: &str) -> Option<f64> {
    run["metrics"][name]["value"].as_f64()
}

fn total(runs: &[&Value], name: &str) -> Metric {
    if runs.is_empty() {
        return Metric {
            value: None,
            unavailable: Some("No runs recorded".to_owned()),
        };
    }
    let missing = runs.iter().filter(|r| value(r, name).is_none()).count();
    if missing > 0 {
        return Metric {
            value: None,
            unavailable: Some(format!("{missing} run(s) lack {name}")),
        };
    }
    let sum = runs.iter().filter_map(|r| value(r, name)).sum::<f64>();
    Metric {
        value: Some(sum).filter(|sum| sum.is_finite()),
        unavailable: None,
    }
}

/// Whether `value` exceeds `baseline * ratio` by more than floating-point
/// roundoff. At most nine terms per group: absorb only binary rounding, not
/// a currency-sized allowance that would hide real threshold violations.
fn above_threshold(value: f64, baseline: f64, ratio: f64) -> bool {
    let threshold = baseline * ratio;
    let tolerance = 32.0 * f64::EPSILON * value.max(threshold);
    value - threshold > tolerance
}

fn decide(groups: &[Group], arm: &'static str, complete: bool, expected: usize) -> Decision {
    // Every policy has a group for every arm, so no baseline lacks a pair.
    let pairs: Vec<_> = groups
        .iter()
        .filter(|g| g.arm == BASELINE)
        .filter_map(|base| {
            groups
                .iter()
                .find(|g| g.arm == arm && g.cache_policy == base.cache_policy)
                .map(|candidate| (base, candidate))
        })
        .collect();
    let cost_time = |g: &Group| Some((g.metric(COST)?, g.metric(WALL)?));
    let (verdict, reason) = if !complete || pairs.is_empty() {
        (
            "inconclusive",
            "Incomplete matrix; do not drop or replace failures.",
        )
    } else if pairs
        .iter()
        .any(|(base, candidate)| base.passed != expected || candidate.passed != expected)
    {
        (
            "reject",
            "Quality gate requires every baseline and candidate run to pass.",
        )
    } else if pairs.iter().any(
        |(base, candidate)| match (cost_time(base), cost_time(candidate)) {
            (Some((cost, time)), Some(_)) => cost == 0.0 || time == 0.0,
            _ => true,
        },
    ) {
        (
            "inconclusive",
            "Cost/time unavailable or baseline denominator zero.",
        )
    } else if pairs.iter().any(
        |(base, candidate)| match (cost_time(base), cost_time(candidate)) {
            (Some((base_cost, base_time)), Some((cost, time))) => {
                above_threshold(cost, base_cost, 0.8) || above_threshold(time, base_time, 1.1)
            }
            _ => true,
        },
    ) {
        (
            "reject",
            "Cost/time threshold not met in both cache strata.",
        )
    } else {
        (
            "eligible",
            "All checks pass; at least 20% cheaper and no more than 10% slower in both cache strata.",
        )
    };
    Decision {
        arm,
        verdict,
        reason,
    }
}

pub fn summarize<'a>(m: &'a Value, runs: &'a Value) -> Result<Summary<'a>> {
    validate_manifest(m)?;
    let runs = runs
        .as_array()
        .filter(|runs| runs.len() <= MAX_RUNS)
        .with_context(|| format!("runs must be an array of at most {MAX_RUNS} records"))?;
    if m["status"] == "pending" {
        ensure!(runs.is_empty(), "cannot accept runs before freezing sample");
        return Ok(Summary {
            status: "pending",
            reason: m["pending"].as_str(),
            manifest_sha256: None,
            groups: Vec::new(),
            decisions: Vec::new(),
        });
    }
    let manifest_sha256 = manifest_digest(m)?;
    let mut cells = HashSet::new();
    let mut ids = HashSet::new();
    for r in runs {
        validate_run(r, m, &manifest_sha256)?;
        let cell = (
            r["taskId"].as_str(),
            r["arm"].as_str(),
            r["cachePolicy"].as_str(),
            // Validated as an integer; 1 and 1.0 are the same cell.
            r["repetition"].as_f64().map(|n| n as u64),
        );
        ensure!(
            cells.insert(cell) && ids.insert(r["id"].as_str()),
            "duplicate run or matrix cell"
        );
    }
    // Validated above: three repetitions and two or three tasks.
    let repetitions = m["repetitions"].as_f64().unwrap_or_default() as usize;
    let expected = m["tasks"].as_array().map_or(0, Vec::len) * repetitions;
    let policies: Vec<_> = m["cachePolicies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut groups = Vec::new();
    for &cache_policy in &policies {
        for arm in ARMS {
            let selected: Vec<_> = runs
                .iter()
                .filter(|r| r["cachePolicy"] == cache_policy && r["arm"] == arm)
                .collect();
            let passed = selected.iter().filter(|r| r["outcome"] == "passed").count();
            let failures = selected
                .iter()
                .filter(|r| r["outcome"] != "passed")
                .map(|r| Failure {
                    id: r["id"].as_str().unwrap_or_default(),
                    outcome: r["outcome"].as_str().unwrap_or_default(),
                    failures: r["failures"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect(),
                })
                .collect();
            groups.push(Group {
                cache_policy,
                arm,
                runs: selected.len(),
                expected,
                passed,
                failures,
                metrics: Metrics(METRICS.map(|name| total(&selected, name))),
            });
        }
    }
    let complete = runs.len() == expected * ARMS.len() * policies.len();
    let decisions = ARMS[1..]
        .iter()
        .map(|arm| decide(&groups, arm, complete, expected))
        .collect();
    Ok(Summary {
        status: if complete { "complete" } else { "incomplete" },
        reason: None,
        manifest_sha256: Some(manifest_sha256),
        groups,
        decisions,
    })
}

#[cfg(test)]
mod tests;
