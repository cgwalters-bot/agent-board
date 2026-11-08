//! Stop bot-written or changed text being published under a human-text policy.
//! Missing authorship is not operator authorship, including on a bot-created PR.
use super::*;
use serde_json::json;

// Whole or refuse: at most 100 edits and 100 rename events in one budgeted read.
// GitHub's filtered timeline totalCount includes other event types; pageInfo,
// not equality with that count, proves the filtered connection is complete.
pub(super) const QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){number url headRefOid title body lastEditedAt editor{login} userContentEdits(first:100){totalCount pageInfo{hasNextPage endCursor} nodes{editedAt deletedAt editor{login}}} timelineItems(first:100,itemTypes:[RENAMED_TITLE_EVENT]){totalCount pageInfo{hasNextPage endCursor} nodes{... on RenamedTitleEvent{id createdAt previousTitle currentTitle actor{login}}}}}}}";

fn complete<'a>(connection: &'a Value, name: &str, edits: bool) -> Result<&'a [Value]> {
    let nodes = connection["nodes"]
        .as_array()
        .with_context(|| format!("missing {name}"))?;
    ensure!(
        connection["pageInfo"]["hasNextPage"] == false
            && connection["pageInfo"].get("endCursor").is_some()
            && nodes.len() < PAGE_SIZE,
        "incomplete {name}"
    );
    let count = connection["totalCount"]
        .as_u64()
        .with_context(|| format!("missing {name} count"))?;
    ensure!(
        (!edits && count >= nodes.len() as u64) || count == nodes.len() as u64,
        "incomplete {name}"
    );
    Ok(nodes)
}

fn operator(actor: &Value, config: &Config) -> bool {
    actor["login"]
        .as_str()
        .is_some_and(|s| s.eq_ignore_ascii_case(&config.operator))
}

pub(super) fn verify(
    client: &impl Transport,
    config: &Config,
    repo: &str,
    pull: &Value,
    command: &Command,
) -> Result<()> {
    hold!(command.human_text, "selected command lacks --human-text");
    ensure!(
        command.author.eq_ignore_ascii_case(&config.operator)
            && pull["head"]["sha"] == command.head
            && command.pushed < command.created,
        "command head/push binding mismatch"
    );
    hold!(
        command
            .pusher
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(&config.operator)),
        "current-head pusher is not the operator or is missing"
    );
    let body = pull["body"].as_str().context("missing REST body")?;
    hold!(
        !body.to_ascii_lowercase().contains("generated-by:"),
        "body still has Generated-by attribution"
    );
    let (owner, name) = repo.split_once('/').context("invalid repository")?;
    let number = pull["number"].as_u64().context("missing PR number")?;
    let data = client
        .graphql(QUERY, json!({"owner":owner,"name":name,"number":number}))
        .context("unreadable text evidence")?;
    let p = &data["repository"]["pullRequest"];
    ensure!(
        p["number"] == number
            && p["url"] == format!("https://github.com/{repo}/pull/{number}")
            && p["headRefOid"] == command.head
            && p["title"] == pull["title"]
            && p["body"] == pull["body"],
        "REST/GraphQL PR/head/text disagreement"
    );
    let edits = complete(&p["userContentEdits"], "body edit history", true)?;
    hold!(!edits.is_empty(), "missing body edit proof");
    let mut previous = None;
    for edit in edits {
        let t = at(&edit["editedAt"]).context("unverifiable body edit time")?;
        ensure!(
            edit.get("deletedAt") == Some(&Value::Null)
                && edit["editor"]["login"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                && previous.is_none_or(|prev| t < prev),
            "deleted, ambiguous or unverifiable body edit history"
        );
        previous = Some(t);
    }
    let latest = &edits[0];
    hold!(
        operator(&latest["editor"], config) && operator(&p["editor"], config),
        "last body editor is not the operator or is missing"
    );
    let edited = at(&latest["editedAt"])?;
    ensure!(
        at(&p["lastEditedAt"])? == edited,
        "body edit metadata disagreement"
    );
    hold!(
        edited < command.created,
        "body edit does not predate command; fresh command required"
    );

    let renames = complete(&p["timelineItems"], "title rename timeline", false)?;
    hold!(!renames.is_empty(), "missing title rename proof");
    let mut ids = BTreeSet::new();
    let mut previous_time = None;
    let mut previous_title = None;
    for rename in renames {
        let id = rename["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("missing title rename identity")?;
        let t = at(&rename["createdAt"])?;
        let before = rename["previousTitle"]
            .as_str()
            .context("missing previous title")?;
        let after = rename["currentTitle"]
            .as_str()
            .context("missing renamed title")?;
        ensure!(
            ids.insert(id)
                && previous_time.is_none_or(|prev| t > prev)
                && previous_title.is_none_or(|prev| before == prev),
            "ambiguous title rename timeline"
        );
        previous_time = Some(t);
        previous_title = Some(after);
    }
    let latest = &renames[renames.len() - 1];
    hold!(
        operator(&latest["actor"], config),
        "last title renamer is not the operator or is missing"
    );
    ensure!(
        latest["currentTitle"] == p["title"],
        "title rename/current title disagreement"
    );
    hold!(
        at(&latest["createdAt"])? < command.created,
        "title rename does not predate command; fresh command required"
    );
    Ok(())
}
