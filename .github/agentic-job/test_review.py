"""Credential-free contracts for the repository's thin review/queue caller."""

import itertools
import json
from pathlib import Path
import re
import subprocess
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[2]


class ReviewContracts(unittest.TestCase):
    def test_caller_follows_main_and_never_checks_out_the_head(self):
        caller = (ROOT / ".github/workflows/review.yml").read_text()
        self.assertRegex(
            caller,
            r"uses: cgwalters-forge/agentic-job/\.github/workflows/review\.yml@main\n",
        )
        self.assertIn("types: [opened, synchronize, reopened, ready_for_review]", caller)
        self.assertIn("issue_comment:\n    types: [created]", caller)
        self.assertIn("github.event.issue.pull_request && startsWith", caller)
        self.assertIn("permissions: {}", caller)
        for forbidden in ("checkout@", "steps:", "secrets:", "contents: write", "merge_group:"):
            with self.subTest(forbidden=forbidden):
                self.assertNotIn(forbidden, caller)

    def test_third_party_uses_are_hash_pinned(self):
        # Covers step-level and job-level (reusable workflow) uses:, which
        # zizmor's unpinned-uses does not; only cgwalters-forge follows main
        # (cgwalters-forge/tracker#544).
        workflows = sorted((ROOT / ".github/workflows").glob("*.yml")) + sorted((ROOT / "live").glob("*.yml"))
        self.assertTrue(workflows)
        for path in workflows:
            for ref in re.findall(r"^\s*(?:-\s+)?uses:\s*['\"]?([^\s'\"#]+)", path.read_text(), re.MULTILINE):
                with self.subTest(path=path.name, ref=ref):
                    if ref.startswith("./"):
                        continue
                    target, _, pin = ref.partition("@")
                    if target.startswith("cgwalters-forge/"):
                        self.assertEqual(pin, "main")
                    else:
                        self.assertRegex(pin, r"\A[0-9a-f]{40}\Z")

    def test_bounds_are_comment_only_and_commands_need_write(self):
        for name, events, roles in (
            ("review", ["pull_request_target", "pull_request"], ["all"]),
            ("review-command", ["issue_comment"], ["admin", "maintain", "write"]),
        ):
            with self.subTest(name=name):
                bounds = tomllib.loads((ROOT / f"workflow/{name}.toml").read_text())
                self.assertEqual(bounds["hosts"], ["github.com"])
                self.assertEqual(bounds["repos"], ["cgwalters-forge/agent-board"])
                self.assertEqual(bounds["bases"], ["main"])
                self.assertEqual(bounds["max_outputs"], 1)
                self.assertEqual(bounds["outputs"], {"add_comment": {"max": 1}, "noop": {"max": 1}})
                self.assertEqual(bounds["trigger"]["events"], events)
                self.assertEqual(bounds["trigger"]["roles"], roles)
                self.assertEqual(bounds["trigger"]["bots"], [])
                self.assertTrue(bounds["trigger"]["forks"])
                self.assertEqual(bounds["trigger"].get("commands"), ["/review"] if name == "review-command" else None)

    def test_host_config_and_task(self):
        config = tomllib.loads((ROOT / ".github/agentic-job/hosted.toml").read_text())
        self.assertIn("cargo", config["setup"]["repo-packages"]["cgwalters-forge/agent-board"])
        self.assertNotIn("egress", config)
        self.assertNotIn("world-write-walk", config["sandbox"])
        task = (ROOT / "workflow/review.md").read_text()
        for contract in ("hostile", "Do not modify code", "exact admitted head SHA", "VERDICT: APPROVE", "VERDICT: CHANGES", "never a GitHub approval"):
            with self.subTest(contract=contract):
                self.assertIn(contract, task)

    def test_merge_queue_sentinel_fails_closed(self):
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn("merge_group:\n    types: [checks_requested]", ci)
        sentinel = ci.split("\n  ci:\n", 1)[1]
        self.assertIn("if: ${{ always() }}", sentinel)
        self.assertIn("needs: [specs, rust]", sentinel)
        expression = re.search(r"run: jq -e '([^']+)'", sentinel).group(1)
        for results in itertools.product(("success", "failure", "cancelled", "skipped"), repeat=2):
            with self.subTest(results=results):
                needs = {job: {"result": result} for job, result in zip(("specs", "rust"), results)}
                checked = subprocess.run(
                    ["jq", "-e", expression], input=json.dumps(needs), text=True, capture_output=True,
                )
                self.assertEqual(checked.returncode, 0 if results == ("success", "success") else 1)


if __name__ == "__main__":
    unittest.main()
