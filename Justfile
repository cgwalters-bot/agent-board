# The forge's shared workflow check (cgwalters-forge/actions, lint/README.md):
# actionlint, zizmor with .github/zizmor.yml, and a runner-label policy that
# denies ubuntu-latest. Their versions are pinned there, not here.
# FORGE_ACTIONS is a checkout of that repository; CI checks it out beside
# this one. CONTAINER=podman runs the linters without docker.
forge_actions := env("FORGE_ACTIONS", "../actions")

# As agentic-job's release-check: refuse malformed, mismatched or old versions.
# pipefail prevents a failed tag read from looking like a first release.
release-check version:
    bash -euo pipefail -c 'git tag --list "v*" | cargo run --quiet --locked -p agent-board --bin release-check -- "$1"' -- {{quote(version)}}

# Shared by PR CI and manual release; musl-tools and the Rust target are prerequisites.
build-static:
    cargo build --release --locked --target x86_64-unknown-linux-musl -p agent-board --bin agent-board --bin gh-agent-board

# Needs just, cargo and docker (or CONTAINER=podman); pulls linter images.
# Checks .github/, not live/. Remove site/{.venv,dist,pages} first: zizmor
# otherwise reads dependencies' workflows.
# Run the forge's shared workflow check.
lint:
    {{ just_executable() }} --justfile "{{ forge_actions }}/lint/Justfile" --working-directory . check

# Fresh Linux devspace, downloads included (2026-10-08): format 0.07 s.
fmt:
    cargo fmt --all

# CI's rust job (20 min allowance): format, clippy, tests; fixtures/fake transports,
# no root or network service/credentials needed, though cargo may fetch dependencies.
# Observed 2026-10-08: clippy 15.50 s, tests 20.97 s; not deadlines.
# Check formatting, lint Rust and run offline tests.
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo test --workspace --locked

# Remaining rust-job smoke commands, in CI order; observed 7.91/0.07/0.07 s.
smoke:
    cargo run --locked -q -p agent-board -- item list --snapshot fixtures/board.json
    cargo run --locked -q -p agent-board -- reconcile plan --snapshot fixtures/board.json --emit
    cargo run --locked -q -p agent-board -- project view --snapshot fixtures/board.json --json project,coverage

# Safe local build from live/board-test.yml; cold observation: 54.30 s.
# Do not run that workflow's test project command in a sandbox: it mutates the forge.
# Build the release binary locally.
build-release:
    cargo build --locked --release -p agent-board

# Separate forgejo CI job: fresh service container, docker or CONTAINER=podman,
# and AGENT_BOARD_TEST_FORGEJO{,_CONTAINER}; see docs/forges.md#live-ci.
# Test against a disposable Forgejo service.
test-forgejo:
    cargo test --locked -p agent-board --test forgejo_live -- --ignored --nocapture

# Setup's credential-free recorded pass; preview only, no live writes/dispatch.
replay:
    cargo run --locked -q -p agent-board -- pass --config fixtures/pass/config.json --replay fixtures/pass/board-2026-10-23.json --max-dispatch 1 --max-writes 3 --max-status-changes 5

# Prepare BASE with `git worktree add ../base <start commit>`; see docs/setup.md.
decision-diff base:
    node workflow/decision-diff.cjs {{quote(base)}} .

# docs/cli.md, the CLI reference, from the binary's own help; a test
# fails when the committed copy is stale.
cli-docs:
    cargo run --locked -q -p agent-board -- help-markdown > docs/cli.md

# NODE_PATH must provide Ajv 8, the control app's JSON Schema validator.
test-action-schema:
    node tests/operator-actions.cjs

# The documentation site in site/ (site/zensical.toml), rendering
# site/index.md and docs/*.md, staged under site/pages/ since Zensical reads
# only its docs directory. Zensical and its dependencies come from
# site/requirements.txt alone, by hash and as wheels, so no package's build
# code runs. SITE_URL (the served address, for the 404 page and canonical
# links; the config's copy in site/build.toml gets it) and SITE_SOURCE_URL
# (site/repository_links.py) place it.
# Python 3.11+; observed build 2 s after the first install, not a deadline.
site-build:
    rm -rf site/pages && mkdir -p site/pages/docs
    cp site/index.md site/public/favicon.svg site/pages/ && cp docs/*.md site/pages/docs/
    python3 -m venv site/.venv
    site/.venv/bin/pip install --quiet --disable-pip-version-check --require-hashes --only-binary :all: -r site/requirements.txt
    sed "/^\[project\]$/a site_url = \"${SITE_URL:-}\"" site/zensical.toml > site/build.toml
    cd site && PYTHONPATH=. .venv/bin/zensical build --clean -f build.toml

# site-build for GitHub Pages, from a GitHub Actions run's own variables:
# served at the repository's default project-site address,
# https://OWNER.github.io/REPO/ (a custom domain or a REPO named
# OWNER.github.io would need other values), and linking repository files
# at the built commit. Nothing here names an owner or repository.
site-build-pages:
    SITE_URL="https://$GITHUB_REPOSITORY_OWNER.github.io/${GITHUB_REPOSITORY#*/}/" SITE_SOURCE_URL="$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$GITHUB_SHA/" {{ just_executable() }} site-build

# The link rewriting's tests import Zensical, so they need site-build's
# environment.
site-test: site-build
    cd site && .venv/bin/python -m unittest

# Site CI uses the lychee action, then on main rebuilds with site-build-pages
# and uploads Pages. Serve at / as site-build does by default; repository
# links name the checkout's files.
# Check built-site links and fragments offline.
site-check:
    lychee --offline --include-fragments --index-files index.html,. --no-progress --root-dir "$PWD/site/dist" 'site/dist/**/*.html'
