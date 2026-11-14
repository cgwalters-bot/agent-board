//! Explicit, bounded issue relocation. No pass stage invokes this module.
use super::*;
use crate::github::Transport;

const ISSUE_PAGES: usize = 20;
const ACTIVE: &[&str] = &["requested", "pending", "waiting", "queued", "in_progress"];
const BOARD: &str = r#"query MigrationBoard($owner:String!, $number:Int!, $field:String!, $cursor:String) {
  repositoryOwner(login:$owner) {
    ... on Organization { projectV2(number:$number) { ...MigrationProject } }
    ... on User { projectV2(number:$number) { ...MigrationProject } }
  }
}
fragment MigrationProject on ProjectV2 {
  id number field(name:$field) { ... on ProjectV2SingleSelectField { id name } }
  items(first:100, after:$cursor) {
    pageInfo { hasNextPage endCursor }
    nodes { id content { __typename ... on Issue { id number state repository { nameWithOwner } } } }
  }
}"#;
const SNAPSHOT: &str = r#"query MigrationIssue($id:ID!, $item:ID!) {
  node(id:$id) { __typename ... on Issue {
    id number url title body updatedAt state repository { nameWithOwner }
    parent { id url }
    subIssues(first:1) { totalCount }
    milestone { title dueOn }
    labels(first:100) { nodes { name } pageInfo { hasNextPage } }
    assignees(first:100) { nodes { id } pageInfo { hasNextPage } }
    closedByPullRequestsReferences(first:100, includeClosedPrs:false) {
      totalCount pageInfo { hasNextPage } nodes { state number updatedAt repository { nameWithOwner } author { login } }
    }
    timelineItems(first:100, itemTypes:[CROSS_REFERENCED_EVENT]) {
      pageInfo { hasNextPage } nodes { __typename ... on CrossReferencedEvent {
        source { __typename ... on PullRequest { state } }
      } }
    }
  } }
  board: node(id:$item) { __typename ... on ProjectV2Item {
      id project { id number owner { ... on Organization { login } ... on User { login } } }
      content { ... on Issue { id } }
       fieldValues(first:100) { pageInfo { hasNextPage } nodes { __typename
        ... on ProjectV2ItemFieldSingleSelectValue { optionId name field { ... on ProjectV2SingleSelectField { name } } }
      } }
  } }
}"#;
const TRANSFER: &str = r#"mutation MigrationTransfer($issue:ID!, $repository:ID!) {
  transferIssue(input:{issueId:$issue,repositoryId:$repository}) { issue { id number url } }
}"#;
const DESTINATION: &str = r#"query MigrationMetadata($owner:String!, $name:String!) {
  repository(owner:$owner, name:$name) {
    nameWithOwner
    labels(first:100) { nodes { name } pageInfo { hasNextPage } }
    milestones(first:100) { nodes { title dueOn } pageInfo { hasNextPage } }
  }
}"#;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Before {
    id: String,
    source: String,
    number: u64,
    body: String,
    title: String,
    updated: String,
    board: Value,
    parent: Value,
    labels: BTreeSet<String>,
    assignees: BTreeSet<String>,
    milestone: Value,
}

/// A pending entry is saved *before* the first forge write. Its stable node ID
/// reconciles timeouts and changed issue numbers without issuing another transfer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    before: Before,
    destination: String,
    upstream: Option<u64>,
    canonical: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    receipts: BTreeMap<String, Receipt>,
}

#[derive(Clone, Debug)]
struct Fork {
    repository: String,
    parent: String,
    id: String,
    refusal: Option<String>,
}

fn required<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("missing {key}"))
}

fn get(client: &impl Rest, path: &str) -> Result<Value> {
    let response = client.get_live(&format!("{API_ROOT}{path}"))?;
    ensure!(response.status == 200, "{path}: HTTP {}", response.status);
    Ok(response.body)
}

fn connection<'a>(v: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    let v = &v[key];
    ensure!(
        v.pointer("/pageInfo/hasNextPage") == Some(&json!(false)),
        "incomplete {key}"
    );
    v["nodes"]
        .as_array()
        .with_context(|| format!("missing {key} nodes"))
}

fn snapshot(client: &impl Transport, id: &str, item: &str) -> Result<Value> {
    let data = client.graphql(SNAPSHOT, json!({"id":id, "item":item}))?;
    ensure!(data.get("errors").is_none(), "migration GraphQL errors");
    let node = data.get("node").context("missing issue node")?;
    ensure!(
        node["__typename"] == "Issue" && node["id"] == id,
        "issue node identity mismatch"
    );
    let board = &data["board"];
    ensure!(
        board["__typename"] == "ProjectV2Item"
            && board["id"] == item
            && board["content"]["id"] == id,
        "board item identity or content changed"
    );
    let mut node = node.clone();
    node["board"] = board.clone();
    Ok(node)
}

/// Activity prevents starting a transfer, not recognizing a completed one.
/// Recovery only reads identity and preservation evidence; it never retries.
fn idle_references(node: &Value) -> Result<()> {
    let references = &node["closedByPullRequestsReferences"];
    let closing = connection(node, "closedByPullRequestsReferences")?;
    ensure!(
        closing.iter().all(|reference| reference["state"]
            .as_str()
            .is_some_and(|state| ["OPEN", "CLOSED", "MERGED"].contains(&state))),
        "in-flight or unreadable linked PRs"
    );
    let (open, read, total) = pulls(references)?;
    ensure!(
        read == total && open.is_empty(),
        "in-flight or unreadable linked PRs"
    );
    for event in connection(node, "timelineItems")? {
        ensure!(
            event["__typename"] == "CrossReferencedEvent",
            "incomplete cross-reference evidence"
        );
        let source = &event["source"];
        let kind = required(source, "__typename")?;
        if kind == "PullRequest" {
            ensure!(
                ["CLOSED", "MERGED"].contains(&required(source, "state")?),
                "in-flight cross-referenced PR"
            );
        } else {
            ensure!(kind == "Issue", "unknown cross-reference source type");
        }
    }
    Ok(())
}

type IssueItems = BTreeMap<u64, (String, String)>;

fn issues(client: &impl Transport, config: &Config) -> Result<(String, IssueItems)> {
    let board = config
        .github_board()
        .context("migration needs a GitHub board")?;
    let mut cursor = Value::Null;
    let mut identity = None;
    let mut found = BTreeMap::new();
    for _ in 0..ISSUE_PAGES {
        let data = client
            .graphql(
                BOARD,
                json!({"owner":board.owner, "number":board.project,
                "field":board.priority_field, "cursor":cursor}),
            )
            .context("read project issue listing; migration preview incomplete")?;
        let project = &data["repositoryOwner"]["projectV2"];
        let id = required(project, "id")
            .context("configured project inaccessible; migration preview incomplete")?;
        ensure!(
            project["number"].as_u64() == Some(board.project)
                && project["field"]["name"] == board.priority_field,
            "project identity or Priority schema incomplete"
        );
        required(&project["field"], "id")?;
        if let Some(previous) = &identity {
            ensure!(previous == id, "project identity changed");
        }
        identity = Some(id.to_owned());
        for item in project["items"]["nodes"]
            .as_array()
            .context("missing project items")?
        {
            let issue = &item["content"];
            if issue["__typename"] != "Issue" || issue["state"] != "OPEN" {
                continue;
            }
            let repository = required(&issue["repository"], "nameWithOwner")?;
            if !repository.eq_ignore_ascii_case(&config.dispatch_repo) {
                continue;
            }
            let number = issue["number"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("missing project issue number")?;
            ensure!(
                found
                    .insert(
                        number,
                        (required(issue, "id")?.into(), required(item, "id")?.into())
                    )
                    .is_none(),
                "duplicate source issue on project"
            );
        }
        match project
            .pointer("/items/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
        {
            Some(false) => return Ok((id.to_owned(), found)),
            Some(true) => {
                let next = project
                    .pointer("/items/pageInfo/endCursor")
                    .filter(|c| c.is_string())
                    .context("missing project cursor")?;
                ensure!(next != &cursor, "project pagination did not advance");
                cursor = next.clone();
            }
            None => bail!("incomplete project pageInfo"),
        }
    }
    bail!("incomplete project issue listing")
}

fn before(node: &Value, config: &Config) -> Result<Before> {
    // Closing a transferred issue must not strand its pending receipt. Admission
    // checks openness separately; recovery still requires a known issue state.
    ensure!(
        matches!(node["state"].as_str(), Some("OPEN" | "CLOSED")),
        "missing or invalid issue state"
    );
    let board = config
        .github_board()
        .context("migration needs a GitHub board")?;
    let labels = connection(node, "labels")?
        .iter()
        .map(|label| required(label, "name").map(str::to_owned))
        .collect::<Result<BTreeSet<_>>>()?;
    let milestone = node
        .get("milestone")
        .context("missing milestone evidence")?;
    // Stable user IDs avoid treating a login rename or connection reordering as
    // ownership loss. Missing evidence must never mean an unassigned issue.
    let assignees = connection(node, "assignees")?
        .iter()
        .map(|assignee| required(assignee, "id").map(str::to_owned))
        .collect::<Result<BTreeSet<_>>>()?;
    if !milestone.is_null() {
        validate_milestone(milestone)?;
    }
    let item = node.get("board").context("missing board item")?;
    ensure!(
        item.pointer("/project/number").and_then(Value::as_u64) == Some(board.project)
            && item
                .pointer("/project/owner/login")
                .and_then(Value::as_str)
                .is_some_and(|o| o.eq_ignore_ascii_case(&board.owner)),
        "board item is outside configured project"
    );
    required(item, "id")?;
    required(&item["project"], "id")?;
    let mut priority = Vec::new();
    for value in connection(item, "fieldValues")? {
        // An unreadable entry could be Priority, not proof that it is unset.
        let kind = required(value, "__typename").context("unreadable project field value")?;
        if kind != "ProjectV2ItemFieldSingleSelectValue" {
            continue;
        }
        let name = required(&value["field"], "name").context("unreadable single-select field")?;
        if name == board.priority_field {
            let option = required(value, "optionId").context("unreadable Priority")?;
            let value_name = required(value, "name").context("unreadable Priority")?;
            // Keep the existing journal representation: the query's new type
            // discriminator must not strand pending receipts from older runs.
            priority.push(json!({"optionId":option, "name":value_name, "field":{"name":name}}));
        }
    }
    ensure!(priority.len() <= 1, "ambiguous Priority");
    let board = json!({"item":item["id"], "project":item["project"]["id"], "priority":priority});
    let parent = node
        .get("parent")
        .context("missing parent evidence")?
        .clone();
    parent_id(&parent)?;
    let source = required(&node["repository"], "nameWithOwner")?.to_owned();
    validate_repository(&source)?;
    let number = node["number"]
        .as_u64()
        .filter(|n| *n > 0)
        .context("missing issue number")?;
    ensure!(
        node["url"] == format!("https://github.com/{source}/issues/{number}"),
        "issue URL mismatch"
    );
    Ok(Before {
        id: required(node, "id")?.into(),
        source,
        number,
        body: required_body(node)?.into(),
        title: required(node, "title")?.into(),
        updated: required(node, "updatedAt")?.into(),
        board,
        parent,
        labels,
        assignees,
        milestone: milestone.clone(),
    })
}

fn parent_id(parent: &Value) -> Result<Option<&str>> {
    if parent.is_null() {
        return Ok(None);
    }
    required(parent, "url")?;
    Ok(Some(required(parent, "id")?))
}

fn validate_milestone(milestone: &Value) -> Result<()> {
    required(milestone, "title")?;
    let due = milestone
        .get("dueOn")
        .context("missing milestone due date")?;
    ensure!(
        due.is_null() || due.is_string(),
        "invalid milestone due date"
    );
    Ok(())
}

fn compatible_metadata(client: &impl Transport, before: &Before, fork: &Fork) -> Result<()> {
    let (owner, name) = fork
        .repository
        .split_once('/')
        .context("invalid fork name")?;
    let data = client.graphql(DESTINATION, json!({"owner":owner,"name":name}))?;
    ensure!(
        data.get("errors").is_none(),
        "destination metadata GraphQL errors"
    );
    let repository = &data["repository"];
    ensure!(
        repository["nameWithOwner"] == fork.repository,
        "destination metadata identity mismatch"
    );
    let labels = connection(repository, "labels")?
        .iter()
        .map(|label| required(label, "name"))
        .collect::<Result<BTreeSet<_>>>()?;
    ensure!(
        before
            .labels
            .iter()
            .all(|label| labels.contains(label.as_str())),
        "destination missing source labels"
    );
    if !before.milestone.is_null() {
        let milestones = connection(repository, "milestones")?;
        for milestone in milestones {
            validate_milestone(milestone)?;
        }
        ensure!(
            milestones.contains(&before.milestone),
            "destination missing matching milestone title and due date"
        );
    }
    Ok(())
}

fn required_body(node: &Value) -> Result<&str> {
    node["body"].as_str().context("missing issue body")
}

fn discover_forks(client: &impl Rest, config: &Config) -> Result<Vec<Fork>> {
    let midstream = config
        .midstream
        .as_ref()
        .context("midstream is not configured")?;
    let repositories = config
        .resolved_midstreams
        .clone()
        .map(Ok)
        .unwrap_or_else(|| midstream.read(client))?;
    ensure!(
        repositories.len() <= POLL_REPOSITORY_LIMIT,
        "too many midstreams"
    );
    let mut forks = Vec::new();
    for repository in repositories {
        let result = (|| -> Result<Fork> {
            let metadata = get(client, &format!("repos/{repository}"))?;
            ensure!(
                required(&metadata, "full_name")?.eq_ignore_ascii_case(&repository)
                    && metadata["fork"] == true,
                "incomplete fork metadata"
            );
            ensure!(
                metadata["topics"]
                    .as_array()
                    .context("missing fork topics")?
                    .iter()
                    .any(|t| t == &midstream.topic),
                "midstream topic admission changed"
            );
            ensure!(
                metadata["archived"] == false,
                "destination archived or archive metadata incomplete"
            );
            let parent = required(&metadata["parent"], "full_name")?.to_owned();
            validate_repository(&parent)?;
            ensure!(
                !parent.eq_ignore_ascii_case(&repository),
                "fork is its own parent"
            );
            let enabled = metadata["has_issues"]
                .as_bool()
                .context("missing has_issues")?;
            Ok(Fork {
                repository: repository.clone(),
                parent,
                id: required(&metadata, "node_id")?.into(),
                refusal: (!enabled).then(|| "destination issues disabled".into()),
            })
        })();
        forks.push(result.unwrap_or_else(|error| Fork {
            repository,
            parent: String::new(),
            id: String::new(),
            refusal: Some(format!(
                "destination inaccessible or metadata incomplete: {error:#}"
            )),
        }));
    }
    Ok(forks)
}

/// Every explicit target contributes evidence, including unknown targets. There
/// is no precedence that can silently override a conflicting accepted plan.
fn classify(
    node: &Value,
    comments: &[plan::Comment],
    config: &Config,
    forks: &[Fork],
) -> Result<usize> {
    ensure!(node["state"] == "OPEN", "issue is not open");
    // An epic can have a plan before its first child exists. Moving that
    // coordinator would strand the plan's tracker identity even without labels.
    ensure!(
        !plan::has_fence(required_body(node)?, crate::epic::EPIC_PLAN_FENCE)
            && plan::no_fenced(
                comments,
                &[&config.operator, &config.bot],
                crate::epic::EPIC_PLAN_FENCE,
            ),
        "excluded issue: epic plan"
    );
    // A project-prefixed parent is still an epic even without an epic label.
    // Read only the count; enumerating children would add no routing evidence.
    let children = node["subIssues"]["totalCount"]
        .as_u64()
        .context("missing or invalid sub-issue count")?;
    ensure!(children == 0, "excluded issue: parent with sub-issues");
    for label in connection(node, "labels")? {
        let label = required(label, "name")?;
        ensure!(
            ![
                "epic",
                "question",
                "infrastructure",
                "harness",
                "operator-decision",
                "needs-operator-approval",
                "needs-human",
                "needs-design"
            ]
            .iter()
            .any(|excluded| label.eq_ignore_ascii_case(excluded)),
            "excluded issue: {label}"
        );
        if let Some(harness) = config
            .operator_ask
            .as_ref()
            .and_then(|a| a.harness.as_ref())
        {
            ensure!(
                !harness
                    .excluded_labels
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case(label)),
                "excluded issue: {label}"
            );
        }
    }
    let mut targets = Vec::new();
    for label in connection(node, "labels")? {
        if let Some((key, target)) = required(label, "name")?.split_once(':')
            && key.eq_ignore_ascii_case("target")
        {
            targets.push(target.trim().to_owned());
        }
    }
    for line in required_body(node)?.lines().map(str::trim) {
        if let Some((key, value)) = line.split_once(':')
            && ["target", "repo"]
                .iter()
                .any(|k| key.eq_ignore_ascii_case(k))
        {
            targets.push(value.trim().to_owned());
        }
    }
    let writers = [&*config.operator, &*config.bot];
    if !plan::no_plan(comments, &writers) {
        let (plan, _) = plan::newest_plan(comments, &writers)?;
        ensure!(
            node["number"].as_u64() == Some(plan.item),
            "accepted plan issue number mismatch"
        );
        if !plan.repo.eq_ignore_ascii_case(&config.dispatch_repo) {
            targets.push(plan.repo.clone());
        }
        if !plan
            .work_repository()
            .eq_ignore_ascii_case(&config.dispatch_repo)
        {
            targets.push(plan.work_repository().into());
        }
    }
    if let Some((prefix, _)) = required(node, "title")?.split_once(':') {
        // Only a known fork prefix is evidence; arbitrary title prose is not.
        if forks
            .iter()
            .any(|f| short(&f.repository).eq_ignore_ascii_case(prefix.trim()))
        {
            targets.push(prefix.trim().into());
        }
    }
    ensure!(!targets.is_empty(), "no structured target evidence");
    let mut selected = BTreeSet::new();
    for target in targets {
        let matches = forks
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                [&*f.repository, &*f.parent, short(&f.repository)]
                    .iter()
                    .any(|s| !s.is_empty() && s.eq_ignore_ascii_case(&target))
            })
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        ensure!(
            matches.len() == 1,
            "ambiguous or unadmitted target {target:?}"
        );
        selected.insert(matches[0]);
    }
    ensure!(selected.len() == 1, "conflicting target evidence");
    selected.into_iter().next().context("no target")
}

fn short(repository: &str) -> &str {
    repository.split_once('/').map_or(repository, |(_, r)| r)
}

fn unplanned(node: &Value, comments: &[plan::Comment], config: &Config) -> Result<()> {
    // Body plans can become executable too. Hold even when bodyPlans is off:
    // preserving a stale plan would break a later enablement of that setting.
    ensure!(
        !plan::has_fence(required_body(node)?, PLAN_FENCE)
            && plan::no_plan(comments, &[&config.operator, &config.bot]),
        "source plan requires replacement-plan handoff before migration"
    );
    Ok(())
}

fn association(body: &str, fork: &Fork) -> Result<Option<u64>> {
    // Reuse #949's grammar. An existing number refers only to this fork's
    // validated parent. Qualified Upstream: syntax is deliberately refused.
    let explicit = crate::upstream::parse(body)
        .with_context(|| format!("upstream association for {}", fork.parent))?;
    let mut numbers = BTreeSet::new();
    for (offset, prefix) in body.match_indices("https://github.com/") {
        // Keep the original suffix: a second URL is not an issue-number
        // delimiter and must not make a malformed first URL look valid.
        let reference = &body[offset + prefix.len()..];
        let Some((repository, tail)) = reference.split_once("/issues/") else {
            continue;
        };
        // GitHub repository names are case-insensitive. A differently cased
        // link must not hide an association or bypass conflicting-link holds.
        if !repository.eq_ignore_ascii_case(&fork.parent) {
            continue;
        }
        let digits = tail
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        ensure!(!digits.is_empty(), "malformed upstream issue reference");
        // Do not turn a malformed path (42oops, 42/43, 42%30) into an
        // association with issue 42. Allow query/fragment and prose/Markdown
        // boundaries, but never silently truncate the issue path component.
        let suffix = &tail[digits.len()..];
        ensure!(
            suffix.starts_with(['?', '#'])
                || suffix
                    .trim_start_matches(['.', ',', ';', ':', '!'])
                    .chars()
                    .next()
                    .is_none_or(|c| {
                        c.is_whitespace() || matches!(c, ')' | ']' | '>' | '\"' | '\'' | '`')
                    }),
            "malformed upstream issue reference: invalid suffix after issue number"
        );
        let number = digits
            .parse::<u64>()
            .context("upstream reference overflow")?;
        ensure!(number > 0, "upstream reference must be positive");
        numbers.insert(number);
    }
    if let Some(number) = explicit {
        numbers.insert(number);
    }
    ensure!(numbers.len() <= 1, "conflicting upstream references");
    Ok(numbers.into_iter().next())
}

fn idle(client: &impl Rest, config: &Config, fork: &Fork) -> Result<()> {
    // Conservative: any open PR or active run in either repository holds all
    // transfers. This avoids inferring safety from incomplete cross-repo links.
    let repos = BTreeSet::from([
        config.dispatch_repo.as_str(),
        fork.repository.as_str(),
        fork.parent.as_str(),
    ]);
    for repo in repos {
        let pulls = get(client, &format!("repos/{repo}/pulls?state=open&per_page=1"))?;
        ensure!(
            pulls.as_array().context("missing pull list")?.is_empty(),
            "in-flight work: open PR in {repo}"
        );
        for status in ACTIVE {
            let runs = get(
                client,
                &format!("repos/{repo}/actions/runs?status={status}&per_page=1"),
            )?;
            ensure!(
                runs["total_count"].as_u64() == Some(0)
                    && runs["workflow_runs"].as_array().is_some_and(Vec::is_empty),
                "in-flight or incomplete job evidence in {repo} ({status})"
            );
        }
    }
    Ok(())
}

fn preserved(before: &Before, after: &Before, destination: &str) -> Result<String> {
    ensure!(
        after.id == before.id && after.source.eq_ignore_ascii_case(destination),
        "transfer identity not reconciled"
    );
    ensure!(
        after.board == before.board,
        "transfer lost board identity or Priority"
    );
    // A repository rename can change the parent's URL while an intent is
    // pending. Retain the original evidence in the journal, but compare identity.
    ensure!(
        parent_id(&after.parent)? == parent_id(&before.parent)?,
        "transfer lost parent link"
    );
    ensure!(after.labels == before.labels, "transfer lost labels");
    ensure!(
        after.assignees == before.assignees,
        "transfer changed assignees"
    );
    ensure!(
        after.milestone == before.milestone,
        "transfer lost milestone"
    );
    ensure!(
        after.body == before.body && after.title == before.title,
        "transfer changed issue content"
    );
    Ok(format!(
        "https://github.com/{}/issues/{}",
        after.source, after.number
    ))
}

/// Run within the existing request/time envelope. `save` must durably persist
/// the journal before returning; failure prevents any subsequent forge write.
pub fn run<C: Rest + Transport>(
    client: &C,
    config: &Config,
    apply: bool,
    max: usize,
    journal: &mut Journal,
    mut save: impl FnMut(&Journal) -> Result<()>,
    mut emit: impl FnMut(String) -> Result<()>,
) -> Result<()> {
    ensure!(max > 0, "--max must be positive");
    preview_reads(client, |client| {
        let resolved = config.resolve(client, &mut Vec::new())?;
        let config = resolved.as_ref().unwrap_or(config);
        ensure!(
            config.forgejo_board().is_none(),
            "migration supports GitHub only"
        );
        config
            .github_board()
            .context("migration needs a GitHub board")?;
        let forks = discover_forks(client, config)?;
        let (project_id, issues) = issues(client, config)?;
        let caps = Caps::new(Limits {
            writes: Some(max),
            ..Limits::default()
        });
        if apply {
            let viewer = get(client, "user")?;
            ensure!(
                required(&viewer, "login")?.eq_ignore_ascii_case(&config.bot),
                "apply requires configured bot identity"
            );
        }
        // Reconcile every pending identity before discovering anything new.
        // No transfer mutation is retried automatically after an uncertain result.
        for receipt in journal.receipts.values_mut() {
            if receipt.canonical.is_some() {
                continue;
            }
            let after = before(
                &snapshot(
                    client,
                    &receipt.before.id,
                    required(&receipt.before.board, "item")?,
                )?,
                config,
            )?;
            if after.source.eq_ignore_ascii_case(&receipt.destination) {
                let canonical = preserved(&receipt.before, &after, &receipt.destination)?;
                emit(format!(
                    "reconciled {}#{} -> {canonical}",
                    receipt.before.source, receipt.before.number
                ))?;
                if apply {
                    receipt.canonical = Some(canonical);
                }
            } else {
                bail!(
                    "unresolved transfer {}#{}: inspect the durable receipt before retrying; no mutation sent",
                    receipt.before.source,
                    receipt.before.number
                );
            }
        }
        if apply {
            save(journal)?;
        }
        for (number, (id, item_id)) in issues {
            let source = format!("{}#{number}", config.dispatch_repo);
            if journal.receipts.contains_key(&id) {
                continue;
            }
            let mut route_hint = format!("{source} -> unresolved upstream=unresolved");
            let candidate = (|| -> Result<(Before, Fork, Option<u64>)> {
                let node = snapshot(client, &id, &item_id)?;
                idle_references(&node)?;
                let evidence = before(&node, config)?;
                ensure!(
                    evidence.board["project"] == project_id,
                    "project identity changed"
                );
                ensure!(
                    evidence.source.eq_ignore_ascii_case(&config.dispatch_repo)
                        && evidence.number == number,
                    "stale source identity"
                );
                let item = ItemRef {
                    host: DEFAULT_HOST.into(),
                    repository: config.dispatch_repo.clone(),
                    number,
                };
                let comments = plan::issue_comments(client, &item)?;
                let fork = forks[classify(&node, &comments, config, &forks)?].clone();
                route_hint = format!("{source} -> {} upstream=unresolved", fork.repository);
                unplanned(&node, &comments, config)?;
                if fork.parent.is_empty() {
                    bail!(
                        "{}",
                        fork.refusal.as_deref().unwrap_or("incomplete fork parent")
                    );
                }
                let upstream = association(&evidence.body, &fork)?;
                route_hint = format!(
                    "{source} -> {} upstream={}",
                    fork.repository,
                    upstream.map_or_else(|| "none".into(), |n| format!("{}#{n}", fork.parent))
                );
                if let Some(refusal) = &fork.refusal {
                    bail!("{refusal}");
                }
                if let Some(number) = upstream {
                    crate::upstream::issue(client, &fork.parent, number)?;
                    ensure!(
                        crate::upstream::parse(&evidence.body)?.is_some(),
                        "add Upstream: #{number} before migration; automatic body edits cannot protect concurrent changes"
                    );
                }
                compatible_metadata(client, &evidence, &fork)?;
                idle(client, config, &fork)?;
                Ok((evidence, fork, upstream))
            })();
            let (evidence, fork, upstream) = match candidate {
                Ok(c) => c,
                Err(error) => {
                    emit(format!("hold {route_hint}: {error:#}"))?;
                    continue;
                }
            };
            let route = format!(
                "{source} -> {} upstream={}",
                fork.repository,
                upstream.map_or_else(|| "none".into(), |n| format!("{}#{n}", fork.parent))
            );
            if caps.check(Kind::Write).is_some() {
                emit(format!("hold {route}: exhausted --max {max}"))?;
                continue;
            }
            if !apply {
                caps.count(Kind::Write);
                emit(format!("preview {route}"))?;
                continue;
            }
            // Reread content/board/parent and accepted plan immediately before
            // recording intent. New comments can replace routing evidence.
            let node = snapshot(client, &id, &item_id)?;
            idle_references(&node)?;
            ensure!(
                before(&node, config)? == evidence,
                "stale source evidence: {source}"
            );
            let comments = plan::issue_comments(
                client,
                &ItemRef {
                    host: DEFAULT_HOST.into(),
                    repository: config.dispatch_repo.clone(),
                    number,
                },
            )?;
            ensure!(
                forks[classify(&node, &comments, config, &forks)?].repository == fork.repository,
                "source plan changed: {source}"
            );
            // Revalidate admission and issues-enabled state, not just source text.
            let current = discover_forks(client, config)?;
            ensure!(
                current.iter().any(|f| f.repository == fork.repository
                    && f.parent == fork.parent
                    && f.id == fork.id
                    && f.refusal.is_none()),
                "destination admission changed"
            );
            unplanned(&node, &comments, config)?;
            idle(client, config, &fork)?;
            compatible_metadata(client, &evidence, &fork)?;
            ensure!(
                client.bounds.left.get() >= 8,
                "insufficient requests to transfer and verify"
            );
            // Never PATCH an issue body: GitHub offers no documented conditional
            // issue edit that would protect a human edit during preflight.
            let expected = evidence.clone();
            journal.receipts.insert(
                id.clone(),
                Receipt {
                    before: expected.clone(),
                    destination: fork.repository.clone(),
                    upstream,
                    canonical: None,
                },
            );
            save(journal)?;
            caps.count(Kind::Write);
            // Whether the mutation returned success, an API error or a timeout,
            // read the stable node. Never resend based on the old issue number.
            let result = client.graphql(TRANSFER, json!({"issue":id,"repository":fork.id}));
            let after = before(
                &snapshot(client, &id, &item_id)
                    .context("transfer outcome uncertain; retain journal")?,
                config,
            )?;
            let canonical = preserved(&expected, &after, &fork.repository).with_context(|| {
                format!("transfer not verified ({result:?}); retain journal and stop")
            })?;
            journal
                .receipts
                .get_mut(&id)
                .context("missing transfer receipt")?
                .canonical = Some(canonical.clone());
            save(journal)?;
            emit(format!("transferred {route} canonical={canonical}"))?;
        }
        emit(caps.summary())
    })
}

#[cfg(test)]
mod tests;
