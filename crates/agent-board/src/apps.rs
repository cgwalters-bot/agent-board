//! `apps create`: register a role's GitHub App through GitHub's manifest flow
//! (docs/apps.md#creating-the-apps). GitHub has no API that creates an app
//! from a token, so the operator clicks **Create GitHub App** once; the rest
//! is a localhost page that posts the manifest, a localhost callback that
//! takes the one-time code, and its conversion into the app's credentials.
//! Prior art: fullsend's internal/appsetup/appsetup.go, plus the `state`
//! check it lacks.

use anyhow::{Context, Result, bail, ensure};
use board_forge::github::{API_ROOT, Client, NotFound, is_github_login};
use board_forge::poll::Rest;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The roles of docs/apps.md, used without `--config`.
pub const DEFAULT_ROLES: &str = include_str!("../../../policy/apps.json");
/// How long the callback waits for the operator's click. GitHub's code
/// itself is valid for an hour.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Requests the localhost server answers before giving up: the page, the
/// callback, and a browser's stray favicon or retry requests.
const CALLBACK_REQUEST_LIMIT: usize = 32;
/// The bytes of a request head the server reads; it needs only the request line.
const REQUEST_HEAD_LIMIT: usize = 8192;
/// GitHub's limit on an app's name.
const APP_NAME_LIMIT: usize = 34;

#[derive(clap::Args)]
pub struct Create {
    /// The organization that will own the app.
    #[arg(long)]
    org: String,
    /// The role in the roles config whose app this is.
    #[arg(long)]
    role: String,
    /// The roles config (JSON); defaults to the roles compiled from this repository.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Write the new app's private key to FILE (created with mode 0600; an existing file is refused).
    #[arg(long, value_name = "FILE", conflicts_with = "key_stdout")]
    key_file: Option<PathBuf>,
    /// Print the new app's private key on stdout, and everything else on stderr.
    #[arg(long)]
    key_stdout: bool,
    /// The role's app exists and its private key is already stored: reuse it instead of creating one.
    #[arg(long, conflicts_with_all = ["key_file", "key_stdout"])]
    key_stored: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Roles {
    roles: BTreeMap<String, Role>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Role {
    /// The app's name; GitHub derives its slug from it.
    name: String,
    /// Set when GitHub's slug differs from the one derived from the name.
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    description: String,
    /// The app's homepage; defaults to the organization's.
    #[serde(default)]
    url: Option<String>,
    /// Public only lets other accounts install it (docs/apps.md, Installations).
    #[serde(default)]
    public: bool,
    permissions: BTreeMap<String, String>,
    events: Vec<String>,
}

fn is_identifier(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|c| c.is_ascii_lowercase() || c == b'_')
}

fn is_slug(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('-')
        && text
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

impl Role {
    fn validate(&self, key: &str) -> Result<()> {
        ensure!(
            (1..=APP_NAME_LIMIT).contains(&self.name.chars().count()),
            "role {key}: name must be 1 to {APP_NAME_LIMIT} characters"
        );
        ensure!(
            is_slug(&self.slug()),
            "role {key}: slug {:?} is not a GitHub App slug",
            self.slug()
        );
        if let Some(url) = &self.url {
            ensure!(url.starts_with("https://"), "role {key}: url must be https");
        }
        for (name, access) in &self.permissions {
            ensure!(
                is_identifier(name),
                "role {key}: permission {name:?} is not a permission name"
            );
            ensure!(
                ["read", "write"].contains(&access.as_str()),
                "role {key}: permission {name} must be read or write"
            );
        }
        for event in &self.events {
            ensure!(
                is_identifier(event),
                "role {key}: event {event:?} is not an event name"
            );
        }
        Ok(())
    }

    /// GitHub lowercases the name and turns other characters into hyphens.
    fn slug(&self) -> String {
        self.slug.clone().unwrap_or_else(|| {
            self.name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect()
        })
    }
}

fn load(text: &str, role: &str) -> Result<Role> {
    let mut roles: Roles = serde_json::from_str(text).context("decode roles config")?;
    for (key, value) in &roles.roles {
        ensure!(
            is_slug(key),
            "role name {key:?} must be lowercase letters, digits and hyphens"
        );
        value.validate(key)?;
    }
    roles
        .roles
        .remove(role)
        .with_context(|| format!("role {role} is not in the roles config"))
}

/// The manifest GitHub's form takes
/// (docs.github.com/en/apps/sharing-github-apps/registering-a-github-app-from-a-manifest).
/// The webhook is inactive: no role here receives events.
fn manifest(org: &str, role: &Role, redirect_url: &str) -> Value {
    let url = role
        .url
        .clone()
        .unwrap_or_else(|| format!("https://github.com/{org}"));
    json!({
        "name": role.name,
        "url": url,
        "description": role.description,
        "public": role.public,
        "redirect_url": redirect_url,
        "hook_attributes": {"url": url, "active": false},
        "default_permissions": role.permissions,
        "default_events": role.events,
        "request_oauth_on_install": false,
    })
}

fn html_escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&#39;".into(),
            c => c.to_string(),
        })
        .collect()
}

/// The localhost page that posts the manifest to GitHub. The manifest's JSON
/// goes in a form field, so the browser, not this process, holds the
/// operator's GitHub session.
fn page(org: &str, manifest: &Value, state: &str) -> String {
    let action = format!("https://github.com/organizations/{org}/settings/apps/new?state={state}");
    format!(
        "<!DOCTYPE html><html><body>\
         <form id=\"manifest\" method=\"post\" action=\"{}\">\
         <input type=\"hidden\" name=\"manifest\" value=\"{}\">\
         <button type=\"submit\">Continue to GitHub</button></form>\
         <script>document.getElementById('manifest').submit();</script>\
         </body></html>",
        html_escape(&action),
        html_escape(&manifest.to_string())
    )
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .context("answer the browser")
}

/// The request path of one connection, or none when it sent no request line.
fn request_path(stream: &mut TcpStream) -> Result<Option<String>> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut head = Vec::new();
    let mut buffer = [0; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < REQUEST_HEAD_LIMIT {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buffer[..n]),
        }
    }
    let head = String::from_utf8_lossy(&head);
    let mut parts = head.lines().next().unwrap_or_default().split(' ');
    Ok(match (parts.next(), parts.next()) {
        (Some("GET"), Some(path)) => Some(path.to_owned()),
        _ => None,
    })
}

/// Serve the page at `/` and wait for GitHub's redirect to `/callback`,
/// returning its code. A callback whose `state` is not ours ends the flow:
/// it would be another page (any site the browser visits can send one to
/// localhost) handing us the code of an app it made, whose key we would then
/// tell the operator to store and install.
fn serve(listener: &TcpListener, page: &str, state: &str, timeout: Duration) -> Result<String> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    let mut requests = 0;
    while requests < CALLBACK_REQUEST_LIMIT {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                ensure!(
                    Instant::now() < deadline,
                    "no callback from GitHub within {} minutes",
                    timeout.as_secs() / 60
                );
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(error) => return Err(error).context("accept a localhost connection"),
        };
        stream.set_nonblocking(false)?;
        requests += 1;
        let Some(path) = request_path(&mut stream)? else {
            continue;
        };
        if path == "/" {
            respond(&mut stream, "200 OK", page)?;
            continue;
        }
        let Some(query) = path.strip_prefix("/callback?") else {
            respond(&mut stream, "404 Not Found", "not found")?;
            continue;
        };
        let parameters: BTreeMap<&str, &str> =
            query.split('&').filter_map(|p| p.split_once('=')).collect();
        if parameters.get("state") != Some(&state) {
            respond(&mut stream, "400 Bad Request", "state mismatch")?;
            bail!("refused a callback whose state is not this run's; no app was converted");
        }
        let code = parameters.get("code").copied().unwrap_or_default();
        respond(
            &mut stream,
            "200 OK",
            "GitHub App created; return to the terminal.",
        )?;
        return Ok(code.to_owned());
    }
    bail!("the localhost server answered {CALLBACK_REQUEST_LIMIT} requests without a callback")
}

/// What the operator needs from a conversion.
struct Created {
    id: u64,
    slug: String,
    client_id: String,
    pem: String,
}

/// Exchange the callback's code for the app's credentials. The response's
/// client secret and webhook secret are dropped: nothing here uses them, and
/// both can be regenerated in the app's settings.
fn convert(rest: &impl Rest, code: &str, org: &str) -> Result<Created> {
    // The code goes into the URL path.
    ensure!(
        !code.is_empty() && code.bytes().all(|c| c.is_ascii_alphanumeric()),
        "the callback's code is not a manifest code"
    );
    let (status, body) = rest.send(
        "POST",
        &format!("{API_ROOT}app-manifests/{code}/conversions"),
        &json!({}),
    )?;
    ensure!(
        status == 201,
        "POST /app-manifests/CODE/conversions: GitHub HTTP status {status}: {}",
        body["message"].as_str().unwrap_or("message unavailable")
    );
    let owner = body["owner"]["login"].as_str().unwrap_or_default();
    ensure!(
        owner.eq_ignore_ascii_case(org),
        "the converted app is owned by {owner:?}, not {org}"
    );
    let id = body["id"]
        .as_u64()
        .context("conversion without an app id")?;
    let slug = body["slug"]
        .as_str()
        .filter(|s| is_slug(s))
        .context("conversion without an app slug")?;
    let pem = body["pem"]
        .as_str()
        .filter(|p| p.starts_with("-----BEGIN ") && p.contains("PRIVATE KEY-----"))
        .with_context(|| format!("app {slug} ({id}) was created but its conversion returned no private key; generate one in its settings"))?;
    Ok(Created {
        id,
        slug: slug.to_owned(),
        client_id: body["client_id"].as_str().unwrap_or_default().to_owned(),
        pem: pem.to_owned(),
    })
}

/// The role's existing app, if GitHub shows one. Without a token GitHub
/// shows only public apps.
fn lookup(rest: &impl Rest, slug: &str) -> Result<Option<Value>> {
    match rest.get(&format!("{API_ROOT}apps/{slug}"), None) {
        Ok(response) => Ok(Some(response.body)),
        Err(error) if error.downcast_ref::<NotFound>().is_some() => Ok(None),
        Err(error) => Err(error).context(format!("look up app {slug}")),
    }
}

/// Reuse the existing app when the operator says its key is stored (its
/// id), create one when there is none (`None`), and stop otherwise. An app
/// owned by another account is never reused: the operator would install a
/// stranger's app.
fn reuse(existing: Option<&Value>, key_stored: bool, org: &str, slug: &str) -> Result<Option<u64>> {
    let Some(app) = existing else {
        ensure!(
            !key_stored,
            "--key-stored, but GitHub shows no app {slug} (a private app needs GH_TOKEN to be seen)"
        );
        return Ok(None);
    };
    let owner = app["owner"]["login"].as_str().unwrap_or_default();
    ensure!(
        owner.eq_ignore_ascii_case(org),
        "app {slug} is owned by {owner:?}, not {org}; choose another name in the roles config"
    );
    ensure!(
        key_stored,
        "app {slug} already exists and its private key is lost unless stored: generate a new key at https://github.com/organizations/{org}/settings/apps/{slug}, store it, and rerun with --key-stored"
    );
    app["id"]
        .as_u64()
        .map(Some)
        .context("GitHub's app has no id")
}

/// Where the operator stores the app's id and key: the names the workflows read.
fn names(role: &str) -> (String, String) {
    let prefix = role.to_ascii_uppercase().replace('-', "_");
    (
        format!("{prefix}_APP_ID"),
        format!("{prefix}_APP_PRIVATE_KEY"),
    )
}

fn report(out: &mut dyn Write, org: &str, role: &str, id: u64, slug: &str) -> Result<()> {
    let (variable, secret) = names(role);
    writeln!(out, "app id: {id}")?;
    writeln!(out, "slug: {slug}")?;
    writeln!(
        out,
        "settings: https://github.com/organizations/{org}/settings/apps/{slug}"
    )?;
    writeln!(
        out,
        "install: https://github.com/apps/{slug}/installations/new"
    )?;
    writeln!(out, "store: variable {variable}, secret {secret}")?;
    Ok(())
}

/// The key file, created before the flow so that a path that cannot be
/// written fails before GitHub returns a key it never returns again.
fn open_key(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .with_context(|| format!("create key file {} (it must not exist)", path.display()))
}

fn random_state() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("generate state"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn flow(rest: &impl Rest, org: &str, role: &Role) -> Result<Created> {
    let listener = TcpListener::bind("127.0.0.1:0").context("bind a localhost port")?;
    let port = listener.local_addr()?.port();
    let state = random_state()?;
    let manifest = manifest(org, role, &format!("http://127.0.0.1:{port}/callback"));
    eprintln!(
        "Open http://127.0.0.1:{port}/ in a browser signed in as an owner of {org}, and click Create GitHub App."
    );
    let code = serve(
        &listener,
        &page(org, &manifest, &state),
        &state,
        CALLBACK_TIMEOUT,
    )?;
    convert(rest, &code, org)
}

pub fn create(args: &Create) -> Result<()> {
    ensure!(
        is_github_login(&args.org),
        "--org {:?} is not an organization login",
        args.org
    );
    let text = match &args.config {
        Some(path) => {
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
        }
        None => DEFAULT_ROLES.to_owned(),
    };
    let role = load(&text, &args.role)?;
    ensure!(
        args.key_stored || args.key_file.is_some() || args.key_stdout,
        "GitHub returns a new app's private key only once: pass --key-file FILE or --key-stdout"
    );
    // The lookup sees private apps only with the operator's token; neither
    // request needs one otherwise.
    let rest = Client::from_var("GH_TOKEN")
        .or_else(|| Client::from_var("GITHUB_TOKEN"))
        .unwrap_or_else(|| {
            eprintln!("No GH_TOKEN: an existing private app will not be found.");
            Client::anonymous()
        });
    let slug = role.slug();
    if let Some(id) = reuse(
        lookup(&rest, &slug)?.as_ref(),
        args.key_stored,
        &args.org,
        &slug,
    )? {
        eprintln!("Reusing app {slug}; its stored key is unchanged.");
        return report(&mut std::io::stdout(), &args.org, &args.role, id, &slug);
    }
    let mut file = args.key_file.as_deref().map(open_key).transpose()?;
    let created = match flow(&rest, &args.org, &role) {
        Ok(created) => created,
        Err(error) => {
            if let Some(path) = &args.key_file {
                let _ = std::fs::remove_file(path);
            }
            return Err(error);
        }
    };
    let mut stdout = std::io::stdout();
    match (&mut file, &args.key_file) {
        (Some(file), Some(path)) => {
            let written = file
                .write_all(created.pem.as_bytes())
                .and_then(|()| file.sync_all());
            if written.is_err() {
                report(
                    &mut std::io::stderr(),
                    &args.org,
                    &args.role,
                    created.id,
                    &created.slug,
                )?;
                bail!(
                    "writing the key to {} failed; generate a new key in the app's settings",
                    path.display()
                );
            }
            eprintln!("Wrote the private key to {}.", path.display());
            report(
                &mut stdout,
                &args.org,
                &args.role,
                created.id,
                &created.slug,
            )?;
        }
        _ => {
            stdout.write_all(created.pem.as_bytes())?;
            report(
                &mut std::io::stderr(),
                &args.org,
                &args.role,
                created.id,
                &created.slug,
            )?;
        }
    }
    if !created.client_id.is_empty() {
        eprintln!("client id (for the device flow): {}", created.client_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use board_forge::poll::RestResponse;
    use std::cell::RefCell;

    /// Built from parts: a key literal must not appear in the tree.
    fn pem() -> String {
        [
            "-----BEGIN RSA ",
            "PRIVATE KEY-----\nAAAA\n-----END RSA ",
            "PRIVATE KEY-----\n",
        ]
        .concat()
    }

    struct Fake {
        get: Option<Value>,
        send: (u16, Value),
        requests: RefCell<Vec<String>>,
    }

    impl Rest for Fake {
        fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
            assert!(validator.is_none());
            self.requests.borrow_mut().push(format!("GET {url}"));
            let body = self.get.clone().ok_or(NotFound)?;
            Ok(RestResponse {
                status: 200,
                etag: None,
                last_modified: None,
                link: None,
                body,
            })
        }

        fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
            assert_eq!(body, &json!({}));
            self.requests.borrow_mut().push(format!("{method} {url}"));
            Ok(self.send.clone())
        }
    }

    fn fake(get: Option<Value>, send: (u16, Value)) -> Fake {
        Fake {
            get,
            send,
            requests: RefCell::new(vec![]),
        }
    }

    #[test]
    fn default_roles_build_manifests() {
        for role in ["status", "review", "implement", "executor"] {
            load(DEFAULT_ROLES, role).unwrap();
        }
        let role = load(DEFAULT_ROLES, "review").unwrap();
        assert_eq!(role.slug(), "cgwalters-forge-review");
        assert_eq!(
            manifest("example", &role, "http://127.0.0.1:9/callback"),
            json!({
                "name": "cgwalters-forge-review",
                "url": "https://github.com/example",
                "description": role.description,
                "public": false,
                "redirect_url": "http://127.0.0.1:9/callback",
                "hook_attributes": {"url": "https://github.com/example", "active": false},
                "default_permissions": {"pull_requests": "write", "statuses": "write"},
                "default_events": [],
                "request_oauth_on_install": false,
            })
        );
        assert_eq!(
            names("review"),
            ("REVIEW_APP_ID".into(), "REVIEW_APP_PRIVATE_KEY".into())
        );
    }

    #[test]
    fn roles_config_refusals() {
        let base = json!({"roles":{"status":{"name":"Example Status","permissions":{"issues":"write"},"events":["issues"]}}});
        assert_eq!(
            load(&base.to_string(), "status").unwrap().slug(),
            "example-status"
        );
        for (pointer, value, message) in [
            ("/extra", json!(1), "unknown field"),
            ("/roles/status/extra", json!(1), "unknown field"),
            (
                "/roles/status/permissions/issues",
                json!("own"),
                "read or write",
            ),
            (
                "/roles/status/permissions/Issues",
                json!("read"),
                "permission name",
            ),
            ("/roles/status/events/0", json!("x y"), "event name"),
            ("/roles/status/name", json!(""), "characters"),
            ("/roles/status/name", json!("a".repeat(35)), "characters"),
            ("/roles/status/slug", json!("-x"), "slug"),
            ("/roles/status/url", json!("http://example.com"), "https"),
            (
                "/roles/Status",
                json!({"name":"x","permissions":{},"events":[]}),
                "role name",
            ),
            ("/roles/other", json!(null), "invalid type"),
        ] {
            let mut config = base.clone();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            match config.pointer_mut(parent).unwrap() {
                Value::Array(items) => items[key.parse::<usize>().unwrap()] = value,
                object => {
                    object.as_object_mut().unwrap().remove(key);
                    object[key] = value;
                }
            }
            let error = format!("{:#}", load(&config.to_string(), "status").err().unwrap());
            assert!(error.contains(message), "{pointer}: {error}");
        }
        let error = load(&base.to_string(), "review").err().unwrap();
        assert!(error.to_string().contains("not in the roles config"));
    }

    #[test]
    fn existing_apps() {
        let ours = json!({"id": 7, "slug": "x", "owner": {"login": "Example"}});
        let theirs = json!({"id": 8, "slug": "x", "owner": {"login": "other"}});
        for (existing, stored, expected) in [
            (None, false, Ok(None)),
            (None, true, Err("no app x")),
            (Some(&ours), true, Ok(Some(7))),
            (Some(&ours), false, Err("private key is lost")),
            (Some(&theirs), true, Err("owned by \"other\"")),
            (Some(&theirs), false, Err("owned by \"other\"")),
        ] {
            match (reuse(existing, stored, "example", "x"), expected) {
                (Ok(got), Ok(want)) => assert_eq!(got, want),
                (Err(error), Err(want)) => assert!(error.to_string().contains(want), "{error}"),
                (got, want) => panic!("{:?} != {want:?}", got.map_err(|e| e.to_string())),
            }
        }
        let found = fake(Some(ours.clone()), (500, json!(null)));
        assert_eq!(lookup(&found, "x").unwrap(), Some(ours));
        assert_eq!(
            found.requests.into_inner(),
            ["GET https://api.github.com/apps/x"]
        );
        assert_eq!(lookup(&fake(None, (500, json!(null))), "x").unwrap(), None);
    }

    #[test]
    fn conversion_responses() {
        let good = json!({"id": 42, "slug": "example-status", "client_id": "Iv1.abc",
            "client_secret": "s", "webhook_secret": "w", "pem": pem(), "owner": {"login": "example"}});
        let rest = fake(None, (201, good.clone()));
        let created = convert(&rest, "abc123", "example").unwrap();
        assert_eq!(
            (
                created.id,
                created.slug.as_str(),
                created.client_id.as_str()
            ),
            (42, "example-status", "Iv1.abc")
        );
        assert_eq!(created.pem, pem());
        assert_eq!(
            rest.requests.into_inner(),
            ["POST https://api.github.com/app-manifests/abc123/conversions"]
        );
        let with = |pointer: &str, value: Value| {
            let mut body = good.clone();
            *body.pointer_mut(pointer).unwrap() = value;
            body
        };
        for (code, status, body, message) in [
            (
                "abc123",
                404,
                json!({"message": "Not Found"}),
                "status 404: Not Found",
            ),
            (
                "abc123",
                201,
                with("/owner/login", json!("other")),
                "owned by",
            ),
            ("abc123", 201, with("/id", json!("42")), "app id"),
            ("abc123", 201, with("/slug", json!("../x")), "app slug"),
            (
                "abc123",
                201,
                with("/pem", json!("secret")),
                "no private key",
            ),
            ("abc123", 201, with("/pem", json!(null)), "no private key"),
        ] {
            let error = convert(&fake(None, (status, body)), code, "example")
                .err()
                .unwrap();
            assert!(error.to_string().contains(message), "{message}: {error}");
        }
        for code in ["", "../apps", "a b", "a%2f"] {
            let rest = fake(None, (201, good.clone()));
            assert!(convert(&rest, code, "example").is_err(), "{code}");
            assert!(rest.requests.into_inner().is_empty(), "{code}");
        }
    }

    fn get(port: u16, path: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn callback_checks_state() {
        let role = load(DEFAULT_ROLES, "status").unwrap();
        let manifest = manifest("example", &role, "http://127.0.0.1:9/callback");
        let page = page("example", &manifest, "s1");
        assert!(page.contains(
            "action=\"https://github.com/organizations/example/settings/apps/new?state=s1\""
        ));
        assert!(page.contains(&html_escape(&manifest.to_string())));
        assert!(!page.contains("\"name\":"));
        for (callback, expected) in [
            ("/callback?code=abc&state=s1", Ok("abc")),
            ("/callback?state=s1&code=abc", Ok("abc")),
            ("/callback?code=abc&state=s2", Err("state")),
            ("/callback?code=abc", Err("state")),
            ("/callback?code=abc&state=s1x", Err("state")),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let expected_page = page.clone();
            let client = std::thread::spawn(move || {
                let served = get(port, "/");
                assert!(served.starts_with("HTTP/1.1 200") && served.ends_with(&expected_page));
                assert!(get(port, "/favicon.ico").starts_with("HTTP/1.1 404"));
                get(port, callback)
            });
            let result = serve(&listener, &page, "s1", Duration::from_secs(10));
            let response = client.join().unwrap();
            match (result, expected) {
                (Ok(code), Ok(want)) => {
                    assert_eq!(code, want);
                    assert!(response.starts_with("HTTP/1.1 200"));
                }
                (Err(error), Err(want)) => {
                    assert!(error.to_string().contains(want), "{error}");
                    assert!(response.starts_with("HTTP/1.1 400"));
                }
                (got, want) => panic!("{callback}: {:?} != {want:?}", got.err()),
            }
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let error = serve(&listener, "", "s1", Duration::ZERO).err().unwrap();
        assert!(error.to_string().contains("no callback"));
    }

    #[test]
    fn key_file_is_private_and_new() {
        let path =
            std::env::temp_dir().join(format!("agent-board-key-{}", random_state().unwrap()));
        drop(open_key(&path).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(open_key(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
