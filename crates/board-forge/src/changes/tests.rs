use super::*;
use std::cell::RefCell;

const SINCE: &str = "2026-10-20T00:00:00Z";
const BEFORE: &str = "2026-10-19T00:00:00Z";
const IN: &str = "2026-10-21T00:00:00Z";
/// The token's login, and the operator's.
const VIEWER: &str = "worker";
const OPERATOR: &str = "owner";

fn authors() -> BTreeSet<String> {
    [VIEWER, OPERATOR].map(str::to_owned).into()
}

/// A member as [`ITEMS_QUERY`] reads it: open, untouched and created before
/// the window unless `fields` says otherwise. Its state follows its merge and
/// closure unless given, and its nested lists are whole unless they say so.
fn item(kind: &str, n: u64, fields: Value) -> Value {
    let path = if kind == "Issue" { "issues" } else { "pull" };
    let mut c = json!({"__typename": kind, "number": n, "title": format!("Item {n}"),
        "url": format!("https://github.com/example/project/{path}/{n}"), "createdAt": BEFORE,
        "closedAt": null, "repository": {"nameWithOwner": "example/project"},
        "labels": {"nodes": []}, "timelineItems": {"nodes": []}});
    if kind == "PullRequest" {
        c["author"] = json!({"__typename": "User", "login": VIEWER});
        c["merged"] = json!(false);
        c["mergedAt"] = Value::Null;
        c["closingIssuesReferences"] = json!({"nodes": []});
    }
    for (k, v) in fields.as_object().unwrap() {
        c[k] = v.clone();
    }
    if c.get("state").is_none() {
        c["state"] = json!(if c["merged"] == true {
            "MERGED"
        } else if c["closedAt"].is_string() {
            "CLOSED"
        } else {
            "OPEN"
        });
    }
    for list in ["labels", "timelineItems", "closingIssuesReferences"] {
        if c[list].is_object() && c[list].get("pageInfo").is_none() {
            c[list]["pageInfo"] = json!({"hasNextPage": false});
        }
    }
    json!({"content": c})
}

fn event(kind: &str, label: &str, at: &str) -> Value {
    json!({"nodes": [{"__typename": kind, "createdAt": at, "label": {"name": label}}]})
}

/// A pull request as a search finds it.
fn pr(n: u64, fields: Value) -> Value {
    item("PullRequest", n, fields)["content"].take()
}

fn closes(n: u64) -> Value {
    json!({"nodes": [{"number": n, "url": format!("https://github.com/example/project/issues/{n}"),
        "repository": {"nameWithOwner": "example/project"}}]})
}

/// One window holding every kind of change, and members that are none.
fn window() -> Vec<Value> {
    vec![
        item("Issue", 2, json!({})),
        item(
            "PullRequest",
            12,
            json!({"title": "Add the changes list", "merged": true, "mergedAt": IN, "closedAt": IN,
            "closingIssuesReferences": closes(7)}),
        ),
        // Closed by the landing, so told on its line.
        item("Issue", 7, json!({"closedAt": IN})),
        item(
            "Issue",
            5,
            json!({"title": "Flaky [test] <b>", "closedAt": IN}),
        ),
        item("PullRequest", 15, json!({"closedAt": IN})),
        // Unmerged, so its closing reference is only intent.
        item(
            "PullRequest",
            14,
            json!({"createdAt": IN, "closingIssuesReferences": closes(2)}),
        ),
        // New and held at once: new is the news.
        item(
            "Issue",
            20,
            json!({"createdAt": IN, "labels": {"nodes": [{"name": "needs-human"}]},
            "timelineItems": event("LabeledEvent", "needs-human", IN)}),
        ),
        // Reopened: its old closure is not news.
        item("Issue", 8, json!({"closedAt": IN, "state": "OPEN"})),
        // Labeled at the cutoff itself, which the window excludes.
        item(
            "Issue",
            10,
            json!({"timelineItems": event("LabeledEvent", "needs-human", SINCE)}),
        ),
        // Merged before the window: only its hold is news.
        item(
            "Issue",
            9,
            json!({"labels": {"nodes": [{"name": "Needs-Human"}]},
            "timelineItems": event("LabeledEvent", "needs-human", IN)}),
        ),
        item(
            "Issue",
            3,
            json!({"timelineItems": event("UnlabeledEvent", "needs-design", IN)}),
        ),
        item(
            "Issue",
            4,
            json!({"timelineItems": event("LabeledEvent", "bug", IN)}),
        ),
        item(
            "Issue",
            6,
            json!({"timelineItems": event("LabeledEvent", "needs-human", BEFORE)}),
        ),
        item(
            "PullRequest",
            1,
            json!({"merged": true, "mergedAt": BEFORE, "closedAt": BEFORE}),
        ),
        json!({"content": {"__typename": "DraftIssue"}}),
    ]
}

const WINDOW: &str = "\
**Landed**
- [example/project#12](https://github.com/example/project/pull/12) Add the changes list (closes [example/project#7](https://github.com/example/project/issues/7))

**Closed**
- [example/project#5](https://github.com/example/project/issues/5) Flaky \\[test\\] \\<b\\>
- [example/project#15](https://github.com/example/project/pull/15) Item 15

**Opened**
- [example/project#14](https://github.com/example/project/pull/14) Item 14
- [example/project#20](https://github.com/example/project/issues/20) Item 20

**Held**
- [example/project#3](https://github.com/example/project/issues/3) Item 3 (released)
- [example/project#9](https://github.com/example/project/issues/9) Item 9
";

#[test]
fn a_window_lists_each_change_once_in_order() {
    let mut items = window();
    assert_eq!(
        classify(&items, &[], SINCE, &authors()).unwrap().markdown(),
        WINDOW
    );
    items.reverse();
    let changes = classify(&items, &[], SINCE, &authors()).unwrap();
    assert_eq!(changes.markdown(), WINDOW);
    let json = serde_json::to_value(&changes).unwrap();
    assert_eq!(json["since"], SINCE);
    assert_eq!(
        json["landed"],
        json!([{"ref": "example/project#12", "url": "https://github.com/example/project/pull/12",
            "title": "Add the changes list", "closes": [{"ref": "example/project#7", "url": "https://github.com/example/project/issues/7"}]}])
    );
    assert_eq!(json["held"][0]["released"], true);
    assert!(json["held"][1].get("released").is_none());
}

#[test]
fn an_empty_window_says_so() {
    for items in [vec![], vec![item("Issue", 2, json!({}))]] {
        let changes = classify(&items, &[], SINCE, &authors()).unwrap();
        assert_eq!(changes.markdown(), format!("No changes since {SINCE}\n"));
        assert_eq!(
            serde_json::to_value(&changes).unwrap(),
            json!({"since": SINCE})
        );
    }
}

#[test]
fn unreadable_members_refuse_the_list() {
    let mut no_labels = item("Issue", 2, json!({}));
    no_labels["content"]["labels"] = Value::Null;
    let mut no_state = item("Issue", 2, json!({}));
    no_state["content"].as_object_mut().unwrap().remove("state");
    let more = json!({"nodes": [], "pageInfo": {"hasNextPage": true}});
    for (items, since) in [
        (vec![json!({"content": null})], SINCE),
        (
            vec![item("Issue", 2, json!({"closedAt": "yesterday"}))],
            SINCE,
        ),
        (vec![item("Issue", 2, json!({"closedAt": 1}))], SINCE),
        (vec![no_labels], SINCE),
        (vec![no_state], SINCE),
        (vec![item("Issue", 2, json!({"labels": more}))], SINCE),
        (
            vec![item("Issue", 2, json!({"timelineItems": more}))],
            SINCE,
        ),
        (
            vec![item(
                "PullRequest",
                2,
                json!({"merged": true, "mergedAt": IN, "closedAt": IN, "closingIssuesReferences": more}),
            )],
            SINCE,
        ),
        (
            vec![item("Issue", 2, json!({})), item("Issue", 2, json!({}))],
            SINCE,
        ),
        (vec![], "last week"),
    ] {
        assert!(
            classify(&items, &[], since, &authors()).is_err(),
            "{items:?} {since}"
        );
    }
}

#[test]
fn summaries_link_full_refs_and_unambiguous_short_ones() {
    let repos: BTreeSet<String> = [
        "cgwalters-forge/tracker",
        "a/dup",
        "b/dup",
        "Example/Project",
    ]
    .map(str::to_owned)
    .into();
    let resolve = |r: &str| resolve(r, &repos);
    let known = |r: &str| (r == "example/project#12").then(|| "https://x/pull/12".to_owned());
    for (summary, linked) in [
        (
            "see o/r#5.",
            "see [o/r#5](https://github.com/o/r/issues/5).",
        ),
        (
            "(o/r#5, o.x/r-2#6)",
            // o.x is no login.
            "([o/r#5](https://github.com/o/r/issues/5), o.x/r-2#6)",
        ),
        (
            "(o/r#5, o-x/r.2#6)",
            "([o/r#5](https://github.com/o/r/issues/5), [o-x/r.2#6](https://github.com/o-x/r.2/issues/6))",
        ),
        (
            "Example/Project#12",
            "[Example/Project#12](https://x/pull/12)",
        ),
        ("[o/r#5](elsewhere)", "[o/r#5](elsewhere)"),
        ("https://github.com/o/r#5", "https://github.com/o/r#5"),
        (
            "tracker#885 and Project#12",
            "[tracker#885](https://github.com/cgwalters-forge/tracker/issues/885) and [Project#12](https://x/pull/12)",
        ),
        // Two covered repositories have the name, or none does.
        (
            "dup#1 nothing#2 a/tracker#3",
            "dup#1 nothing#2 [a/tracker#3](https://github.com/a/tracker/issues/3)",
        ),
        ("o/r#5x o/r#05 o/r# a/b/c#5", "o/r#5x o/r#05 o/r# a/b/c#5"),
        ("#5 o/r#5#6", "#5 o/r#5#6"),
        ("é o/r#5 ü", "é [o/r#5](https://github.com/o/r/issues/5) ü"),
    ] {
        let known = |r: &str| known(&r.to_ascii_lowercase());
        assert_eq!(linkify(summary, &resolve, &known), linked, "{summary}");
    }
}

#[test]
fn notes_ride_their_lines_or_go_under_also() {
    let changes = classify(&window(), &[], SINCE, &authors()).unwrap();
    let notes: BTreeMap<_, _> = [
        // A short ref resolves against the covered repositories.
        ("project#14", "Needs a rebase."),
        ("nothing#3", "Unknown, so plain."),
        // An issue the landing closed: noted on the landing's line.
        ("Example/Project#7", "Fixed at last."),
        // Only an open pull request's intent to close it: not on that line.
        ("example/project#2", "Next."),
        ("example/other#1", " Upstream is slow. "),
    ]
    .map(|(r, n)| (r.to_owned(), n.to_owned()))
    .into();
    let body = changes
        .status_body(Some("Landed example/project#12."), &notes, usize::MAX)
        .unwrap();
    assert!(body.starts_with(
        "Landed [example/project#12](https://github.com/example/project/pull/12).\n\n**Landed**\n"
    ));
    assert!(body.contains("(closes [example/project#7](https://github.com/example/project/issues/7)) — Fixed at last.\n"));
    assert!(body.contains("/pull/14) Item 14 — Needs a rebase.\n"));
    assert!(body.ends_with(
        "\n\n**Also**\n- [example/other#1](https://github.com/example/other/issues/1) — Upstream is slow.\n\
        - [example/project#2](https://github.com/example/project/issues/2) — Next.\n\
        - nothing#3 — Unknown, so plain.\n"
    ));
    for bad in [
        &[("other#x", "x")][..],
        &[("o/r#1", "x"), ("O/R#1", "y")],
        &[("project#2", "x"), ("example/project#2", "y")],
    ] {
        let bad: BTreeMap<_, _> = bad
            .iter()
            .map(|(r, n)| (r.to_string(), n.to_string()))
            .collect();
        assert!(changes.status_body(None, &bad, usize::MAX).is_err());
    }
    // Mentions in a title stay text.
    let mention = classify(
        &[item(
            "Issue",
            2,
            json!({"createdAt": IN, "title": "Ask @someone"}),
        )],
        &[],
        SINCE,
        &authors(),
    )
    .unwrap();
    assert!(mention.markdown().contains("Ask \\@someone"));
}

#[test]
fn long_bodies_trim_the_longest_sections_first() {
    let mut items: Vec<_> = (100..110)
        .map(|n| item("Issue", n, json!({"createdAt": IN})))
        .collect();
    items.extend((200..203).map(|n| item("Issue", n, json!({"closedAt": IN}))));
    let changes = classify(&items, &[], SINCE, &authors()).unwrap();
    let notes: BTreeMap<_, _> = [("example/project#100".to_owned(), "Kept.".to_owned())].into();
    let full = changes
        .status_body(Some("Busy."), &notes, usize::MAX)
        .unwrap();
    // Opened, the longest, gives way until Closed is as long; ties trim the
    // later section, sparing Landed; the noted line goes last.
    let limit = full.chars().count() - 1;
    for (limit, closed, opened) in [
        (full.chars().count(), 3, 10),
        (limit, 3, 9),
        (limit - 600, 3, 2),
    ] {
        let body = changes.status_body(Some("Busy."), &notes, limit).unwrap();
        assert!(body.chars().count() <= limit, "{body}");
        let count = |heading: &str| {
            body.split(heading)
                .nth(1)
                .unwrap()
                .split("\n\n")
                .next()
                .unwrap()
                .lines()
                .filter(|l| l.starts_with("- [example"))
                .count()
        };
        assert_eq!(
            (count("**Closed**"), count("**Opened**")),
            (closed, opened),
            "{body}"
        );
        assert!(body.contains("/100) Item 100 — Kept."), "{body}");
        let hidden = 13 - closed - opened;
        assert_eq!(body.contains(" more\n"), hidden > 0, "{body}");
    }
    let body = changes.status_body(Some("Busy."), &notes, limit).unwrap();
    assert!(body.contains("- [example/project#100]"));
    assert!(body.ends_with("- +1 more\n"), "{body}");
    assert!(changes.status_body(Some("Busy."), &notes, 20).is_err());
}

/// Project pages by query, in order; records each read's variables.
struct Pages {
    pages: RefCell<Vec<Value>>,
    variables: RefCell<Vec<Value>>,
}

impl Transport for Pages {
    fn graphql(&self, _: &str, variables: Value) -> Result<Value> {
        self.variables.borrow_mut().push(variables);
        Ok(self.pages.borrow_mut().remove(0))
    }
}

#[test]
fn reads_page_the_items_from_the_latest_update() {
    let page = |name: &str, nodes: Value, next: Option<&str>| json!({"node": {name: {"nodes": nodes, "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}}}});
    let mut items = window();
    let rest = items.split_off(6);
    let forge = Pages {
        pages: RefCell::new(vec![
            page(
                "statusUpdates",
                json!([{"createdAt": BEFORE}, {"createdAt": SINCE}]),
                None,
            ),
            json!({"viewer": {"login": "Worker"}}),
            page("items", json!(items), Some("c1")),
            page("items", json!(rest), None),
            // The merged search finds a landing that is no item, and one that is.
            json!({"search": {"nodes": [pr(30, json!({"title": "Not on the board", "merged": true, "mergedAt": IN, "closedAt": IN})),
                pr(12, json!({"merged": true, "mergedAt": IN, "closedAt": IN}))], "pageInfo": {"hasNextPage": false}}}),
            // The created one finds a new pull request, that landing again,
            // and one closed unmerged, which outside the board is no news.
            json!({"search": {"nodes": [pr(31, json!({"createdAt": IN})), pr(30, json!({"merged": true, "mergedAt": IN, "closedAt": IN})),
                pr(32, json!({"createdAt": IN, "closedAt": IN})), {}], "pageInfo": {"hasNextPage": false}}}),
            page("statusUpdates", json!([]), None),
            json!({"viewer": {"login": "worker"}}),
            page("items", json!([]), Some("c2")),
            page("items", json!([]), Some("c2")),
        ]),
        variables: RefCell::new(vec![]),
    };
    let since = last_update(&forge, "P").unwrap().unwrap();
    let expected = WINDOW
        .replace(
            "\n\n**Closed**",
            "\n- [example/project#30](https://github.com/example/project/pull/30) Not on the board\n\n**Closed**",
        )
        .replace(
            "Item 20\n",
            "Item 20\n- [example/project#31](https://github.com/example/project/pull/31) Item 31\n",
        );
    assert_eq!(
        read(&forge, "P", since, None, None, false)
            .unwrap()
            .markdown(),
        expected
    );
    let variables = forge.variables.borrow();
    assert_eq!(
        variables[3],
        json!({"id": "P", "since": SINCE, "cursor": "c1"})
    );
    assert_eq!(
        variables[4..6],
        [
            json!({"q": format!("repo:example/project is:pr merged:>={SINCE}")}),
            json!({"q": format!("repo:example/project is:pr created:>={SINCE}")})
        ]
    );
    drop(variables);
    assert_eq!(last_update(&forge, "P").unwrap(), None);
    // A repeated cursor never completes the list.
    assert!(read(&forge, "P", since, None, None, false).is_err());
}

#[test]
fn public_only_leaves_out_private_repositories() {
    let visible = |mut content: Value, repo: &str, private: bool| {
        let c = if content.get("content").is_some() {
            &mut content["content"]
        } else {
            &mut content
        };
        c["repository"] = json!({"nameWithOwner": repo, "isPrivate": private});
        c["url"] = json!(format!("https://github.com/{repo}/issues/{}", c["number"]));
        content
    };
    let private_ref = json!({"number": 4, "url": "https://github.com/example/secret/issues/4",
        "repository": {"nameWithOwner": "example/secret", "isPrivate": true}});
    let mut landed = visible(
        item(
            "PullRequest",
            12,
            json!({"merged": true, "mergedAt": IN, "closedAt": IN}),
        ),
        "example/project",
        false,
    );
    let mut public_ref = closes(3)["nodes"][0].take();
    public_ref["repository"]["isPrivate"] = json!(false);
    landed["content"]["closingIssuesReferences"]["nodes"] = json!([public_ref, private_ref]);
    let items = vec![
        landed,
        visible(
            item("Issue", 5, json!({"createdAt": IN})),
            "example/secret",
            true,
        ),
    ];
    let page = |nodes: Value| json!({"node": {"items": {"nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}}}});
    let none = json!({"search": {"nodes": [], "pageInfo": {"hasNextPage": false}}});
    let forge = |items: &[Value], searches: usize| Pages {
        pages: RefCell::new(
            [json!({"viewer": {"login": VIEWER}}), page(json!(items))]
                .into_iter()
                .chain(std::iter::repeat_n(none.clone(), searches))
                .collect(),
        ),
        variables: RefCell::new(vec![]),
    };
    let since = parse_time(SINCE).unwrap();
    // Unfiltered, the private issue and reference are listed and searched.
    let all = forge(&items, 4);
    let body = read(&all, "P", since, None, None, false)
        .unwrap()
        .markdown();
    assert!(
        body.contains("example/secret#5") && body.contains("example/secret#4"),
        "{body}"
    );
    assert_eq!(all.variables.borrow().len(), 6);
    let public = forge(&items, 2);
    let body = read(&public, "P", since, None, None, true)
        .unwrap()
        .markdown();
    assert_eq!(
        body,
        "**Landed**\n- [example/project#12](https://github.com/example/project/issues/12) Item 12 \
        (closes [example/project#3](https://github.com/example/project/issues/3))\n"
    );
    // Only the public repository is searched.
    let variables = public.variables.borrow();
    assert_eq!(variables.len(), 4);
    assert!(variables.iter().all(|v| !v.to_string().contains("secret")));
    drop(variables);
    // A visibility the read lacks is not taken for public.
    let unread = vec![item("Issue", 6, json!({"createdAt": IN}))];
    let error = read(&forge(&unread, 2), "P", since, None, None, true).unwrap_err();
    assert!(
        format!("{error:#}").contains("unread visibility of example/project"),
        "{error:#}"
    );
}

#[test]
fn until_ends_the_window() {
    const UNTIL: &str = "2026-10-22T00:00:00Z";
    const LATER: &str = "2026-10-23T00:00:00Z";
    let items = vec![
        // Merged in the window, and merged after it but opened in it.
        item(
            "PullRequest",
            1,
            json!({"merged": true, "mergedAt": IN, "closedAt": IN}),
        ),
        item(
            "PullRequest",
            2,
            json!({"createdAt": IN, "merged": true, "mergedAt": LATER, "closedAt": LATER}),
        ),
        // Opened after the window, and closed after it.
        item("Issue", 3, json!({"createdAt": LATER})),
        item("Issue", 4, json!({"closedAt": LATER})),
        // A hold changed only after the window.
        item(
            "Issue",
            5,
            json!({"timelineItems": event("LabeledEvent", "needs-human", LATER)}),
        ),
    ];
    let open = classify(&items, &[], SINCE, &authors()).unwrap().markdown();
    for n in 1..=5 {
        assert!(open.contains(&format!("example/project#{n}]")), "{open}");
    }
    let bounded = classify_window(&items, &[], SINCE, Some(UNTIL), &authors()).unwrap();
    assert_eq!(
        bounded.markdown(),
        "**Landed**\n- [example/project#1](https://github.com/example/project/pull/1) Item 1\n\n\
        **Opened**\n- [example/project#2](https://github.com/example/project/pull/2) Item 2\n"
    );
    assert_eq!(serde_json::to_value(&bounded).unwrap()["until"], UNTIL);
    // Off the board, a pull request opened in the window is still Opened
    // when it merged or closed after it, and is no news when it closed
    // unmerged in it.
    let searched = [
        pr(
            6,
            json!({"createdAt": IN, "merged": true, "mergedAt": LATER, "closedAt": LATER}),
        ),
        pr(7, json!({"createdAt": IN, "closedAt": LATER})),
        pr(8, json!({"createdAt": IN, "closedAt": IN})),
        pr(9, json!({"createdAt": LATER})),
    ];
    let body = classify_window(&[], &searched, SINCE, Some(UNTIL), &authors())
        .unwrap()
        .markdown();
    assert_eq!(
        body,
        "**Opened**\n- [example/project#6](https://github.com/example/project/pull/6) Item 6\n\
        - [example/project#7](https://github.com/example/project/pull/7) Item 7\n"
    );
    // Without an end, only what is open now is Opened, as before.
    let body = classify(&[], &searched, SINCE, &authors())
        .unwrap()
        .markdown();
    assert!(
        body.starts_with("**Landed**\n- [example/project#6]"),
        "{body}"
    );
    assert!(
        body.contains("**Opened**\n- [example/project#9]") && !body.contains("#7]"),
        "{body}"
    );
    let empty = classify_window(&[], &[], SINCE, Some(UNTIL), &authors()).unwrap();
    assert_eq!(
        empty.markdown(),
        format!("No changes from {SINCE} to {UNTIL}\n")
    );

    // The searches take the window as a range.
    let page = json!({"node": {"items": {"nodes": [items[0].clone()], "pageInfo": {"hasNextPage": false, "endCursor": null}}}});
    let none = json!({"search": {"nodes": [], "pageInfo": {"hasNextPage": false}}});
    let forge = Pages {
        pages: RefCell::new(vec![
            json!({"viewer": {"login": VIEWER}}),
            page,
            none.clone(),
            none,
        ]),
        variables: RefCell::new(vec![]),
    };
    let (since, until) = (parse_time(SINCE).unwrap(), parse_time(UNTIL).unwrap());
    read(&forge, "P", since, Some(until), None, false).unwrap();
    assert_eq!(
        forge.variables.borrow()[2],
        json!({"q": format!("repo:example/project is:pr merged:{SINCE}..{UNTIL}")})
    );
    assert!(read(&forge, "P", until, Some(since), None, false).is_err());
}

#[test]
fn markdown_within_trims_like_a_status_body() {
    let changes = classify(&window(), &[], SINCE, &authors()).unwrap();
    let whole = changes.markdown();
    assert_eq!(
        changes.markdown_within(whole.chars().count()).unwrap(),
        whole
    );
    let short = changes.markdown_within(whole.chars().count() - 1).unwrap();
    assert!(short.chars().count() < whole.chars().count());
    assert!(short.contains("- +1 more\n"), "{short}");
    assert_eq!(
        short,
        changes
            .status_body(None, &BTreeMap::new(), whole.chars().count() - 1)
            .unwrap()
    );
    assert!(changes.markdown_within(10).is_err());
}

#[test]
fn a_long_landed_section_groups_by_repository() {
    let landed = |repo: &str, n: u64| {
        let mut pr = pr(n, json!({"merged": true, "mergedAt": IN, "closedAt": IN}));
        pr["repository"]["nameWithOwner"] = json!(repo);
        pr["url"] = json!(format!("https://github.com/{repo}/pull/{n}"));
        pr
    };
    let few: Vec<_> = (1..=8).map(|n| landed("example/b", n)).collect();
    let mut many = few.clone();
    many.push(landed("example/a", 9));
    for (searched, grouped) in [(few, false), (many, true)] {
        let body = classify(&[], &searched, SINCE, &authors())
            .unwrap()
            .markdown();
        assert_eq!(
            body.contains("- example/b\n  - [example/b#1]"),
            grouped,
            "{body}"
        );
        if grouped {
            assert!(
                body.starts_with("**Landed**\n- example/a\n  - [example/a#9]"),
                "{body}"
            );
        }
    }
}

#[test]
fn search_found_pull_requests_by_author() {
    let landed = json!({"merged": true, "mergedAt": IN, "closedAt": IN});
    let opened = json!({"createdAt": IN});
    let user = |login: &str| json!({"__typename": "User", "login": login});
    let bot = |login: &str| json!({"__typename": "Bot", "login": login});
    // (author, fields, the line the pull request gets, if any)
    for (author, fields, line) in [
        (
            user(VIEWER),
            &opened,
            Some("**Opened**\n- [example/project#40]"),
        ),
        (
            user("Owner"),
            &opened,
            Some("**Opened**\n- [example/project#40]"),
        ),
        (user("stranger"), &opened, None),
        (Value::Null, &opened, None),
        (
            user("stranger"),
            &landed,
            Some("**Landed**\n- [example/project#40]"),
        ),
        (
            bot("github-actions"),
            &landed,
            Some("**Landed**\n- 1 dependency and bot pull requests in example/project\n"),
        ),
        (
            user("renovate"),
            &opened,
            Some("**Opened**\n- 1 dependency and bot pull requests in example/project\n"),
        ),
        (
            user("dependabot[bot]"),
            &landed,
            Some("**Landed**\n- 1 dependency and bot pull requests in example/project\n"),
        ),
    ] {
        let mut found = pr(40, fields.clone());
        found["author"] = author.clone();
        let body = classify(&[], &[found], SINCE, &authors())
            .unwrap()
            .markdown();
        match line {
            Some(line) => assert!(body.starts_with(line), "{author}: {body}"),
            None => assert_eq!(body, format!("No changes since {SINCE}\n"), "{author}"),
        }
    }
    // A project item is listed whoever wrote it; bots in several
    // repositories share one count line, after the section's own lines.
    let mut item = item("PullRequest", 41, landed.clone());
    item["content"]["author"] = bot("renovate");
    let elsewhere = |n: u64, repo: &str| {
        let mut found = pr(n, landed.clone());
        found["author"] = bot("renovate");
        found["repository"]["nameWithOwner"] = json!(repo);
        found
    };
    let body = classify(
        &[item],
        &[elsewhere(42, "example/b"), elsewhere(43, "example/a")],
        SINCE,
        &authors(),
    )
    .unwrap()
    .markdown();
    assert_eq!(
        body,
        "**Landed**\n- [example/project#41](https://github.com/example/project/pull/41) Item 41\n\
        - 2 dependency and bot pull requests in example/a, example/b\n"
    );
}
