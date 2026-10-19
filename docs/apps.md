# Per-role GitHub Apps

Design for [tracker#766](https://github.com/cgwalters-forge/tracker/issues/766)
(revising [tracker#749](https://github.com/cgwalters-forge/tracker/issues/749)):
each automation role writes as its own GitHub App, so that authorship says which
step wrote a comment, review or push, and each role holds only the permissions
its writes need. **Nothing here is implemented yet.** Today every write in
tracker's workflows uses one organization secret, `CGWALTERS_BOT_PAT`, the
cgwalters-bot user's token, or the job's `GITHUB_TOKEN` (`github-actions[bot]`).
Sources were read at tracker, cgwalters-forge/actions and agentic-job `main` and
GitHub's documentation on 2026-10-19.

## Roles and permissions

Each role's writes are the ones its steps make today; its permissions are the
smallest [GitHub App permissions](https://docs.github.com/en/rest/authentication/permissions-required-for-github-apps)
that cover them. Every app also gets Metadata: read, which GitHub always grants.
No role's token reaches an agent: agents keep the read-only job token
(operator decision of 2026-10-09), as the wrappers already arrange.

| Role | Steps that write today | Writes | Permissions | Token |
| --- | --- | --- | --- | --- |
| status | agentic-job's policy `notify` and apply's `conclude`, and the `session` job of cgwalters-forge/actions' `cgwalters-agentic-job.yml`, given `SAFE_OUTPUTS_PAT` from tracker's `dispatch.yml`, `session.yml` and `supervisor.yml` | post and edit the run-status comment on the dispatched issue or pull request, in any followed repository | Issues: write, Pull requests: write | installation |
| review | `cgwalters-review.yml`'s `submit`, `refusal` and `status` jobs (job token today), and its `apply` comment fallback (`SAFE_OUTPUTS_PAT`, which tracker does not pass) | submit a pull request review or comment, set the `review` commit status | Pull requests: write, Commit statuses: write | installation |
| implement/fix | agentic-job's `apply` job for implement and fix runs, given `SAFE_OUTPUTS_PAT` | fork the target into the token user's account and turn Actions off there, push a branch to the fork or to an existing pull request's branch, open the pull request, post the result comment | Contents: write, Pull requests: write, Issues: write, Workflows: write, Administration: write | user access token |
| supervisor executor | tracker `supervisor.yml`'s `execute` (`agent-board supervise execute`), and `poll.yml`'s board scheduler, coordinator and poll pass (`agent-board dispatch --apply`, `poll --apply`, epic apply) | dispatch `dispatch.yml` (`execute` only; poll dispatches with the job token), comment, create, close, lock and label issues, add sub-issues, edit the supervisor state and decisions issues, rebase-merge or enqueue pull requests, set project fields and post project status updates | Repository: Actions: write, Contents: write, Workflows: write, Issues: write, Pull requests: write, Checks: read, Commit statuses: read; Organization: Projects: write | user access token |

Notes on the table:

- **Executor covers poll.** tracker#749 gives the executor the typed controller
  actions (comment, label, dispatch, enqueue); the poll, board scheduler and
  coordinator steps make the same kinds of write and match each other's
  markers by the bot login, so they share one role.
- **Implement/fix needs Administration: write** because agentic-job's apply, on
  every run, calls [Create a fork](https://docs.github.com/en/rest/repos/forks#create-a-fork)
  (which answers with the existing fork) and turns Actions off on the fork and
  reads that back (`/repos/{owner}/{repo}/actions/permissions`). It also lets the
  token delete repositories the app is installed on and the bot administers,
  i.e. the bot's forks; that is the price of keeping forks' Actions off.
  Workflows: write is needed when a patch touches `.github/workflows/`, which fix
  bounds allow for some repositories, and is assumed for the executor's merges of
  such pull requests (the staging test checks it).
- **Installations.** Each app installs on the organizations whose repositories
  its role writes: cgwalters-forge, and bootc-dev where tracker's comment and fix
  bounds admit it. Implement/fix also installs on cgwalters-bot's user account with
  access to all repositories, as forking requires
  ([Create a fork](https://docs.github.com/en/rest/repos/forks#create-a-fork)).
  A private app "can only be installed on the account that owns the app", and only
  the owning organization's members can authorize it
  ([public or private](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/making-a-github-app-public-or-private)),
  so the apps are owned by cgwalters-forge and made public where they install
  elsewhere or cgwalters-bot is not a member. Public only lets other accounts
  install the app on their own resources; it grants nothing of ours, and the
  keys stay with the operator.
  Organization Projects permission covers every project of the organization;
  agent-board's project identity checks still bound which one is written.
- **Status needs its own secret.** agentic-job hands one `SAFE_OUTPUTS_PAT` to
  apply and to the status steps alike, so moving status alone needs the wrapper
  and agentic-job to take a separate status token.
- **Not covered:** `broker-probe.yml`, `usage.yml` and `release-train.yml`, and
  the read-only supervisor health snapshot and poll epic census, keep the PAT until a later step
  assigns them; the PAT is retired only where an app covers all of a step's
  writes.

## Token types

GitHub Apps have two tokens that can write:

- **Installation access token** ([generating one](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app)):
  minted from the app's private key with `POST /app/installations/ID/access_tokens`,
  optionally narrowed to listed repositories and permissions; "The installation
  access token will expire after 1 hour." It acts as the app: "API requests made
  by an app installation are attributed to the app"
  ([authenticating as an installation](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app-installation)).
  It has no user, so `GET /user` is not available to it: that endpoint is listed for
  [user access tokens](https://docs.github.com/en/rest/authentication/endpoints-available-for-github-app-user-access-tokens)
  but not for [installation access tokens](https://docs.github.com/en/rest/authentication/endpoints-available-for-github-app-installation-access-tokens).
- **User access token** ([on behalf of a user](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-with-a-github-app-on-behalf-of-a-user)):
  the app can reach only what the user can, what the app has permission for,
  and accounts where the app is installed, i.e. the intersection; requests "will
  be attributed to that user", shown as the user's avatar with the app's badge.
  It needs one authorization by the user in a browser (web or device flow). By
  default it expires after eight hours and comes with a refresh token that
  expires after six months and is single-use: "Once you use a refresh token, that
  refresh token and the old user access token will no longer work"
  ([refreshing](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/refreshing-user-access-tokens)).
  An app can opt out of expiry, and then its user access tokens do not expire.

**Per role.** Status and review use installation tokens: they only label their
own output, need no user identity, and an app author is the provenance they are
for. Implement/fix and the executor act as cgwalters-bot today, and code depends
on it: agentic-job's apply forks into the account `GET /user` names and refuses a
token without one; `poll --apply` requires `GET /user` to be the configured bot;
`dispatch --apply` requires GraphQL `viewer` to be a plan writer; and poll,
coordinator and executor find their own markers and logs by the bot's login. So
those two use user access tokens on behalf of cgwalters-bot, which keep every one
of those checks working and add the app's badge.

For the user tokens, the single-use refresh token is the cost. A workflow that
refreshes must write the new refresh token back to a secret before the next run,
which needs a Secrets: write credential in that job, and two concurrent runs
refreshing at once lock one of them out. That fails the gut check, so implement
and executor apps **opt out of user-token expiry**: cgwalters-bot authorizes each
once (device flow), and the operator stores the token as that role's secret
where the steps that use it can read it: the `agent-apply` environment for
implement/fix, and for the executor an organization or repository secret as
`CGWALTERS_BOT_PAT` is now, since poll's jobs name no environment. That token is long-lived like the PAT it replaces,
but bounded by the app's permissions instead of the bot's, attributed to the app
as well as the bot, and revoked per app. Moving these roles to installation tokens
later needs the code above to stop requiring a user: a fork owner other than
`GET /user`, and the bot login configurable as the app's.

## Provenance agent-board can read

| Token | REST comment `user.login` | REST `performed_via_github_app.id` | GraphQL `author` |
| --- | --- | --- | --- |
| installation | `SLUG[bot]`, `type: Bot` | the app's id | `Bot`, `login: SLUG`, `databaseId` the bot account's id |
| user access token | `cgwalters-bot`, `type: User` | the app's id (expected; staging confirms) | `User`, `login: cgwalters-bot` |
| PAT (today) | `cgwalters-bot` | `null` | `User`, `login: cgwalters-bot` |

The REST [issue comment](https://docs.github.com/en/rest/issues/comments#get-an-issue-comment)
schema has `performed_via_github_app`, null or an app object with `id`. The
GraphQL `IssueComment` has no app field, only `author`
([schema](https://docs.github.com/en/graphql/reference/objects#issuecomment)),
so in GraphQL a user-token comment is indistinguishable from the PAT's.

So: an installation token's provenance is readable everywhere, by author;
a user token's only through REST. agent-board's input rule reads authors
through GraphQL and refuses any non-`User` author (`board-forge/src/input.rs`),
so it admits the status app only after the input-rule step of tracker#766. That
step compares an id from the caller's trusted configuration: the app id with REST
`performed_via_github_app.id`, or, in GraphQL, the app's bot account id
(`GET /users/SLUG[bot]`) with `Bot.databaseId`. Login text alone is not enough
because the `SLUG` user name of a `Bot` author is a separate account anyone may
hold (the input rule's `renovate` note). Poll and executor read comments through
REST and can compare `performed_via_github_app.id` once that is configured; until
then they keep matching `cgwalters-bot`, which user tokens preserve.

Moving review has the same prerequisite on the poll side: poll counts a review
job's verdict only when the review's GraphQL author is `github-actions[bot]`
(`ACTIONS_BOT_ID` in `board-forge/src/poll.rs`). Reviews submitted by the review
app would be ignored, so auto-merge would stop (failing closed) until poll is
configured with the review app's bot account id instead.

## Creating the apps

GitHub has no API that registers an app from a token. The REST
[Apps](https://docs.github.com/en/rest/apps/apps) endpoints create installation
tokens and manage installations; the only one that creates an app is
`POST /app-manifests/{code}/conversions`, whose `code` GitHub issues only after a
person clicks **Create GitHub App** on
`https://github.com/organizations/ORG/settings/apps/new`, within an hour
([manifest flow](https://docs.github.com/en/apps/sharing-github-apps/registering-a-github-app-from-a-manifest)).
Its response holds the app id and the private key, returned only then. So a
command can automate everything but one click per app, plus the installation,
which is also a browser step.

Prior art: fullsend's [`internal/appsetup/appsetup.go`](https://github.com/fullsend-ai/fullsend/blob/e26f6360e9c22eb8c3ab7bd0057732d21066853f/internal/appsetup/appsetup.go)
posts the manifest form from a localhost page, takes the code on a localhost
callback, converts it, stores the key at once, reuses an app whose key is already
stored, and reports a lost key (to be regenerated in the app's settings) rather
than recreating the app. Its [`internal/statuscomment/statuscomment.go`](https://github.com/fullsend-ai/fullsend/blob/e26f6360e9c22eb8c3ab7bd0057732d21066853f/internal/statuscomment/statuscomment.go)
mints a fresh client (`refreshClient`) before posting the start and again before
posting the completion, so the completion of a long run never uses the token
minted at its start. tracker#766's `apps create` step follows the
first; the actions mint step follows the second.

## Where tokens live and expiry

App credentials reach only the steps that write: apply, the status steps
(`notify`, `session`, `conclude`), review's `submit`, `refusal` and `status`, the
executor and the poll steps, the same steps that hold `CGWALTERS_BOT_PAT` or a
write-scoped job token now. Installation tokens are minted in the step that uses them, with a pinned
`actions/create-github-app-token`, from the app id (variable) and private key
(secret), narrowed to the repositories and permissions of that step.

No renewal sidecar is needed. A sidecar would have to place a new token into the
environment of a process that is already running, and only a long-lived process
holding a token needs that. Here no such process exists: agents never hold a
write token, and every writing step is short (the longest, poll's board job, is
bounded at 40 minutes; agentic-job's apply at 15 and conclude at 5; the executor
step at 8), so a token minted at the step's start outlives it. A status comment
written after a six-hour agent run is written by `conclude`, which mints its own.
A step that might outlast an hour mints again before a later write, as fullsend
does, or on a 401, inside the same process. User tokens for implement and
executor do not expire (see above), so they need neither.

## Staging test on cgwalters-forge-stage

Proves the provenance table with one app; run by the operator, since creating an
app needs the browser. Use the disposable repository cgwalters-forge-stage/board-test.

1. The operator creates one app, `agent-board-stage-test`, owned by
   cgwalters-forge-stage, with Issues: write, Pull requests: write, no webhook,
   "Expire user authorization tokens" opted out and device flow enabled, and
   installs it on cgwalters-forge-stage/board-test only. They store its id and
   private key outside any agent's reach. cgwalters-bot must be able to comment
   on board-test, and, unless the app is public, be a member of
   cgwalters-forge-stage to authorize it.
2. Installation token: mint one for the installation (for example
   `actions/create-github-app-token` in a staging workflow) and post a comment on a
   test issue with `gh api repos/cgwalters-forge-stage/board-test/issues/N/comments -f body=installation`.
   Also run `gh api user` and `gh api graphql -f query='{viewer{login}}'` with it.
3. User access token: signed in as cgwalters-bot, authorize the app with the
   device flow, then post a comment on the same issue with `body=user`, and run
   `gh api user` and the same `viewer` query.
4. Read both comments back with `gh api repos/cgwalters-forge-stage/board-test/issues/N/comments --jq '.[] | {body, login: .user.login, type: .user.type, app: .performed_via_github_app.id}'`
   and through GraphQL with `author { __typename login ... on Bot { databaseId } }`.
5. For the Workflows assumption: add Contents: write (not Workflows) to the app,
   and merge a test pull request that changes a file under `.github/workflows/`
   with an installation token (`gh pr merge --rebase`).
6. Delete the test comments and pull request branch, and as cgwalters-bot revoke
   the app's authorization (Settings, Applications, Authorized GitHub Apps).

Expected, from the documentation above except where marked:

| Check | Installation token | User access token |
| --- | --- | --- |
| REST `user.login` / `type` | `agent-board-stage-test[bot]` / `Bot` | `cgwalters-bot` / `User` |
| REST `performed_via_github_app.id` | the app's id | the app's id (assumed) |
| GraphQL `author` | `Bot` `agent-board-stage-test` | `User` `cgwalters-bot` |
| `GET /user` | refused | `cgwalters-bot` |
| GraphQL `viewer.login` | unknown | `cgwalters-bot` |
| merge touching a workflow without Workflows | refused (assumed) | not run |

**Results: pending the operator's app.** This worker cannot create an app, so none
of the above has been observed. The rows to confirm before any role moves are
`performed_via_github_app` for the user token (the executor and implement provenance
rests on it), `viewer` for the installation token, and the workflow merge.
