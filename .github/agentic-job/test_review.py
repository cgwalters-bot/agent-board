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
    def test_caller_uses_a_pinned_wrapper_and_never_checks_out_the_head(self):
        caller = (ROOT / ".github/workflows/review.yml").read_text()
        self.assertRegex(
            caller,
            r"uses: cgwalters-forge/actions/\.github/workflows/cgwalters-review\.yml@[0-9a-f]{40} # ",
        )
        self.assertIn("@d8e22f71b2abbe5f41306a0c7ef1b6c3d2829cac # agentic-job v0.6.2", caller)
        self.assertIn("types: [opened, synchronize, reopened, ready_for_review]", caller)
        self.assertIn("issue_comment:\n    types: [created]", caller)
        self.assertIn("github.event.issue.pull_request && startsWith", caller)
        self.assertIn("permissions: {}", caller)
        for forbidden in ("checkout@", "steps:", "secrets:", "contents: write", "merge_group:"):
            with self.subTest(forbidden=forbidden):
                self.assertNotIn(forbidden, caller)

    def test_third_party_uses_are_hash_pinned(self):
        # Covers step-level and job-level (reusable workflow) uses:, which
        # zizmor's unpinned-uses does not. agentic-job is referenced by release
        # tag, except reconcile.yml's call, which receives SAFE_OUTPUTS_PAT and
        # is pinned by commit; other cgwalters-forge references follow main
        # (cgwalters-forge/tracker#544), except the review wrapper, which holds
        # review.yml's write grants, and secure-host, which runs as root before
        # the job is hardened and which `agentic-job workflows check` admits
        # only at a commit (agentic-job docs/workflows-check.md): both are
        # pinned by commit.
        workflows = sorted((ROOT / ".github/workflows").glob("*.yml")) + sorted((ROOT / "live").glob("*.yml"))
        self.assertTrue(workflows)
        for path in workflows:
            for ref in re.findall(r"^\s*(?:-\s+)?uses:\s*['\"]?([^\s'\"#]+)", path.read_text(), re.MULTILINE):
                with self.subTest(path=path.name, ref=ref):
                    if ref.startswith("./"):
                        continue
                    target, _, pin = ref.partition("@")
                    if path.name == "reconcile.yml" and target == "cgwalters-forge/agentic-job/.github/workflows/agentic-job.yml":
                        self.assertRegex(pin, r"\A[0-9a-f]{40}\Z")
                    elif re.match(r"cgwalters-forge/agentic-job(/|\Z)", target):
                        self.assertRegex(pin, r"\Av\d+\.\d+\.\d+\Z")
                    elif target in (
                        "cgwalters-forge/actions/.github/workflows/cgwalters-review.yml",
                        "cgwalters-forge/actions/secure-host",
                    ):
                        self.assertRegex(pin, r"\A[0-9a-f]{40}\Z")
                    elif target.startswith("cgwalters-forge/"):
                        self.assertEqual(pin, "main")
                    else:
                        self.assertRegex(pin, r"\A[0-9a-f]{40}\Z")

    def test_dependabot_proposes_agentic_job_releases_daily(self):
        config = (ROOT / ".github/dependabot.yml").read_text()
        self.assertIn("package-ecosystem: github-actions\n    directory: /\n    schedule:\n      interval: daily\n", config)
        self.assertIn('groups:\n      agentic-job:\n        patterns:\n          - "cgwalters-forge/agentic-job"\n          - "cgwalters-forge/agentic-job/*"\n', config)

    def test_bounds_are_comment_only_and_commands_need_write(self):
        for name, events, roles, bots in (
            ("review", ["pull_request_target", "pull_request"], ["all"], ["dependabot[bot]"]),
            ("review-command", ["issue_comment"], ["admin", "maintain", "write"], []),
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
                self.assertEqual(bounds["trigger"]["bots"], bots)
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
        jobs = ("specs", "rust", "lint")
        self.assertIn(f"needs: [{', '.join(jobs)}]", sentinel)
        expression = re.search(r"run: jq -e '([^']+)'", sentinel).group(1)
        for results in itertools.product(("success", "failure", "cancelled", "skipped"), repeat=len(jobs)):
            with self.subTest(results=results):
                needs = {job: {"result": result} for job, result in zip(jobs, results)}
                checked = subprocess.run(
                    ["jq", "-e", expression], input=json.dumps(needs), text=True, capture_output=True,
                )
                self.assertEqual(checked.returncode, 0 if set(results) == {"success"} else 1)

    def test_ci_runs_the_shared_workflow_check(self):
        # Through the Justfile, beside a checkout of the shared actions at
        # main, on the standing runner label; the check itself denies
        # ubuntu-latest.
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        lint = ci.split("\n  lint:\n", 1)[1].split("\n\n", 1)[0]
        self.assertIn("runs-on: ubuntu-26.04", lint)
        self.assertIn("repository: cgwalters-forge/actions\n          ref: main\n          path: actions\n", lint)
        self.assertEqual(lint.count("persist-credentials: false"), 2)
        self.assertIn("- run: just lint\n        working-directory: agent-board", lint)
        # It runs code from another repository's main: no credential beyond
        # the workflow's contents: read.
        for forbidden in ("permissions:", "secrets.", "GH_TOKEN", "token:"):
            with self.subTest(forbidden=forbidden):
                self.assertNotIn(forbidden, lint)
        justfile = (ROOT / "Justfile").read_text()
        self.assertIn('forge_actions := env("FORGE_ACTIONS", "../actions")', justfile)
        self.assertIn('--justfile "{{ forge_actions }}/lint/Justfile" --working-directory . check', justfile)

    def test_site_is_built_hardened_and_deployed_by_a_deploy_only_job(self):
        # The shape `agentic-job workflows check` admits for GitHub Pages
        # (agentic-job docs/workflows-check.md, "Deploy-only Pages jobs"),
        # which this repository does not yet run: the job holding the Pages
        # grant and the OIDC request token runs nothing of the repository,
        # only one pinned upstream commit of deploy-pages, and takes the
        # artifact the hardened check job uploaded on a push to main. The
        # pin is v5.0.1; agentic-job's check must admit that commit (its
        # DEPLOY_PAGES_COMMITS) before this repository can adopt the check.
        site = (ROOT / ".github/workflows/site.yml").read_text()
        self.assertIn("\npermissions:\n  contents: read\n\nconcurrency:\n", site)
        check, deploy = site.split("\n  check:\n", 1)[1].split("\n  deploy:\n", 1)
        # Every checkout drops its token, and nothing runs between the
        # checkout and secure-host: the tree is anyone's until hardened.
        self.assertEqual(check.count("checkout@"), check.count("persist-credentials: false"))
        self.assertGreater(check.count("checkout@"), 0)
        before, hardened = check.split("uses: cgwalters-forge/actions/secure-host@", 1)
        self.assertNotIn("- run:", before)
        self.assertNotIn("- uses:", before.split("persist-credentials: false", 1)[1])
        self.assertIn("- run: just site-test\n", hardened)
        self.assertIn("- run: just site-build\n", hardened)
        main_only = "if: github.event_name == 'push' && github.ref == 'refs/heads/main'\n        "
        self.assertIn(main_only + "run: just site-build-pages\n", hardened)
        self.assertIn(main_only + "uses: actions/upload-pages-artifact@", hardened)
        for forbidden in ("id-token", "secrets.", "env:", "GH_TOKEN", "token:"):
            with self.subTest(job="check", forbidden=forbidden):
                self.assertNotIn(forbidden, check)
        self.assertIn("if: github.event_name == 'push' && github.ref == 'refs/heads/main'\n    needs: check\n", deploy)
        self.assertIn("permissions:\n      pages: write\n      id-token: write\n    environment: github-pages\n", deploy)
        self.assertEqual(
            deploy.split("steps:\n", 1)[1].strip(),
            "- uses: actions/deploy-pages@368f82528645a54fb793d4d04e342629a3f51346 # v5.0.1",
        )
        for forbidden in ("checkout", "run:", "env:", "with:", "secrets.", "container:", "services:"):
            with self.subTest(job="deploy", forbidden=forbidden):
                self.assertNotIn(forbidden, deploy)
        justfile = (ROOT / "Justfile").read_text()
        self.assertIn("cd site && npm ci --ignore-scripts --no-audit --no-fund\n", justfile)
        self.assertIn("node --test site/links.test.mjs\n", justfile)


if __name__ == "__main__":
    unittest.main()
