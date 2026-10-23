# The forge's shared workflow check (cgwalters-forge/actions, lint/README.md):
# actionlint, zizmor with .github/zizmor.yml, and a runner-label policy that
# denies ubuntu-latest. Their versions are pinned there, not here.
# FORGE_ACTIONS is a checkout of that repository; CI checks it out beside
# this one. CONTAINER=podman runs the linters without docker.
forge_actions := env("FORGE_ACTIONS", "../actions")

lint:
    {{ just_executable() }} --justfile "{{ forge_actions }}/lint/Justfile" --working-directory . check

# docs/cli.md, the CLI reference, from the binary's own help; a test
# fails when the committed copy is stale.
cli-docs:
    cargo run --locked -q -p agent-board -- help-markdown > docs/cli.md

# NODE_PATH must provide Ajv 8, the review site's JSON Schema validator.
test-action-schema:
    node tests/operator-actions.cjs

# The documentation site in site/, a rendering of docs/ and the README, as
# agentic-job's (its Justfile has the same recipes). Dependencies come from
# the lock alone, without their install scripts. SITE_BASE, SITE_URL and
# SITE_SOURCE_URL (site/astro.config.mjs) place it.
site-build:
    cd site && npm ci --ignore-scripts --no-audit --no-fund
    cd site && ASTRO_TELEMETRY_DISABLED=1 npm run build

# site-build for GitHub Pages, from a GitHub Actions run's own variables:
# served at the repository's default project-site address,
# https://OWNER.github.io/REPO/ (a custom domain or a REPO named
# OWNER.github.io would need other values), and linking repository files
# at the built commit. Nothing here names an owner or repository.
site-build-pages:
    SITE_URL="https://$GITHUB_REPOSITORY_OWNER.github.io" SITE_BASE="/${GITHUB_REPOSITORY#*/}/" SITE_SOURCE_URL="$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/$GITHUB_SHA/" {{ just_executable() }} site-build

# dist.test.mjs checks the pages site-build renders, so it builds first.
site-test: site-build
    node --test site/links.test.mjs
    node --test site/dist.test.mjs

# Links and fragments of a build served at /, as site-build makes it by
# default; links to repository files then name the checkout's files.
site-check:
    lychee --offline --include-fragments --index-files index.html,. --no-progress --root-dir "$PWD/site/dist" 'site/dist/**/*.html'
