# The forge's shared workflow check (cgwalters-forge/actions, lint/README.md):
# actionlint, zizmor with .github/zizmor.yml, and a runner-label policy that
# denies ubuntu-latest. Their versions are pinned there, not here.
# FORGE_ACTIONS is a checkout of that repository; CI checks it out beside
# this one. CONTAINER=podman runs the linters without docker.
forge_actions := env("FORGE_ACTIONS", "../actions")

lint:
    {{ just_executable() }} --justfile "{{ forge_actions }}/lint/Justfile" --working-directory . check

# NODE_PATH must provide Ajv 8, the review site's JSON Schema validator.
test-action-schema:
    node tests/operator-actions.cjs
