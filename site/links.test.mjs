import assert from "node:assert/strict";
import { test } from "node:test";
import { firstHeading, headingAnchor, pageId, remarkRepositoryLinks, rewrite } from "./links.mjs";

const opts = { base: "/agent-board/", source: "https://forge.example/o/r/blob/abc/" };

test("rewrite", () => {
  for (const [url, from, want] of [
    // Between docs pages, with and without a fragment.
    ["job-plans.md", "docs/epic-plans.md", "/agent-board/docs/job-plans/"],
    ["job-plans.md#batching", "docs/design.md", "/agent-board/docs/job-plans/#batching"],
    // The README and the landing page are pages too.
    ["../README.md#commands", "docs/job-plans.md", "/agent-board/overview/#commands"],
    ["docs/design.md#current-implementation-limits", "README.md", "/agent-board/docs/design/#current-implementation-limits"],
    ["../docs/job-plans.md", "site/index.md", "/agent-board/docs/job-plans/"],
    ["index.md", "site/index.md", "/agent-board/"],
    // Other repository files, Markdown or not, go to the forge.
    ["epic-plans.schema.json", "docs/epic-plans.md", "https://forge.example/o/r/blob/abc/docs/epic-plans.schema.json"],
    ["../policy/apps.json", "docs/apps.md", "https://forge.example/o/r/blob/abc/policy/apps.json"],
    ["live/board-test.yml", "README.md", "https://forge.example/o/r/blob/abc/live/board-test.yml"],
    [".github/workflows/reconcile.yml", "README.md", "https://forge.example/o/r/blob/abc/.github/workflows/reconcile.yml"],
    ["../crates/board-core/src/lib.rs#L10", "docs/design.md", "https://forge.example/o/r/blob/abc/crates/board-core/src/lib.rs#L10"],
    ["../fixtures/", "docs/design.md", "https://forge.example/o/r/blob/abc/fixtures/"],
    ["..", "docs/design.md", "https://forge.example/o/r/blob/abc/"],
    // Kept: external, absolute, fragment-only, and out of the repository.
    ["https://example.com/a.md", "docs/design.md", "https://example.com/a.md"],
    ["mailto:a@example.com", "README.md", "mailto:a@example.com"],
    ["/docs/x.md", "README.md", "/docs/x.md"],
    ["#commands", "README.md", "#commands"],
    ["../../elsewhere.md", "docs/design.md", "../../elsewhere.md"],
  ]) {
    assert.equal(rewrite(url, from, opts), want, `${from}: ${url}`);
  }
});

test("pageId", () => {
  assert.equal(pageId("README.md"), "overview");
  assert.equal(pageId("site/index.md"), "");
  assert.equal(pageId("docs/design.md"), "docs/design");
  // Only the pages the collection loads.
  assert.equal(pageId("docs/sub/x.md"), null);
  assert.equal(pageId("docs/epic-plans.schema.json"), null);
  assert.equal(pageId("specs/board.tla"), null);
  assert.equal(pageId("AGENTS.md"), null);
});

test("headingAnchor", () => {
  assert.equal(headingAnchor("Where it fits"), "where-it-fits");
  assert.equal(headingAnchor("3. Agentic-job bounds and delegated operations"), "3-agentic-job-bounds-and-delegated-operations");
  assert.equal(headingAnchor("Operator decisions (cgwalters, 2026-10-07)"), "operator-decisions-cgwalters-2026-10-07");
  assert.equal(headingAnchor("CLI: gh agent-board"), "cli-gh-agent-board");
});

test("remarkRepositoryLinks", () => {
  const plugin = remarkRepositoryLinks({ root: "/repo", ...opts });
  const page = () => ({
    children: [
      { type: "heading", depth: 1, children: [{ type: "text", value: "The " }, { type: "inlineCode", value: "poll" }, { type: "text", value: " command" }] },
      { type: "paragraph", children: [{ type: "link", url: "job-plans.md#dispatch", children: [] }] },
      { type: "definition", url: "../README.md" },
    ],
  });
  const tree = page();
  plugin(tree, { path: "/repo/docs/design.md" });
  assert.deepEqual(tree.children[0], { type: "html", value: '<span id="the-poll-command"></span>' });
  assert.equal(tree.children[1].children[0].url, "/agent-board/docs/job-plans/#dispatch");
  assert.equal(tree.children[2].url, "/agent-board/overview/");
  // A file the site does not render as a page is left alone.
  const other = page();
  plugin(other, { path: "/repo/specs/board.md" });
  assert.deepEqual(other, page());
});

test("firstHeading", () => {
  assert.equal(firstHeading("# The `poll` command\n\ntext"), "The poll command");
  assert.equal(firstHeading("\n# Title\n"), "Title");
  assert.equal(firstHeading("text\n# Later\n"), null);
  assert.equal(firstHeading("## Second level\n"), null);
});
