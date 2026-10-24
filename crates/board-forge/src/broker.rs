//! The broker probe, moved from tracker's `.github/poll/broker.cjs`
//! (tracker#603): probes the inference broker, TCP then HTTP, and keeps one
//! outage issue open while it is unreachable, for a person to see and for
//! poll's `brokerOutage` to read. The pass's own failed `/usage` read is one
//! line of its log, not an issue. Without `apply` it reads and prints each
//! write as `would: ...`.
use crate::github::{http_agent, is_github_login};
use crate::poll::{OUTAGE_MARKER, check_keys, validate_repository};
use crate::release_train::{Api, Http};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

/// Each of the TCP connection and the HTTP answer, as broker.cjs waited.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// One page of open outage issues; a full one stops the probe rather than
/// open a second issue beside one it did not read.
const ISSUE_PAGE: usize = 100;
const KEYS: &[&str] = &["url", "repository", "label"];

/// The probe's configuration file, from the caller's default branch.
#[derive(Clone, Debug)]
pub struct Config {
    /// Any address on the broker; only whether it answers is read.
    pub url: Url,
    /// Where the outage issue is.
    pub repository: String,
    /// The outage issue's label, created if missing. Poll's `brokerOutage`
    /// reads `outage`.
    pub label: String,
}

impl Config {
    pub fn from_json(value: &Value) -> Result<Self> {
        let what = "broker config";
        check_keys(&[KEYS], value, what)?;
        let text = |key| {
            value[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .with_context(|| format!("{what} needs {key}, a string"))
        };
        // Not printed: the address may be private.
        let url = Url::parse(text("url")?).context("url is not an address")?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none()
                && url.host().is_some(),
            "url must be HTTP(S) with a host and without credentials"
        );
        let repository = text("repository")?.to_owned();
        validate_repository(&repository)?;
        let label = text("label")?.to_owned();
        ensure!(
            label.len() <= 50 && !label.contains(','),
            "invalid label {label:?}: at most 50 characters, without a comma"
        );
        Ok(Self {
            url,
            repository,
            label,
        })
    }
}

/// Whether the broker answers at `url`: a TCP connection, then one GET
/// without the token, redirects or an environment proxy, whose status is not
/// read (a redirect, 401, 404 or 500 are answers too).
pub fn probe(url: &Url, timeout: Duration) -> bool {
    let connects = url.socket_addrs(|| None).is_ok_and(|addrs| {
        addrs
            .iter()
            .any(|addr| TcpStream::connect_timeout(addr, timeout).is_ok())
    });
    connects
        && http_agent()
            .get(url.as_str())
            .header("User-Agent", "agent-board")
            .config()
            .max_redirects(0)
            .max_redirects_will_error(false)
            .proxy(None)
            .timeout_global(Some(timeout))
            .build()
            .call()
            .is_ok()
}

/// The bot's open outage issue, if any. Anyone who can open an issue can add
/// the label and the marker, so only `login`'s count.
fn find(api: &impl Api, config: &Config, login: &str) -> Result<Option<u64>> {
    let label: String = url::form_urlencoded::byte_serialize(config.label.as_bytes()).collect();
    let path = format!(
        "/repos/{}/issues?state=open&labels={label}&per_page={ISSUE_PAGE}",
        config.repository
    );
    let issues = api.request("GET", &path, &Value::Null)?;
    let issues = issues.as_array().context("the issue list is not a list")?;
    ensure!(
        issues.len() < ISSUE_PAGE,
        "{ISSUE_PAGE} open {} issues: the scan reached its bound",
        config.label
    );
    Ok(issues
        .iter()
        .find(|i| {
            i.get("pull_request").is_none()
                && i["user"]["login"]
                    .as_str()
                    .is_some_and(|l| l.eq_ignore_ascii_case(login))
                && i["body"]
                    .as_str()
                    .is_some_and(|b| b.starts_with(OUTAGE_MARKER))
        })
        .and_then(|i| i["number"].as_u64()))
}

/// Opens, refreshes or closes the outage issue for the probe's answer `up`,
/// then fails if the broker is unreachable, so the run says so too (the
/// supervisor's `broker` check reads the probe's runs).
pub fn publish(
    api: &impl Api,
    config: &Config,
    up: bool,
    apply: bool,
    now: OffsetDateTime,
    out: &mut impl Write,
) -> Result<()> {
    writeln!(
        out,
        "broker: {}",
        if up { "reachable" } else { "unreachable" }
    )?;
    let account = api.request("GET", "/user", &Value::Null)?;
    let login = account["login"]
        .as_str()
        .filter(|l| is_github_login(l))
        .context("the token's account has no login")?;
    let issue = find(api, config, login)?;
    let now = now.format(&Rfc3339)?;
    let path = format!("/repos/{}/issues", config.repository);
    let state = if up { "reachable again" } else { "unreachable" };
    let body = format!(
        "{OUTAGE_MARKER}\nBroker {state} from the probe at {now}.\nTCP plus one unauthenticated HTTP request; any HTTP response counts as reachable."
    );
    let mut writes: Vec<(String, &str, String, Value)> = vec![];
    match (up, issue) {
        (true, None) => {}
        // The close first: a probe that failed between the two would
        // otherwise say so again each run; a closed issue is not found again.
        (true, Some(n)) => {
            writes.push((
                format!("close {}#{n}", config.repository),
                "PATCH",
                format!("{path}/{n}"),
                json!({"state": "closed", "state_reason": "completed"}),
            ));
            writes.push((
                format!("comment on {}#{n}", config.repository),
                "POST",
                format!("{path}/{n}/comments"),
                json!({"body": format!("Broker reachable again at {now}.")}),
            ));
        }
        (false, Some(n)) => writes.push((
            format!("refresh {}#{n}", config.repository),
            "PATCH",
            format!("{path}/{n}"),
            json!({"body": body}),
        )),
        (false, None) => {
            writes.push((
                format!("create label {} in {}", config.label, config.repository),
                "POST",
                format!("/repos/{}/labels", config.repository),
                json!({"name": config.label, "color": "b60205", "description": "Infrastructure unavailable"}),
            ));
            writes.push((
                format!("open an outage issue in {}", config.repository),
                "POST",
                path,
                json!({"title": "Inference broker unreachable", "body": body, "labels": [config.label]}),
            ));
        }
    }
    for (said, method, path, body) in writes {
        if !apply {
            writeln!(out, "would: {said}")?;
            continue;
        }
        match api.request(method, &path, &body) {
            Ok(_) => writeln!(out, "{said}")?,
            // An existing label.
            Err(e)
                if path.ends_with("/labels")
                    && e.downcast_ref::<Http>().is_some_and(|h| h.status == 422) => {}
            Err(e) => return Err(e.context(said)),
        }
    }
    if !up {
        bail!("the broker is unreachable");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Read;
    use std::net::TcpListener;

    const FIXTURE: &str = include_str!("../../../fixtures/broker/issues.json");

    fn config() -> Config {
        Config::from_json(&json!({
            "url": "http://127.0.0.1:1/",
            "repository": "example/tracker",
            "label": "outage",
        }))
        .unwrap()
    }

    /// GitHub as the fixture has it: `issues` answers the issue list, and
    /// each write is recorded and answered, 422 for `refused` paths.
    struct Fake {
        issues: Value,
        refused: &'static str,
        writes: RefCell<Vec<String>>,
    }

    impl Api for Fake {
        fn request(&self, method: &str, path: &str, _: &Value) -> Result<Value> {
            match (method, path) {
                ("GET", "/user") => Ok(json!({"login": "example-bot"})),
                ("GET", p) if p.starts_with("/repos/example/tracker/issues?") => {
                    assert!(p.contains("labels=outage&"), "{p}");
                    Ok(self.issues.clone())
                }
                ("GET", p) => panic!("unexpected GET {p}"),
                (m, p) => {
                    self.writes.borrow_mut().push(format!("{m} {p}"));
                    if p == self.refused {
                        return Err(Http {
                            method: m.into(),
                            path: p.into(),
                            status: 422,
                        }
                        .into());
                    }
                    Ok(json!({}))
                }
            }
        }
    }

    #[test]
    fn opens_refreshes_and_closes_only_the_bots_issue() {
        let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
        let now = OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap();
        // The fixture's open issue #7 is the bot's; #8 has the marker but is
        // another user's, #9 is a pull request and #10 lacks the marker.
        let open = fixture["open"].clone();
        let others = fixture["others"].clone();
        for (issues, up, refused, writes, error) in [
            (
                &open,
                true,
                "",
                &[
                    "PATCH /repos/example/tracker/issues/7",
                    "POST /repos/example/tracker/issues/7/comments",
                ][..],
                None,
            ),
            (
                &open,
                false,
                "",
                &["PATCH /repos/example/tracker/issues/7"][..],
                Some("unreachable"),
            ),
            (&others, true, "", &[][..], None),
            (
                &others,
                false,
                "/repos/example/tracker/labels",
                &[
                    "POST /repos/example/tracker/labels",
                    "POST /repos/example/tracker/issues",
                ][..],
                Some("unreachable"),
            ),
            (
                &others,
                false,
                "/repos/example/tracker/issues",
                &[
                    "POST /repos/example/tracker/labels",
                    "POST /repos/example/tracker/issues",
                ][..],
                Some("open an outage issue"),
            ),
        ] {
            for apply in [false, true] {
                let fake = Fake {
                    issues: issues.clone(),
                    refused,
                    writes: RefCell::default(),
                };
                let mut out = Vec::new();
                let result = publish(&fake, &config(), up, apply, now, &mut out);
                let out = String::from_utf8(out).unwrap();
                // A preview writes nothing to refuse.
                match error.map(|e| if apply { e } else { "unreachable" }) {
                    Some(e) => {
                        let message = format!("{:#}", result.unwrap_err());
                        assert!(message.contains(e), "{message}");
                    }
                    None => result.unwrap(),
                }
                let expected: &[&str] = if apply { writes } else { &[] };
                assert_eq!(*fake.writes.borrow(), expected, "{out}");
                if !apply {
                    assert_eq!(out.matches("would: ").count(), writes.len(), "{out}");
                }
            }
        }
    }

    #[test]
    fn a_full_page_of_issues_stops_the_probe() {
        let issue = json!({"number": 1, "user": {"login": "someone"}, "body": ""});
        let fake = Fake {
            issues: Value::Array(vec![issue; ISSUE_PAGE]),
            refused: "",
            writes: RefCell::default(),
        };
        let now = OffsetDateTime::UNIX_EPOCH;
        let error = publish(&fake, &config(), false, true, now, &mut Vec::new()).unwrap_err();
        assert!(
            format!("{error:#}").contains("reached its bound"),
            "{error:#}"
        );
        assert!(fake.writes.borrow().is_empty());
    }

    #[test]
    fn config_is_checked() {
        let base = json!({"url": "http://broker.example:8080/usage", "repository": "example/tracker", "label": "outage"});
        assert!(Config::from_json(&base).is_ok());
        for (key, value, expected) in [
            ("url", json!("ftp://broker.example/"), "HTTP(S)"),
            (
                "url",
                json!("http://user:secret@broker.example/"),
                "without credentials",
            ),
            ("url", json!("not an address"), "not an address"),
            ("repository", json!("example"), "repository"),
            ("label", json!("a,b"), "invalid label"),
            ("label", json!(""), "needs label"),
            ("extra", json!(true), "unknown broker config key extra"),
        ] {
            let mut config = base.clone();
            config[key] = value;
            let error = Config::from_json(&config).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
    }

    /// A listener on a free local port that answers each connection with
    /// `answer`, or holds it unanswered if `answer` is empty.
    fn listener(answer: &'static str) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/usage", listener.local_addr().unwrap())).unwrap();
        std::thread::spawn(move || {
            let mut held = vec![];
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if answer.is_empty() {
                    held.push(stream);
                    continue;
                }
                let mut request = [0; 1024];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(answer.as_bytes());
            }
        });
        url
    }

    #[test]
    fn any_http_answer_is_reachable() {
        let timeout = Duration::from_secs(2);
        let closed = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            Url::parse(&format!("http://{addr}/")).unwrap()
        };
        for (url, up) in [
            (
                listener("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"),
                true,
            ),
            (
                listener("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n"),
                true,
            ),
            // Not followed: the address it names is closed.
            (
                listener(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\n\r\n",
                ),
                true,
            ),
            // TCP accepts, but nothing answers.
            (listener(""), false),
            (closed, false),
        ] {
            assert_eq!(probe(&url, timeout), up, "{url}");
        }
    }
}
