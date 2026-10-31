//! Preview-only Forgejo pass. Identity precedes broker reads; no publication,
//! GitHub fallback, adapter activation or dispatch is reachable here.
use super::*;
use crate::forgejo::{Config as ForgejoConfig, Transport as ForgejoTransport};

const CONTRACT: &str = include_str!("../../../../fixtures/forgejo/pass-contract.json");

/// Reject explicit settings, even false, empty or malformed ones, before typed
/// decoding can discard their presence or hide the capability diagnostic.
pub(super) fn check_options(value: &Value) -> Result<()> {
    let contract: Value = serde_json::from_str(CONTRACT).context("Forgejo pass contract")?;
    for (option, entry) in contract["options"]
        .as_object()
        .context("contract options")?
    {
        let pointer = format!("/{}", option.replace('.', "/"));
        if entry["verdict"] == "refused" && value.pointer(&pointer).is_some() {
            bail!(
                "unsupported_capability: {option}: {}",
                entry["reason"].as_str().unwrap_or_default()
            );
        }
    }
    for (option, follow_up) in [
        ("epics", "epic driver #514"),
        ("holds", "scheduler and hold publication #513"),
        ("usage.repository", "usage publication in scheduler #513"),
        ("usage.issue", "usage publication in scheduler #513"),
        ("usage.probe", "run inventory in scheduler #513"),
        ("sweep", "a follow-up to scheduler #513 for sweep ownership"),
        (
            "revisit",
            "a follow-up to scheduler #513 for revisit ownership",
        ),
        ("leases", "lease wiring in scheduler #513"),
        ("reruns", "run wiring in scheduler #513"),
    ] {
        let pointer = format!("/{}", option.replace('.', "/"));
        if value.pointer(&pointer).is_some() {
            bail!(
                "unsupported_capability: {option}: not built on Forgejo yet; needs {follow_up}; leave this option out for preview"
            );
        }
    }
    Ok(())
}

impl<C: ForgejoTransport> ForgejoTransport for Budget<'_, C> {
    fn get(&self, url: &url::Url) -> Result<crate::forgejo::Response> {
        self.spend(false)?;
        self.observe(self.inner.get(url))
    }
}

fn identity(
    client: &impl ForgejoTransport,
    board: &ForgejoConfig,
    config: &Config,
) -> Result<String> {
    let api = board.api_root()?;
    let bot = client.get(&api.join("user")?)?.body;
    ensure!(
        text(&bot, "/login").eq_ignore_ascii_case(&config.bot),
        "the Forgejo token's login is not bot {:?}",
        config.bot
    );
    let bot_id = bot["id"].as_u64().context("bot has no user id")?;
    ensure!(
        board.recorders.contains(&bot_id),
        "bot id {bot_id} is not in board.recorders"
    );
    let mut operator_url = api.join("users/")?;
    operator_url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid API root"))?
        .pop_if_empty()
        .push(&config.operator);
    let operator = client.get(&operator_url)?.body;
    ensure!(
        text(&operator, "/login").eq_ignore_ascii_case(&config.operator),
        "Forgejo operator lookup returned a different login"
    );
    let operator_id = operator["id"].as_u64().context("operator has no user id")?;
    ensure!(
        board.recorders.contains(&operator_id),
        "operator id {operator_id} is not in board.recorders; verify the operator's numeric id, not a reused login"
    );
    Ok(format!(
        "identity: bot {} (id {bot_id}), operator {} (id {operator_id}); both in recorders",
        config.bot, config.operator
    ))
}

/// Run only identity and stage 1 under the shared request and time bounds.
pub fn run(
    client: &impl ForgejoTransport,
    broker: &impl Broker,
    config: &Config,
    (mode, limits): (Mode<'_>, Limits),
    now: OffsetDateTime,
) -> Result<Report> {
    let bounds = Bounds::new(
        PASS_REQUEST_LIMIT,
        Instant::now() + Duration::from_secs(PASS_SECONDS_LIMIT),
    );
    run_within(client, broker, config, (mode, limits), now, &bounds)
}

fn run_within(
    client: &impl ForgejoTransport,
    broker: &impl Broker,
    config: &Config,
    (mode, limits): (Mode<'_>, Limits),
    now: OffsetDateTime,
    bounds: &Bounds,
) -> Result<Report> {
    ensure!(
        matches!(mode, Mode::Preview),
        "unsupported_capability: --apply: Forgejo scheduling needs #513"
    );
    config.check()?;
    let board = config
        .forgejo_board()
        .context("Forgejo pass needs a Forgejo board")?;
    let client = Budget {
        inner: client,
        bounds,
    };
    let mut report = Report {
        summary: Caps::new(limits).summary(),
        ..Report::default()
    };
    match identity(&client, board, config) {
        Ok(line) => report.lines.push(line),
        Err(error) => {
            report.failures += 1;
            report.lines.push(format!("identity error: {error:#}"));
            report.spent = bounds.spent();
            return Ok(report);
        }
    }
    // Broker calls share the same bound and clock, but not the forge transport
    // or its token. Nothing can fall back to a forge snapshot here.
    let broker = Budget {
        inner: broker,
        bounds,
    };
    refresh(&broker, config, now, &mut report)?;
    for (stage, name) in [
        (2, "follow pull requests"),
        (3, "epics"),
        (4, "schedule"),
        (5, "status"),
        (6, "cost"),
    ] {
        report
            .lines
            .push(format!("stage {stage} {name}: not built on Forgejo yet"));
    }
    report.spent = bounds.spent();
    Ok(report)
}

/// Bound each broker read as well as forge reads. A slow read cannot admit a
/// second request after the deadline; HTTP transports also bound each call.
struct BrokerReads<'a, B>(&'a Budget<'a, B>);

impl<B: Broker> Broker for BrokerReads<'_, B> {
    fn usage(&self, url: &str) -> Result<Value> {
        self.0.spend(false)?;
        self.0.inner.usage(url)
    }

    fn grant(&self, url: &str) -> Result<Value> {
        self.0.spend(false)?;
        self.0.inner.grant(url)
    }
}

fn refresh<B: Broker>(
    broker: &Budget<'_, B>,
    config: &Config,
    now: OffsetDateTime,
    report: &mut Report,
) -> Result<()> {
    let Some(url) = config.usage.as_ref().and_then(|u| u.url.as_deref()) else {
        report
            .lines
            .push("usage: no usage.url; no broker reading, no forge fallback".into());
        return Ok(());
    };
    let broker = BrokerReads(broker);
    let grant = config
        .grant()
        .and_then(|budget| match usage::read_grant(&broker, budget) {
            Ok(grant) => Some(grant),
            Err(error) => {
                report.failures += 1;
                report.lines.push(format!(
                    "usage error: reading the grant: {error:#}; every pool holds"
                ));
                None
            }
        });
    let read = broker
        .usage(url)
        .and_then(|answer| usage::reading(&answer, config, grant.as_ref(), now));
    let snapshot = match read {
        Ok((_, snapshot)) => snapshot,
        Err(error) => {
            report.failures += 1;
            report.lines.push(format!(
                "usage error: reading the broker: {error:#}; no forge fallback"
            ));
            let mut snapshot = usage::bare(&config.pools, now)?;
            if let Some(budget) = config.grant() {
                usage::grants(&mut snapshot, grant.as_ref(), budget, now)?;
            }
            snapshot
        }
    };
    report
        .lines
        .push(format!("usage: {}", usage::pools(&snapshot).join("; ")));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forgejo::Response;

    fn file() -> Value {
        let contract: Value = serde_json::from_str(CONTRACT).unwrap();
        json!({"operator": "operator", "bot": "bot", "dispatchRepo": "example/control",
            "dispatchWorkflow": "agent.yml", "board": contract["instance"]["board"],
            "pools": {"claude": {}, "openai": {}}, "usage": {"url": "http://broker.internal/usage"}})
    }

    fn config(value: &Value) -> Config {
        Config::parse(&value.to_string()).unwrap()
    }

    fn set(value: &mut Value, path: &str, leaf: Value) {
        let (head, rest) = path.split_once('.').unwrap_or((path, ""));
        if rest.is_empty() {
            value[head] = leaf;
        } else {
            if !value[head].is_object() {
                value[head] = json!({});
            }
            set(&mut value[head], rest, leaf);
        }
    }

    #[test]
    fn naming_follows_trusted_board() {
        for (login, forgejo, github) in [
            ("alice", true, true),
            ("alice-smith", true, true),
            ("alice.smith", true, false),
            ("alice_smith", true, true),
            ("a..b", false, false),
            ("a._b", false, false),
            (".alice", false, false),
            ("alice.", false, false),
            ("..", false, false),
            ("../alice", false, false),
            ("alice/bob", false, false),
            ("alice\\bob", false, false),
            ("alice?x", false, false),
            ("alice#x", false, false),
            ("alice\n", false, false),
            ("%2e%2e", false, false),
            ("//evil.example", false, false),
            ("https://evil.example", false, false),
            ("alice@evil.example", false, false),
        ] {
            for (is_forgejo, accepted) in [(true, forgejo), (false, github)] {
                for field in ["operator", "bot", "dispatchRepo", "workRepositories"] {
                    let mut value = file();
                    // Keep the GitHub baseline independent of broker settings.
                    value.as_object_mut().unwrap().remove("usage");
                    value.as_object_mut().unwrap().remove("pools");
                    if !is_forgejo {
                        value.as_object_mut().unwrap().remove("board");
                    }
                    value[field] = match field {
                        "dispatchRepo" => json!(format!("{login}/work.repo")),
                        "workRepositories" => json!([format!("{login}/work.repo")]),
                        _ => json!(login),
                    };
                    assert_eq!(
                        Config::parse(&value.to_string()).is_ok(),
                        accepted,
                        "forgejo={is_forgejo} {field}={login:?}"
                    );
                }
            }
        }
        for name in [
            "work.repo",
            ".config",
            "work_repo",
            "work-repo",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a?b",
            "a#b",
            "a\n",
            "%2f",
            "//evil.example",
        ] {
            let accepted = matches!(name, "work.repo" | ".config" | "work_repo" | "work-repo");
            for field in ["dispatchRepo", "workRepositories"] {
                let mut value = file();
                let repo = format!("example/{name}");
                value[field] = if field == "workRepositories" {
                    json!([repo])
                } else {
                    json!(repo)
                };
                assert_eq!(
                    Config::parse(&value.to_string()).is_ok(),
                    accepted,
                    "{field}={name:?}"
                );
            }
        }
    }

    #[test]
    fn contract_drives_every_refused_option_before_decoding() {
        let contract: Value = serde_json::from_str(CONTRACT).unwrap();
        for (option, entry) in contract["options"].as_object().unwrap() {
            if entry["verdict"] != "refused" {
                continue;
            }
            for supplied in [json!(false), json!(0), json!([]), json!({}), Value::Null] {
                let mut value = file();
                set(&mut value, option, supplied);
                let error = Config::parse(&value.to_string()).unwrap_err().to_string();
                assert_eq!(
                    error,
                    format!(
                        "unsupported_capability: {option}: {}",
                        entry["reason"].as_str().unwrap()
                    )
                );
            }
        }
    }

    #[test]
    fn temporary_refusals_and_broker_only_usage() {
        assert!(config(&file()).forgejo_board().is_some());
        for (option, follow_up) in [
            ("epics.repository", "#514"),
            ("holds.issue", "#513"),
            ("usage.repository", "#513"),
            ("usage.issue", "#513"),
            ("usage.probe", "#513"),
            ("sweep.max", "sweep ownership"),
            ("revisit.perPass", "revisit ownership"),
            ("leases", "#513"),
            ("reruns", "#513"),
        ] {
            let mut value = file();
            set(&mut value, option, json!(false));
            let error = Config::parse(&value.to_string()).unwrap_err().to_string();
            assert!(
                error.starts_with("unsupported_capability:"),
                "{option}: {error}"
            );
            assert!(error.contains(follow_up), "{option}: {error}");
        }
        for variable in ["GH_TOKEN", "GITHUB_TOKEN", "DISPATCH_TOKEN"] {
            let mut value = file();
            value["board"]["tokenEnv"] = variable.into();
            assert!(Config::parse(&value.to_string()).is_err());
        }
        let mut github = file();
        github["board"] = json!({"owner": "example", "project": 1});
        assert!(Config::parse(&github.to_string()).is_err());
        github["usage"]["repository"] = "example/private".into();
        github["usage"]["issue"] = 1.into();
        assert_eq!(
            config(&github).github_board().unwrap().priority_field,
            "Priority"
        );
    }

    struct Fake {
        calls: RefCell<Vec<String>>,
        bot: Value,
        operator: Value,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                calls: RefCell::default(),
                bot: json!({"login": "bot", "id": 1234}),
                operator: json!({"login": "operator", "id": 5678}),
            }
        }
    }

    impl ForgejoTransport for Fake {
        fn get(&self, url: &url::Url) -> Result<Response> {
            self.calls.borrow_mut().push(url.to_string());
            let body = match url.path() {
                "/subpath/api/v1/user" => self.bot.clone(),
                "/subpath/api/v1/users/operator" => self.operator.clone(),
                other => panic!("unexpected forge read {other}"),
            };
            ensure!(body.is_object(), "identity unreadable");
            Ok(Response { body, total: None })
        }

        fn send(&self, _: &str, _: &url::Url, _: &Value) -> Result<(u16, Value)> {
            panic!("preview wrote")
        }
    }

    struct BrokerFake {
        calls: RefCell<Vec<String>>,
        usage: Option<Value>,
        grant: Option<Value>,
    }

    impl Default for BrokerFake {
        fn default() -> Self {
            Self {
                calls: RefCell::default(),
                usage: Some(usage::tests::broker()),
                grant: Some(usage::tests::grant()),
            }
        }
    }

    impl Broker for BrokerFake {
        fn usage(&self, url: &str) -> Result<Value> {
            assert_eq!(url, "http://broker.internal/usage");
            self.calls.borrow_mut().push("usage".into());
            self.usage.clone().context("usage unavailable")
        }

        fn grant(&self, url: &str) -> Result<Value> {
            assert_eq!(url, "http://broker.internal/grant");
            self.calls.borrow_mut().push("grant".into());
            self.grant.clone().context("grant unavailable")
        }
    }

    fn preview_config() -> Config {
        let mut value = file();
        value["board"]["url"] = "https://forge.example:8443/subpath".into();
        config(&value)
    }

    fn preview(fake: &Fake, broker: &BrokerFake, config: &Config) -> Report {
        run(
            fake,
            broker,
            config,
            (Mode::Preview, Limits::default()),
            usage::tests::now(),
        )
        .unwrap()
    }

    #[test]
    fn fake_transport_preview_output() {
        let (fake, broker, config) = (Fake::default(), BrokerFake::default(), preview_config());
        let report = preview(&fake, &broker, &config);
        assert_eq!(report.failures, 0);
        assert_eq!(report.spent.requests, 3);
        assert_eq!(report.lines.len(), 7);
        assert!(report.lines[0].starts_with("identity: bot bot (id 1234)"));
        let snapshot = usage::reading(&usage::tests::broker(), &config, None, usage::tests::now())
            .unwrap()
            .1;
        assert_eq!(
            report.lines[1],
            format!("usage: {}", usage::pools(&snapshot).join("; "))
        );
        for (stage, line) in (2..=6).zip(&report.lines[2..]) {
            assert!(
                line.starts_with(&format!("stage {stage} "))
                    && line.ends_with("not built on Forgejo yet")
            );
        }
        assert_eq!(
            *fake.calls.borrow(),
            [
                "https://forge.example:8443/subpath/api/v1/user",
                "https://forge.example:8443/subpath/api/v1/users/operator"
            ]
        );
        for line in &report.lines {
            println!("{line}");
        }
        println!("{}", report.summary_line());
    }

    #[test]
    fn identity_failures_stop_immediately() {
        for (bot, operator, reads, diagnostic) in [
            (
                json!({"login": "visitor", "id": 1234}),
                json!({}),
                1,
                "login",
            ),
            (json!({"login": "bot", "id": 999}), json!({}), 1, "bot id"),
            (Value::Null, json!({}), 1, "unreadable"),
            (json!({"login": "bot"}), json!({}), 1, "user id"),
            (
                json!({"login": "bot", "id": 1234}),
                json!({"login": "operator", "id": 999}),
                2,
                "reused login",
            ),
            (
                json!({"login": "bot", "id": 1234}),
                Value::Null,
                2,
                "unreadable",
            ),
            (
                json!({"login": "bot", "id": 1234}),
                json!({"login": "other", "id": 5678}),
                2,
                "different login",
            ),
        ] {
            let fake = Fake {
                bot,
                operator,
                ..Fake::default()
            };
            let broker = BrokerFake::default();
            let report = preview(&fake, &broker, &preview_config());
            assert_eq!(report.failures, 1);
            assert_eq!(report.lines.len(), 1);
            assert!(report.lines[0].contains(diagnostic), "{:?}", report.lines);
            assert_eq!(fake.calls.borrow().len(), reads);
            assert!(broker.calls.borrow().is_empty());
        }
    }

    #[test]
    fn broker_failure_missing_readings_and_grants_never_read_forge() {
        for (has_usage, grant, fails) in [
            (true, false, false),
            (false, false, false),
            (true, true, false),
            (false, true, false),
            (true, true, true),
            (false, true, true),
        ] {
            let fake = Fake::default();
            let mut broker = BrokerFake::default();
            if !has_usage {
                broker.usage = None;
            }
            if fails {
                broker.grant = None;
            }
            let mut value = file();
            value["board"]["url"] = "https://forge.example/subpath".into();
            if grant {
                value["budget"] = json!({"source": "broker", "url": "http://broker.internal/grant",
                    "pools": {"claude": "anthropic", "openai": "codex"}});
            }
            let report = preview(&fake, &broker, &config(&value));
            assert_eq!(fake.calls.borrow().len(), 2);
            assert_eq!(
                report.failures,
                usize::from(!has_usage) + usize::from(fails)
            );
            let line = report
                .lines
                .iter()
                .find(|l| l.starts_with("usage: "))
                .unwrap();
            if grant && !fails {
                assert!(line.contains("granted"), "{line}");
            }
            if grant && fails {
                assert!(line.contains("no-grant"), "{line}");
            }
            if !has_usage && !grant {
                assert!(line.contains("no reading (held)"));
            }
        }
        let fake = Fake::default();
        let broker = BrokerFake {
            usage: Some(json!({"schema": "praxis-broker-usage/v1"})),
            ..BrokerFake::default()
        };
        assert!(preview(&fake, &broker, &preview_config()).lines[1].contains("no reading (held)"));
        let mut value = file();
        value.as_object_mut().unwrap().remove("usage");
        value.as_object_mut().unwrap().remove("pools");
        value["board"]["url"] = "https://forge.example/subpath".into();
        let broker = BrokerFake::default();
        let report = preview(&Fake::default(), &broker, &config(&value));
        assert!(report.lines[1].contains("no usage.url"));
        assert!(broker.calls.borrow().is_empty());
    }

    #[test]
    fn shared_bounds_and_apply_refusal() {
        let config = preview_config();
        for (limit, deadline, expected) in [
            (
                PASS_REQUEST_LIMIT,
                Instant::now() - Duration::from_secs(1),
                0,
            ),
            (WRITE_RESERVE, Instant::now() + Duration::from_secs(60), 0),
            (
                WRITE_RESERVE + 1,
                Instant::now() + Duration::from_secs(60),
                1,
            ),
            (
                WRITE_RESERVE + 2,
                Instant::now() + Duration::from_secs(60),
                2,
            ),
        ] {
            let fake = Fake::default();
            let broker = BrokerFake::default();
            let bounds = Bounds::new(limit, deadline);
            let report = run_within(
                &fake,
                &broker,
                &config,
                (Mode::Preview, Limits::default()),
                usage::tests::now(),
                &bounds,
            )
            .unwrap();
            assert_eq!(fake.calls.borrow().len(), expected);
            assert!(broker.calls.borrow().is_empty());
            assert!(report.failures > 0);
            assert_eq!(report.spent.requests, expected);
        }
        let fake = Fake::default();
        assert!(
            run(
                &fake,
                &BrokerFake::default(),
                &config,
                (Mode::Local, Limits::default()),
                usage::tests::now()
            )
            .is_err()
        );
        assert!(fake.calls.borrow().is_empty());
    }
}
