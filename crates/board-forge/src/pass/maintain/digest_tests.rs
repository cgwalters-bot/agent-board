use super::*;

fn markdown(value: &Value) -> String {
    format!("```{DIGEST_FENCE}\n{value}\n```\n")
}

#[test]
fn candidate_contract_tables() {
    let valid = example(crate::cost::parse_date("2026-10-23").unwrap());
    validate_digest(include_str!(
        "../../../../../fixtures/pass/maintain-digest.md"
    ))
    .unwrap();
    validate_digest(&markdown(&valid)).unwrap();
    for (pointer, bad) in [
        ("/schema", json!("wrong")),
        ("/date", json!("2026-02-29")),
        ("/date", json!("2026-1-01")),
        ("/approve", json!({})),
        ("/closer", Value::Null),
        ("/waiting", json!("none")),
        ("/stale", json!(0)),
        ("/approve/0", json!("not an object")),
        ("/approve/0/pr", json!(7)),
        ("/closer/0/pr", json!(9)),
        ("/waiting/0/pr", json!(10)),
        ("/stale/0/pr", json!(3)),
        ("/approve/0/pr", json!("7")),
        ("/closer/0/pr", json!("proj#9")),
        ("/waiting/0/pr", json!("up/proj#0")),
        ("/stale/0/pr", json!("https://github.com/up/proj/pull/3")),
        ("/approve/0/head", json!("111111111111")),
        ("/closer/0/head", json!("G".repeat(40))),
        ("/approve/0/head", json!("A".repeat(40))),
        ("/approve/0/reason", json!(" \n")),
        ("/closer/0/concern", json!(false)),
        ("/waiting/0/reply", json!("")),
        ("/stale/0/reason", Value::Null),
        ("/closer/0/minutes", json!(0)),
        ("/closer/0/minutes", json!(481)),
        ("/closer/0/minutes", json!(1.5)),
        ("/closer/0/links", json!([])),
        ("/closer/0/links", json!({})),
        ("/closer/0/links/0", json!(null)),
        ("/closer/0/links/0/path", json!("../src/lib.rs")),
        ("/closer/0/links/0/path", json!("/src/lib.rs")),
        ("/closer/0/links/0/path", json!("https://example.com/file")),
        ("/closer/0/links/0/path", json!("src\\lib.rs")),
        ("/closer/0/links/0/start", json!(0)),
        ("/closer/0/links/0/start", json!("12")),
        ("/closer/0/links/0/end", json!(11)),
        ("/closer/0/links/0/end", Value::Null),
        ("/stale/0/pr", json!("UP/PROJ#7")),
        ("/approve/1/pr", json!("up/proj#7")),
    ] {
        let mut value = valid.clone();
        *value.pointer_mut(pointer).unwrap() = bad;
        let error = validate_digest(&markdown(&value)).unwrap_err().to_string();
        let field = pointer
            .trim_start_matches('/')
            .replace("/0", "[0]")
            .replace("/1", "[1]")
            .replace('/', ".");
        assert!(error.contains(&field), "{pointer}: {error}");
    }
    for (parent, field) in [
        ("", "schema"),
        ("", "date"),
        ("", "approve"),
        ("", "closer"),
        ("", "waiting"),
        ("", "stale"),
        ("/approve/0", "pr"),
        ("/approve/0", "head"),
        ("/approve/0", "reason"),
        ("/closer/0", "head"),
        ("/closer/0", "concern"),
        ("/closer/0", "minutes"),
        ("/closer/0", "links"),
        ("/closer/0/links/0", "path"),
        ("/closer/0/links/0", "start"),
        ("/waiting/0", "reply"),
        ("/stale/0", "reason"),
    ] {
        let mut value = valid.clone();
        value
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(field);
        let error = validate_digest(&markdown(&value)).unwrap_err().to_string();
        assert!(error.contains(field), "{parent}/{field}: {error}");
    }
    for (group, _) in DIGEST_GROUPS {
        let mut partial = valid.clone();
        for (empty, _) in DIGEST_GROUPS {
            if empty != group {
                partial[*empty] = json!([]);
            }
        }
        validate_digest(&markdown(&partial)).unwrap();
        let mut oversized = partial.clone();
        oversized[*group] = Value::Array(vec![partial[*group][0].clone(); DIGEST_GROUP_LIMIT + 1]);
        assert!(
            validate_digest(&markdown(&oversized))
                .unwrap_err()
                .to_string()
                .contains("exceeds 100")
        );
    }
    let mut empty = valid;
    for (group, _) in DIGEST_GROUPS {
        empty[*group] = json!([]);
    }
    validate_digest(&markdown(&empty)).unwrap();
}

#[test]
fn candidate_block_tables() {
    let valid = markdown(&example(crate::cost::parse_date("2024-02-29").unwrap()));
    for body in [
        String::new(),
        "```json\n{}\n```".into(),
        format!("```{DIGEST_FENCE}\n{{}}"),
        format!("```{DIGEST_FENCE}\nnot JSON\n```"),
        format!("```{DIGEST_FENCE}\n[]\n```"),
        format!("{valid}\n{valid}"),
        format!("{valid}{}", "x".repeat(DIGEST_MAX_BYTES)),
    ] {
        assert!(validate_digest(&body).is_err());
    }
    validate_digest(&valid).unwrap();
}
