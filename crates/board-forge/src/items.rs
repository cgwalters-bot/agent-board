//! `item search` and `item list --off-board` (agent-board#352): the open
//! issues of the pass configuration's `workRepositories` beside the board's
//! open issues, read as prune reads them ([`crate::prune`]): each REST page
//! conditional through a cache, so an unchanged one answers 304 and costs no
//! rate budget, and the board by GraphQL, which cannot be revalidated. Every
//! read is spent from one budget, and running out refuses the whole list.
use crate::github::Transport;
use crate::pass::Config;
use crate::poll::Rest;
use crate::prune::{Cache, Reader, key, pick};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::collections::BTreeSet;

/// Not prune's: the pages are the same, but the facts kept of them differ.
pub const ITEM_CACHE_SCHEMA: &str = "agent-board/item-cache/v1";
/// Every read of one list: the board's pages (at most 50) and a page per
/// hundred open issues of each work repository.
pub const ITEM_REQUEST_LIMIT: usize = 200;

/// Which open issues of the work repositories to list.
pub enum Select<'a> {
    /// Those on the board whose title or body holds the text, ignoring case.
    Search(&'a str),
    /// Those not on the board.
    OffBoard,
}

#[derive(Debug, PartialEq)]
pub struct Issue {
    pub repository: String,
    pub number: u64,
    pub title: String,
}

/// What an open-issue list keeps of an issue.
fn slim_issue(issue: &Value) -> Value {
    let mut slim = pick(issue, &["number", "title", "body"]);
    slim["pull"] = issue.get("pull_request").is_some().into();
    slim
}

/// The open issues `select` picks, by repository and number, read within
/// `limit` requests through `cache`; with the responses used, the next
/// read's cache.
pub fn read<C: Rest + Transport>(
    client: &C,
    config: &Config,
    cache: Cache,
    select: &Select,
    limit: usize,
) -> Result<(Vec<Issue>, Cache)> {
    ensure!(
        !config.work_repositories.is_empty(),
        "item reads list the configuration's workRepositories, and it names none"
    );
    ensure!(
        config.board.is_some(),
        "item reads need the configuration's board"
    );
    let text = match select {
        Select::Search(text) => {
            ensure!(!text.trim().is_empty(), "the search text is empty");
            Some(text.to_lowercase())
        }
        Select::OffBoard => None,
    };
    let mut reader = Reader::new(client, cache, limit);
    let board = reader.priorities(config)?;
    let repositories: BTreeSet<&String> = config.work_repositories.iter().collect();
    let mut found = Vec::new();
    for repository in repositories {
        // Prune's pages, so a list of the same repository is read alike.
        let path =
            format!("repos/{repository}/issues?state=open&sort=created&direction=asc&per_page=100");
        for issue in reader.list(&path, slim_issue)? {
            if issue["pull"] == true {
                continue;
            }
            let number = issue["number"]
                .as_u64()
                .with_context(|| format!("an open issue of {repository} has no number"))?;
            let Some(title) = issue["title"].as_str() else {
                bail!("{repository}#{number} has no title");
            };
            let on_board = board.contains_key(&key(repository, number));
            let picked = match &text {
                Some(text) => {
                    let body = issue["body"].as_str().unwrap_or_default();
                    on_board && format!("{title}\n{body}").to_lowercase().contains(text)
                }
                None => !on_board,
            };
            if picked {
                found.push(Issue {
                    repository: repository.clone(),
                    number,
                    title: title.into(),
                });
            }
        }
    }
    found.sort_by(|a, b| (&a.repository, a.number).cmp(&(&b.repository, b.number)));
    Ok((found, reader.used))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prune::tests::{Fake, config};

    fn listed(issues: &[Issue]) -> Vec<String> {
        (issues.iter())
            .map(|i| format!("{}#{}", i.repository, i.number))
            .collect()
    }

    fn run(fake: &Fake, config: &Config, select: &Select) -> Result<(Vec<Issue>, Cache)> {
        read(fake, config, Cache::default(), select, ITEM_REQUEST_LIMIT)
    }

    #[test]
    fn search_and_off_board_table() {
        // The board holds tracker#2, #3, app#20 and #21 open; tracker#9 is a
        // pull request, and tracker#1 and #4 match "widget" but are off it.
        for (select, expected) in [
            (
                Select::Search("WIDGET"),
                vec!["example/tracker#2", "example/tracker#3"],
            ),
            (
                Select::Search("crash"),
                vec!["example/app#20", "example/app#21"],
            ),
            (Select::Search("sub-issues hold"), vec![]),
            (
                Select::OffBoard,
                vec![
                    "example/app#22",
                    "example/app#23",
                    "example/tracker#1",
                    "example/tracker#4",
                    "example/tracker#5",
                    "example/tracker#6",
                    "example/tracker#7",
                    "example/tracker#8",
                    "example/tracker#10",
                    "example/tracker#11",
                    "example/tracker#12",
                    "example/tracker#13",
                ],
            ),
        ] {
            let fake = Fake::new();
            let (found, _) = run(&fake, &config(), &select).unwrap();
            assert_eq!(listed(&found), expected);
            assert!(fake.sends.borrow().is_empty());
        }
    }

    #[test]
    fn a_second_read_revalidates_every_page_from_the_cache() {
        let fake = Fake::new();
        let search = Select::Search("spins the widget");
        let (first, cache) = run(&fake, &config(), &search).unwrap();
        assert_eq!(listed(&first), ["example/tracker#2"]);
        assert!(fake.gets.borrow().iter().all(|(_, v)| v.is_none()));
        let json = cache.to_json_as(ITEM_CACHE_SCHEMA);
        let cache = Cache::from_json_as(&json, ITEM_CACHE_SCHEMA).unwrap();
        fake.gets.borrow_mut().clear();
        // A 304 answers from the kept body, which holds the issue's body.
        let again = read(&fake, &config(), cache, &search, ITEM_REQUEST_LIMIT);
        let (second, again) = again.unwrap();
        assert_eq!(second, first);
        let gets = fake.gets.borrow();
        assert_eq!(gets.len(), 3);
        assert!(gets.iter().all(|(_, v)| v.is_some()), "{gets:?}");
        assert_eq!(again.0.len(), 3);
    }

    #[test]
    fn reads_are_bounded_and_checked() {
        // The board, then three list pages.
        for limit in [0, 1, 3] {
            let error = read(
                &Fake::new(),
                &config(),
                Cache::default(),
                &Select::OffBoard,
                limit,
            )
            .unwrap_err();
            assert_eq!(
                format!("{error:#}"),
                format!("request budget ({limit}) exhausted; nothing was listed")
            );
        }
        let mut boardless = config();
        boardless.board = None;
        let mut unscoped = config();
        unscoped.work_repositories.clear();
        for (config, select, error) in [
            (config(), Select::Search(" "), "the search text is empty"),
            (
                boardless,
                Select::OffBoard,
                "need the configuration's board",
            ),
            (unscoped, Select::OffBoard, "names none"),
        ] {
            let fake = Fake::new();
            let error_found = run(&fake, &config, &select).unwrap_err().to_string();
            assert!(error_found.contains(error), "{error_found}");
            assert!(fake.gets.borrow().is_empty());
        }
    }
}
