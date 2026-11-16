import re
import unittest

from repository_links import ROOT, rewrite, staged

SOURCE = "https://forge.example/o/r/blob/abc/"


class RepositoryLinksTest(unittest.TestCase):
    def test_readme_is_a_minimal_entry_point(self):
        readme = (ROOT / "README.md").read_text(encoding="utf-8")
        self.assertRegex(readme, r"^# agent-board\n\n> This text is LLM generated\.\n")
        self.assertIn("https://cgwalters-forge.github.io/agent-board/", readme)
        self.assertRegex(readme, r"\[.+?\]\(site/index\.md\)")
        self.assertIn("Apache-2.0 or MIT, at your option.", readme)
        self.assertIsNone(re.search(r"^##? (?!agent-board$)|^```|^ {4}\S", readme, re.MULTILINE))

    def test_rewrite(self):
        for url, page, want in [
            # Between docs pages, with and without a fragment.
            ("job-plans.md", "docs/epic-plans.md", "job-plans.md"),
            ("job-plans.md#batching", "docs/design.md", "job-plans.md#batching"),
            # The landing page is a page too; the README is not, and goes to the forge.
            ("../README.md", "docs/job-plans.md", f"{SOURCE}README.md"),
            ("../docs/job-plans.md", "index.md", "docs/job-plans.md"),
            ("../docs/design.md#current-implementation-limits", "index.md", "docs/design.md#current-implementation-limits"),
            ("../site/index.md", "docs/setup.md", "../index.md"),
            ("index.md", "index.md", "index.md"),
            # Other repository files, Markdown or not, go to the forge.
            ("epic-plans.schema.json", "docs/epic-plans.md", f"{SOURCE}docs/epic-plans.schema.json"),
            ("../policy/apps.json", "docs/apps.md", f"{SOURCE}policy/apps.json"),
            ("../.github/workflows/ci.yml", "docs/setup.md", f"{SOURCE}.github/workflows/ci.yml"),
            ("../crates/board-core/src/lib.rs#L10", "docs/design.md", f"{SOURCE}crates/board-core/src/lib.rs#L10"),
            ("../fixtures/", "docs/design.md", f"{SOURCE}fixtures/"),
            ("..", "docs/design.md", SOURCE),
            ("../a%20b.md", "docs/design.md", f"{SOURCE}a%20b.md"),
            # Kept: external, absolute, fragment-only, and out of the repository.
            ("https://example.com/a.md", "docs/design.md", "https://example.com/a.md"),
            ("mailto:a@example.com", "index.md", "mailto:a@example.com"),
            ("/docs/x.md", "index.md", "/docs/x.md"),
            ("#commands", "index.md", "#commands"),
            ("../../elsewhere.md", "docs/design.md", "../../elsewhere.md"),
        ]:
            with self.subTest(page=page, url=url):
                self.assertEqual(rewrite(url, page, SOURCE), want)

    def test_staged(self):
        for file, want in [
            ("site/index.md", "index.md"),
            ("docs/design.md", "docs/design.md"),
            # Only the pages site-build stages.
            ("README.md", None),
            ("AGENTS.md", None),
            ("docs/sub/x.md", None),
            ("docs/epic-plans.schema.json", None),
            ("specs/board.tla", None),
        ]:
            with self.subTest(file=file):
                self.assertEqual(staged(file), want)


if __name__ == "__main__":
    unittest.main()
