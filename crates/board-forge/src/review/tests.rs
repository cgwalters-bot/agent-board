use super::*;
use crate::poll::RestResponse;
use std::cell::{Cell, RefCell};

const BOT: &str = "example-bot";
const CHECKBOX: &str = "Send the newest draft as example-bot";
const PULL: &str = "https://api.github.com/repos/ostreedev/ostree/pulls/3649";
const HEAD: &str = "a0207f11818b46729249315399c87fe99d5e3f21";
const REVIEW: &str = "https://github.com/ostreedev/ostree/pull/3649#pullrequestreview-1";

fn fixture(name: &str) -> Value {
    serde_json::from_str(match name {
        "issue" => include_str!("../../../../fixtures/review/issue.json"),
        "comments" => include_str!("../../../../fixtures/review/comments.json"),
        "pull" => include_str!("../../../../fixtures/review/pull-3649.json"),
        "files" => include_str!("../../../../fixtures/review/files-3649.json"),
        _ => unreachable!("{name}"),
    })
    .unwrap()
}

fn config_json() -> Value {
    json!({"operator": "operator", "bot": BOT,
        "review": {"targets": ["ostreedev/*", "containers/bootc"], "checkbox": CHECKBOX}})
}

fn config() -> Config {
    Config::from_json(&config_json()).unwrap()
}

fn draft_text() -> String {
    fixture("comments")[0]["body"].as_str().unwrap().to_owned()
}

#[test]
fn config_is_read_from_agent_board_json() {
    let config = config();
    assert_eq!(config.targets, ["ostreedev/*", "containers/bootc"]);
    assert_eq!(config.footer.as_str(), crate::FOOTER);
    // A change to the valid configuration, then the error it gives.
    for (change, expected) in [
        (json!({"bot": "Operator"}), "operator other than the bot"),
        (json!({"operator": null}), "no valid operator"),
        (json!({"review": null}), "review is not configured"),
        (
            json!({"reviews": {}}),
            "unknown agent-board config key reviews",
        ),
        (
            json!({"review": {"targets": ["ostreedev/*"], "checkbox": CHECKBOX, "extra": 1}}),
            "unknown review key extra",
        ),
        (
            json!({"review": {"targets": [], "checkbox": CHECKBOX}}),
            "targets is empty",
        ),
        (
            json!({"review": {"targets": ["ostreedev"], "checkbox": CHECKBOX}}),
            "not OWNER/* or OWNER/REPO",
        ),
        (
            json!({"review": {"targets": ["a b/*"], "checkbox": CHECKBOX}}),
            "invalid owner",
        ),
        (
            json!({"review": {"targets": "ostreedev/*", "checkbox": CHECKBOX}}),
            "not a list of strings",
        ),
        (
            json!({"review": {"targets": ["ostreedev/*"], "checkbox": "Send\nnow"}}),
            "checkbox is not one trimmed line",
        ),
        (
            json!({"review": {"targets": ["ostreedev/*"]}}),
            "checkbox is not one trimmed line",
        ),
    ] {
        let mut value = config_json();
        for (key, v) in change.as_object().unwrap() {
            if v.is_null() {
                value.as_object_mut().unwrap().remove(key);
            } else {
                value[key] = v.clone();
            }
        }
        let error = Config::from_json(&value).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
}

#[test]
fn draft_format() {
    let draft = parse(&draft_text(), CHECKBOX).unwrap();
    assert_eq!(
        (
            draft.repository.as_str(),
            draft.number,
            draft.head.as_str(),
            draft.event.as_str()
        ),
        ("ostreedev/ostree", 3649, HEAD, "REQUEST_CHANGES")
    );
    assert_eq!(
        draft.body,
        "The policy refresh can still block finalization when `semodule` hangs."
    );
    assert_eq!(
        draft.comments,
        [
            Inline {
                path: "src/libostree/ostree-sysroot-deploy.c".into(),
                line: 3415,
                body: "*Important*: this waits without a timeout.\n\n```c\nwaitpid (pid, &status, 0);\n```".into(),
            },
            Inline {
                path: "src/libostree/ostree-repo-finder-mount.c".into(),
                line: 342,
                body: "(low) Unrelated reformatting.\n\nGenerated-by: https://github.com/cgwalters/#llms".into(),
            },
        ]
    );

    let block = |header: &str| format!("```review-draft\n{header}\n```\n");
    let good = "pr = \"ostreedev/ostree#3649\"\nhead = \"a0207f11818b46729249315399c87fe99d5e3f21\"\nevent = \"COMMENT\"";
    let with = |header: &str, rest: &str| format!("{}{rest}", block(header));
    // Text, then the number of comments or the error.
    for (text, expected) in [
        (with(good, "## Review\n\nFine.\n"), Ok(0)),
        (
            with(
                good,
                "## Review\n\nFine.\n\n- [x] Send the newest draft as example-bot\n",
            ),
            Ok(0),
        ),
        (
            with(
                good,
                "## Review\n\n## Comments\n\n### `a.c:1`\n\n````md\n### `b.c:2`\n````\n",
            ),
            Ok(1),
        ),
        (
            with(
                good,
                "## Review\n\nBody\n\n### Not a comment\n\n## Comments\n",
            ),
            Ok(0),
        ),
        (
            format!("Intro\n\n{}", with(good, "\n\n## Review\nx\n")),
            Ok(0),
        ),
        ("## Review\nx\n".into(), Err("0 review-draft blocks")),
        (
            format!("````md\n{}````\n## Review\n", block(good)),
            Err("0 review-draft blocks"),
        ),
        (
            format!("{}{}", block(good), with(good, "## Review\n")),
            Err("2 review-draft blocks"),
        ),
        (
            format!("```review-draft\n{good}\n## Review\n"),
            Err("not closed"),
        ),
        (with(good, "Body\n"), Err("no ## Review section")),
        (
            with(&format!("{good}\nbase = \"main\""), "## Review\n"),
            Err("unknown review-draft field \"base\""),
        ),
        (
            with(&format!("{good}\nevent = \"APPROVE\""), "## Review\n"),
            Err("sets event twice"),
        ),
        (
            with(&good.replace("event", "# event"), "## Review\n"),
            Err("has no event"),
        ),
        (
            with(&good.replace("COMMENT", "MERGE"), "## Review\n"),
            Err("event \"MERGE\" is not one of"),
        ),
        (
            with(&good.replace("\"COMMENT\"", "COMMENT"), "## Review\n"),
            Err("event is not a quoted string"),
        ),
        (
            with(&good.replace("\"COMMENT\"", "\"COMMENT\" x"), "## Review\n"),
            Err("event is not a quoted string"),
        ),
        (
            with(&good.replace("ostree#", "ostree\\#"), "## Review\n"),
            Err("pr is not a quoted string"),
        ),
        (
            with(
                &good.replace("ostreedev/ostree#3649", "ostree#3649"),
                "## Review\n",
            ),
            Err("expected OWNER/REPO"),
        ),
        (
            with(&good.replace("#3649", "#0"), "## Review\n"),
            Err("is not OWNER/REPO#N"),
        ),
        (
            with(&good.replace("a0207f", "A0207F"), "## Review\n"),
            Err("not a full lowercase commit SHA"),
        ),
        (
            with(&good.replace("a0207f", ""), "## Review\n"),
            Err("not a full lowercase commit SHA"),
        ),
        (with("pr: x", "## Review\n"), Err("is not KEY = \"VALUE\"")),
        (
            with(good, "## Review\n## Comments\nstray\n### `a.c:1`\nx\n"),
            Err("before its first ### heading"),
        ),
        (
            with(good, "## Review\n## Comments\n## Comments\n"),
            Err("two ## Comments sections"),
        ),
        (
            with(good, "## Review\n## Comments\n### a.c:1\nx\n"),
            Err("is not `PATH:LINE`"),
        ),
        (
            with(good, "## Review\n## Comments\n### `:1`\nx\n"),
            Err("is not `PATH:LINE`"),
        ),
        (
            with(good, "## Review\n## Comments\n### `a.c:0`\nx\n"),
            Err("has no line number"),
        ),
        (
            with(good, "## Review\n## Comments\n### `a.c:1-3`\nx\n"),
            Err("has no line number"),
        ),
        (
            with(
                good,
                "## Review\n## Comments\n### `a.c:1`\n\n### `a.c:2`\nx\n",
            ),
            Err("the comment on a.c:1 is empty"),
        ),
    ] {
        match (parse(&text, CHECKBOX), expected) {
            (Ok(draft), Ok(n)) => assert_eq!(draft.comments.len(), n, "{text}"),
            (Err(error), Err(expected)) => {
                assert!(format!("{error:#}").contains(expected), "{text}\n{error:#}")
            }
            (result, expected) => panic!("{text}\n{result:?} != {expected:?}"),
        }
    }
}

#[test]
fn diff_lines_are_the_new_side_of_each_hunk() {
    let files = fixture("files");
    let lines = |n: usize| new_lines(files[n]["patch"].as_str().unwrap()).unwrap();
    assert_eq!(
        lines(0).into_iter().collect::<Vec<_>>(),
        [339, 340, 341, 342, 343, 344, 345]
    );
    let deploy = lines(1);
    assert!(deploy.contains(&3415) && deploy.contains(&3388) && deploy.contains(&3602));
    assert!(!deploy.contains(&3000) && !deploy.contains(&3603));
    assert!(new_lines("@@ bogus @@").is_err());
    assert!(new_lines("").unwrap().is_empty());
}

/// The `issues: edited` payload of the operator's ticking the recorded box.
fn tick_event() -> Value {
    let body = fixture("issue")["body"].as_str().unwrap().to_owned();
    json!({"action": "edited", "sender": {"login": "Operator"},
        "repository": {"full_name": "example/tracker"},
        "changes": {"body": {"from": body.replace("- [x]", "- [ ]")}},
        "issue": {"number": 900, "body": body, "updated_at": "2026-10-21T11:00:00Z"}})
}

/// The approval the recorded body's last edit gives.
fn ticked() -> Ticked {
    Ticked {
        at: OffsetDateTime::parse("2026-10-21T11:00:00Z", &Rfc3339).unwrap(),
        body: fixture("issue")["body"].as_str().unwrap().to_owned(),
    }
}

#[test]
fn approving_event() {
    let issue = ItemRef {
        host: "github.com".into(),
        repository: "example/tracker".into(),
        number: 900,
    };
    assert_eq!(
        Ticked::from_event(&tick_event(), &issue, &config()).unwrap(),
        ticked()
    );
    // A change to the event, then the error.
    for (pointer, value, expected) in [
        ("/action", json!("opened"), "not an issue edit"),
        ("/sender/login", json!("visitor"), "edit is not operator's"),
        ("/repository/full_name", json!("example/other"), "not of"),
        ("/issue/number", json!(901), "not of"),
        ("/issue/pull_request", json!({}), "not of"),
        ("/changes", json!({"title": {"from": "x"}}), "kept the body"),
        (
            "/changes/body/from",
            json!(format!("- [X] {CHECKBOX}")),
            "did not tick",
        ),
        (
            "/issue/body",
            json!(format!("- [ ] {CHECKBOX}")),
            "did not tick",
        ),
        ("/issue/updated_at", json!("yesterday"), "updated_at"),
    ] {
        let mut event = tick_event();
        match pointer.rsplit_once('/') {
            Some((parent, key)) if event.pointer(parent).is_some() => {
                event.pointer_mut(parent).unwrap()[key] = value
            }
            _ => unreachable!("{pointer}"),
        }
        let error = Ticked::from_event(&event, &issue, &config()).unwrap_err();
        assert!(
            format!("{error:#}").contains(expected),
            "{pointer}: {error:#}"
        );
    }
}

/// The recorded tracker issue and pull request, as GitHub serves them.
struct Forge {
    viewer: &'static str,
    /// The edit `By::Edit` approves with: by default [`ticked`].
    ticked: Ticked,
    issue: Value,
    comments: RefCell<Vec<Value>>,
    pull: Value,
    files: Value,
    twist: &'static str,
    files_listed: Cell<bool>,
    writes: RefCell<Vec<(String, String, Value)>>,
}

impl Forge {
    fn new() -> Self {
        Self {
            viewer: BOT,
            ticked: ticked(),
            issue: fixture("issue"),
            comments: RefCell::new(fixture("comments").as_array().unwrap().clone()),
            pull: fixture("pull"),
            files: fixture("files"),
            twist: "",
            files_listed: Cell::default(),
            writes: RefCell::default(),
        }
    }

    fn post(&self, author: &str, body: &str) -> u64 {
        let mut comments = self.comments.borrow_mut();
        let id = 9100 + comments.len() as u64;
        comments.push(json!({"url": format!("https://github.com/example/tracker/issues/900#issuecomment-{id}"),
            "body": body, "createdAt": "2026-10-21T12:00:00Z", "lastEditedAt": null,
            "author": {"login": author}, "editor": null}));
        id
    }
}

impl Transport for Forge {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        if query.contains("viewer") {
            return Ok(json!({"viewer": {"login": self.viewer}}));
        }
        assert_eq!(variables["number"], 900);
        if query.contains("comments(first") {
            return Ok(json!({"repository": {"issue": {"comments": {
                "nodes": *self.comments.borrow(),
                "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}));
        }
        assert_eq!(query, ISSUE_QUERY);
        Ok(json!({"repository": {"issue": self.issue}}))
    }
}

impl Rest for Forge {
    fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
        // The files come in two pages, as a Link header splits them.
        let (status, body, link) = match url.strip_prefix(PULL) {
            // A push lands while the files are listed.
            Some("") if self.twist == "head moves" && self.files_listed.get() => {
                let mut pull = self.pull.clone();
                pull["head"]["sha"] = json!("b".repeat(40));
                (200, pull, None)
            }
            Some("") => (200, self.pull.clone(), None),
            Some("/files?per_page=100") => (
                200,
                json!(self.files.as_array().unwrap()[..2]),
                Some(format!(r#"<{PULL}/files?per_page=100&page=2>; rel="next""#)),
            ),
            Some("/files?per_page=100&page=2") => {
                self.files_listed.set(true);
                (200, json!(self.files.as_array().unwrap()[2..]), None)
            }
            _ => (404, json!({"message": "Not Found"}), None),
        };
        Ok(RestResponse {
            status,
            etag: None,
            last_modified: None,
            link,
            body,
        })
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<(u16, Value)> {
        self.writes
            .borrow_mut()
            .push((method.into(), url.into(), body.clone()));
        let comments = "https://api.github.com/repos/example/tracker/issues/900/comments";
        match (method, url) {
            ("POST", u) if u == comments => {
                if self.twist == "race" {
                    self.post(BOT, &format!("{SENT_MARKER}\nSending."));
                }
                let id = self.post(self.viewer, body["body"].as_str().unwrap());
                Ok((201, json!({"id": id})))
            }
            ("POST", u) if u == format!("{PULL}/reviews") => match self.twist {
                "review fails" => Ok((422, json!({"message": "Unprocessable"}))),
                _ => Ok((200, json!({"html_url": REVIEW}))),
            },
            ("PATCH", _) => Ok((200, json!({}))),
            _ => panic!("unexpected {method} {url}"),
        }
    }
}

/// What a case should come to.
#[derive(Debug)]
enum Expect {
    /// Sent; a dry run returns the requests.
    Sent,
    /// A refusal: the reason commented, with --apply.
    Refused(&'static str),
    /// A dry run as for `Sent`; --apply refuses after its claim, which
    /// becomes the reason.
    LateRefused(&'static str),
    /// Any other error, before any write.
    Error(&'static str),
    /// A dry run as for `Sent`; --apply fails with this.
    ApplyError(&'static str),
}

/// The recorded draft with `from` replaced by `to`.
fn redraft(forge: &mut Forge, from: &str, to: &str) {
    let mut comments = forge.comments.borrow_mut();
    let body = comments[0]["body"].as_str().unwrap().replace(from, to);
    comments[0]["body"] = body.into();
}

fn edit(value: &mut Value, at: &str, editor: &str) {
    value["lastEditedAt"] = json!(at);
    value["editor"] = json!({"login": editor});
}

/// How a case approves: a `/send`, or the edit in [`Forge::ticked`].
#[derive(Clone, Copy)]
enum By {
    Command,
    Edit,
}

#[test]
fn approval_and_refusals() {
    use By::{Command, Edit};
    use Expect::*;
    type Twist = fn(&mut Forge);
    // Name, a change to the recorded state, the approval, then the outcome.
    let cases: Vec<(&str, Twist, By, Expect)> = vec![
        ("send command", |_| {}, Command, Sent),
        ("ticked box", |_| {}, Edit, Sent),
        (
            "draft in the body",
            |f| {
                let text = draft_text();
                f.issue["body"] = format!("{text}\n- [x] {CHECKBOX}\n").into();
                f.ticked.body = f.issue["body"].as_str().unwrap().into();
                f.comments.borrow_mut().clear();
            },
            Edit,
            Sent,
        ),
        (
            "draft in the body edited by the operator after the tick",
            |f| {
                let text = draft_text();
                f.ticked.body = format!("{text}\n- [x] {CHECKBOX}\n");
                f.issue["body"] = f.ticked.body.replace("hangs", "hangs forever").into();
                edit(&mut f.issue, "2026-10-21T11:30:00Z", "operator");
                f.comments.borrow_mut().clear();
            },
            Edit,
            Error("the body changed after the edit that ticked its checkbox"),
        ),
        (
            // The tick's job is queued; the bot posts a newer draft, which
            // the tick cannot have approved.
            "newer draft after the tick",
            |f| {
                f.post(BOT, &draft_text().replace("hangs", "hangs forever"));
            },
            Edit,
            Error("the newest draft changed after the checkbox was ticked"),
        ),
        (
            // As above, then the operator fixes a typo in the body, leaving
            // the box ticked: the body's last edit is theirs and after the
            // newer draft, but it is not the edit that ticked the box.
            "delayed tick job, newer draft, then an unrelated body edit",
            |f| {
                f.post(BOT, &draft_text().replace("hangs", "hangs forever"));
                let body = f.issue["body"].as_str().unwrap().replace("follows", "is");
                f.issue["body"] = body.into();
                edit(&mut f.issue, "2026-10-21T12:30:00Z", "operator");
            },
            Edit,
            Error("the body changed after the edit that ticked its checkbox"),
        ),
        (
            "draft edited by the operator before the tick",
            |f| {
                edit(
                    &mut f.comments.borrow_mut()[0],
                    "2026-10-21T10:45:00Z",
                    "operator",
                )
            },
            Edit,
            Sent,
        ),
        (
            "draft edited after its /send",
            |f| {
                edit(
                    &mut f.comments.borrow_mut()[0],
                    "2026-10-21T10:45:00Z",
                    "operator",
                )
            },
            Command,
            Error("no /send from operator after the newest draft's last edit"),
        ),
        (
            "draft edited after the tick",
            |f| edit(&mut f.comments.borrow_mut()[0], "2026-10-21T11:30:00Z", BOT),
            Edit,
            Error("changed after the checkbox was ticked"),
        ),
        (
            "no /send",
            |f| {
                f.comments.borrow_mut().pop();
            },
            Command,
            Error("no /send"),
        ),
        (
            "/send not on a line of its own",
            |f| f.comments.borrow_mut()[1]["body"] = json!("please /send"),
            Command,
            Error("no /send"),
        ),
        (
            "/send in a fenced example",
            |f| {
                f.comments.borrow_mut()[1]["body"] =
                    json!("Not yet. Once CI passes, comment:\n\n```\n/send\n```\n")
            },
            Command,
            Error("no /send"),
        ),
        (
            "/send in an HTML comment",
            |f| f.comments.borrow_mut()[1]["body"] = json!("Looks right. <!--\n/send\n-->\n"),
            Command,
            Error("no /send"),
        ),
        (
            "/send quoted",
            |f| f.comments.borrow_mut()[1]["body"] = json!("> /send\n\nNot until CI passes.\n"),
            Command,
            Error("no /send"),
        ),
        (
            // Lazy continuation: the second line is in the blockquote too.
            "/send continuing a quote",
            |f| {
                f.comments.borrow_mut()[1]["body"] =
                    json!("> Example only, do not execute:\n/send\n")
            },
            Command,
            Error("no /send"),
        ),
        (
            "/send in an HTML block",
            |f| f.comments.borrow_mut()[1]["body"] = json!("<pre>\n/send\n</pre>\n"),
            Command,
            Error("no /send"),
        ),
        (
            "/send after other text",
            |f| f.comments.borrow_mut()[1]["body"] = json!("Looks right.\n/send\n"),
            Command,
            Error("no /send"),
        ),
        (
            "/send with blank lines around it",
            |f| f.comments.borrow_mut()[1]["body"] = json!("\n /send \n\n"),
            Command,
            Sent,
        ),
        (
            "/send before a refusal",
            |f| {
                f.post(BOT, &format!("{REFUSED_MARKER}\nNot sent: it is closed."));
            },
            Command,
            Error("no /send from operator after the newest draft's last edit and the last refusal"),
        ),
        (
            "/send after a refusal",
            |f| {
                f.post(BOT, &format!("{REFUSED_MARKER}\nNot sent: it is closed."));
                f.post("operator", "/send");
                f.comments.borrow_mut()[3]["createdAt"] = json!("2026-10-21T12:01:00Z");
            },
            Command,
            Sent,
        ),
        (
            // A send claims, the operator sends again, then the send edits
            // its claim into a refusal: that /send predates the refusal.
            "claim edited into a refusal after a later /send",
            |f| {
                f.post(BOT, &format!("{SENT_MARKER}\nSending."));
                f.post("operator", "/send");
                let mut comments = f.comments.borrow_mut();
                comments[2]["body"] = json!(format!("{REFUSED_MARKER}\nNot sent: it moved."));
                edit(&mut comments[2], "2026-10-21T12:02:00Z", BOT);
                comments[3]["createdAt"] = json!("2026-10-21T12:01:00Z");
            },
            Command,
            Error("no /send from operator after the newest draft's last edit and the last refusal"),
        ),
        (
            "box ticked before a refusal",
            |f| {
                f.post(BOT, &format!("{REFUSED_MARKER}\nNot sent: it is closed."));
            },
            Edit,
            Error("ticked before the last refusal"),
        ),
        (
            "a visitor's copied refusal",
            |f| {
                f.post("visitor", &format!("{REFUSED_MARKER}\nNot sent."));
            },
            Edit,
            Sent,
        ),
        (
            "/send by someone else",
            |f| f.comments.borrow_mut()[1]["author"] = json!({"login": "visitor"}),
            Command,
            Error("no /send"),
        ),
        (
            "/send by the bot",
            |f| f.comments.borrow_mut()[1]["author"] = json!({"login": BOT}),
            Command,
            Error("no /send"),
        ),
        (
            // The operator's comment counts as a draft whose fence a
            // visitor may have removed, as in newest_fenced.
            "/send edited in by someone else",
            |f| {
                edit(
                    &mut f.comments.borrow_mut()[1],
                    "2026-10-21T10:40:00Z",
                    "visitor",
                )
            },
            Command,
            Error("9002 was last edited by visitor"),
        ),
        (
            "/send before a newer draft",
            |f| {
                let text = draft_text().replace("COMMENT,", "x");
                f.post("operator", &text);
            },
            Command,
            Error("no /send"),
        ),
        (
            "box ticked by someone else",
            |f| edit(&mut f.issue, "2026-10-21T11:00:00Z", "visitor"),
            Edit,
            Error("not operator's ticking its checkbox"),
        ),
        (
            "box not ticked",
            |f| f.issue["body"] = json!(format!("- [ ] {CHECKBOX}")),
            Edit,
            Error("not operator's ticking its checkbox"),
        ),
        (
            "box ticked beside code and comments",
            |f| {
                f.issue["body"] = json!(format!(
                    "```sh\nls\n```\n\n<!-- note -->\n\n- [x] {CHECKBOX}\n"
                ));
                f.ticked.body = f.issue["body"].as_str().unwrap().into();
            },
            Edit,
            Sent,
        ),
        (
            "box ticked only in a fenced example",
            |f| f.issue["body"] = json!(format!("Tick:\n\n```\n- [x] {CHECKBOX}\n```\n")),
            Edit,
            Error("not operator's ticking its checkbox"),
        ),
        (
            "box ticked only in an HTML block",
            |f| f.issue["body"] = json!(format!("<details>\n- [x] {CHECKBOX}\n</details>\n")),
            Edit,
            Error("not operator's ticking its checkbox"),
        ),
        (
            "box ticked only in an HTML comment",
            |f| f.issue["body"] = json!(format!("<!--\n- [x] {CHECKBOX}\n-->\n")),
            Edit,
            Error("not operator's ticking its checkbox"),
        ),
        (
            "box ticked in an example beside the real one",
            |f| {
                f.issue["body"] = json!(format!(
                    "- [ ] {CHECKBOX}\n\nLike this:\n\n<pre>\n- [x] {CHECKBOX}\n</pre>\n"
                ))
            },
            Edit,
            Error("not operator's ticking its checkbox"),
        ),
        (
            "two ticked boxes",
            |f| {
                f.issue["body"] = json!(format!("- [x] {CHECKBOX}\n- [x] {CHECKBOX}\n"));
                f.ticked.body = f.issue["body"].as_str().unwrap().into();
            },
            Edit,
            Error("not operator's ticking its checkbox"),
        ),
        (
            "draft by someone else",
            |f| f.comments.borrow_mut()[0]["author"] = json!({"login": "visitor"}),
            Command,
            Error("no review draft on this issue"),
        ),
        (
            "draft edited by someone else",
            |f| {
                edit(
                    &mut f.comments.borrow_mut()[0],
                    "2026-10-21T10:10:00Z",
                    "visitor",
                )
            },
            Command,
            Error("last edited by visitor, who may not write drafts"),
        ),
        (
            "draft removed by someone else's edit",
            |f| {
                let mut comments = f.comments.borrow_mut();
                let copy = comments[0].clone();
                comments.insert(1, copy);
                comments[1]["body"] = json!("removed");
                edit(&mut comments[1], "2026-10-21T10:10:00Z", "visitor");
            },
            Command,
            Error("last edited by visitor"),
        ),
        (
            "not labelled",
            |f| f.issue["labels"]["nodes"] = json!([{"name": "bug"}]),
            Command,
            Error("is not labelled review-draft"),
        ),
        (
            "closed issue",
            |f| f.issue["state"] = json!("CLOSED"),
            Command,
            Error("example/tracker#900 is closed"),
        ),
        (
            "already sent",
            |f| {
                f.post(BOT, &format!("{SENT_MARKER}\nSent: {REVIEW}"));
            },
            Command,
            Error("already records a sent review"),
        ),
        (
            "a visitor's copied record",
            |f| {
                f.post("visitor", &format!("{SENT_MARKER}\nSent."));
            },
            Command,
            Sent,
        ),
        (
            "token of another account",
            |f| f.viewer = "operator",
            Command,
            ApplyError("requires the configured bot's token"),
        ),
        (
            "closed pull request",
            |f| f.pull["state"] = json!("closed"),
            Command,
            Refused("ostreedev/ostree#3649 is closed"),
        ),
        (
            "moved head",
            |f| f.pull["head"]["sha"] = json!("b".repeat(40)),
            Edit,
            Refused("the head of ostreedev/ostree#3649 moved from a0207f1"),
        ),
        (
            "head moved while listing files",
            |f| f.twist = "head moves",
            Command,
            LateRefused("the head of ostreedev/ostree#3649 moved from a0207f1"),
        ),
        (
            "moved repository",
            |f| f.pull["base"]["repo"]["full_name"] = json!("elsewhere/ostree"),
            Command,
            Refused("is now in another repository"),
        ),
        (
            "line not in the diff",
            |f| redraft(f, ".c:3415", ".c:3000"),
            Command,
            Refused(
                "src/libostree/ostree-sysroot-deploy.c:3000 is not in the diff of ostreedev/ostree#3649 at a0207f1",
            ),
        ),
        (
            "file not in the diff",
            |f| redraft(f, "ostree-repo-finder-mount.c", "other.c"),
            Command,
            Refused("src/libostree/other.c:342 is not in the diff"),
        ),
        (
            "target outside the list",
            |f| redraft(f, "ostreedev/ostree", "elsewhere/ostree"),
            Command,
            Refused("elsewhere/ostree#3649 is outside review.targets"),
        ),
        (
            "malformed draft",
            |f| redraft(f, "REQUEST_CHANGES\"", "MERGE\""),
            Command,
            Refused("event \"MERGE\" is not one of"),
        ),
        (
            "earlier claim races",
            |f| f.twist = "race",
            Command,
            ApplyError("an earlier claim on example/tracker#900 won"),
        ),
        (
            "review refused by GitHub",
            |f| f.twist = "review fails",
            Command,
            ApplyError("stops a replay"),
        ),
    ];
    let issue = ItemRef {
        host: "github.com".into(),
        repository: "example/tracker".into(),
        number: 900,
    };
    for (name, twist, by, expected) in cases {
        for apply in [false, true] {
            let mut forge = Forge::new();
            twist(&mut forge);
            let approval = match by {
                By::Command => Approval::Command,
                By::Edit => Approval::Edit(forge.ticked.clone()),
            };
            let result = send(&forge, &issue, &config(), &approval, apply);
            let writes: Vec<(String, String)> = forge
                .writes
                .borrow()
                .iter()
                .map(|(m, u, _)| (m.clone(), u.trim_start_matches(API_ROOT).to_owned()))
                .collect();
            let writes: Vec<(&str, &str)> = writes
                .iter()
                .map(|(m, u)| (m.as_str(), u.as_str()))
                .collect();
            let issue_path = "repos/example/tracker/issues/900";
            let comments_path = "repos/example/tracker/issues/900/comments";
            let reviews_path = "repos/ostreedev/ostree/pulls/3649/reviews";
            match (&result, &expected) {
                (Ok(lines), Sent | ApplyError(_) | LateRefused(_)) if !apply => {
                    assert!(writes.is_empty(), "{name}");
                    assert_eq!(lines.len(), 4, "{name}");
                    assert_eq!(lines[1]["body"], review_request(), "{name}");
                }
                (Ok(lines), Sent) => {
                    let claim =
                        format!("repos/example/tracker/issues/comments/{}", claim_id(&forge));
                    assert_eq!(
                        writes,
                        [
                            ("POST", comments_path),
                            ("POST", reviews_path),
                            ("PATCH", claim.as_str()),
                            ("PATCH", issue_path),
                        ],
                        "{name}"
                    );
                    assert_eq!(lines[0]["review"], REVIEW, "{name}");
                    let w = forge.writes.borrow();
                    assert_eq!(w[1].2, review_request(), "{name}");
                    assert!(w[0].2["body"].as_str().unwrap().starts_with(SENT_MARKER));
                    assert!(w[2].2["body"].as_str().unwrap().contains(REVIEW));
                    // The state alone, never the body, which someone may be
                    // editing.
                    assert_eq!(
                        w[3].2,
                        json!({"state": "closed", "state_reason": "completed"}),
                        "{name}"
                    );
                }
                (Err(error), Refused(reason)) => {
                    let error = format!("{error:#}");
                    assert!(error.contains(reason), "{name}: {error}");
                    if apply {
                        assert_eq!(writes, [("POST", comments_path)], "{name}");
                        assert_refusal(&forge, reason);
                    } else {
                        assert!(writes.is_empty(), "{name}");
                    }
                }
                (Err(error), LateRefused(reason)) if apply => {
                    let error = format!("{error:#}");
                    assert!(error.contains(reason), "{name}: {error}");
                    let claim =
                        format!("repos/example/tracker/issues/comments/{}", claim_id(&forge));
                    assert_eq!(
                        writes,
                        [("POST", comments_path), ("PATCH", claim.as_str())],
                        "{name}"
                    );
                    assert_refusal(&forge, reason);
                }
                (Err(error), Error(message)) => {
                    let error = format!("{error:#}");
                    assert!(error.contains(message), "{name}: {error}");
                    assert!(writes.is_empty(), "{name}");
                }
                (Err(error), ApplyError(message)) if apply => {
                    let error = format!("{error:#}");
                    assert!(error.contains(message), "{name}: {error}");
                    let expected: &[(&str, &str)] = match name {
                        "earlier claim races" => &[("POST", comments_path)],
                        "review refused by GitHub" => {
                            &[("POST", comments_path), ("POST", reviews_path)]
                        }
                        _ => &[],
                    };
                    assert_eq!(writes, expected, "{name}");
                }
                (result, expected) => panic!("{name} (apply {apply}): {result:?} != {expected:?}"),
            }
        }
    }
}

/// The refusal's comment, the last write, gives the reason.
fn assert_refusal(forge: &Forge, reason: &str) {
    let w = forge.writes.borrow();
    let comment = w[w.len() - 1].2["body"].as_str().unwrap();
    assert!(
        comment.starts_with(&format!("{REFUSED_MARKER}\nNot sent: ")),
        "{comment}"
    );
    assert!(comment.contains(reason), "{comment}");
}

/// The review the recorded draft makes, with the operator's leading line.
fn review_request() -> Value {
    json!({"commit_id": HEAD, "event": "REQUEST_CHANGES",
        "body": "operator approved sending this review.\n\nThe policy refresh can still block finalization when `semodule` hangs.",
        "comments": [
            {"path": "src/libostree/ostree-sysroot-deploy.c", "line": 3415, "side": "RIGHT",
             "body": "*Important*: this waits without a timeout.\n\n```c\nwaitpid (pid, &status, 0);\n```"},
            {"path": "src/libostree/ostree-repo-finder-mount.c", "line": 342, "side": "RIGHT",
             "body": "(low) Unrelated reformatting.\n\nGenerated-by: https://github.com/cgwalters/#llms"}]})
}

/// The id the fake gave the bot's claim.
fn claim_id(forge: &Forge) -> String {
    forge
        .comments
        .borrow()
        .iter()
        .rev()
        .find(|c| {
            c["body"].as_str().unwrap().starts_with(SENT_MARKER) && c["author"]["login"] == BOT
        })
        .and_then(|c| comment_id(c["url"].as_str().unwrap()))
        .unwrap()
        .to_string()
}
