# SP4a PR Review Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Review GitHub PRs inside Photon: an inbox panel, a review workspace that shows each changed hunk next to the unchanged code it calls or is called by, comments anywhere with drafts that survive restarts, review progress, changes since your last review, and "Run locally".

**Architecture:**
- A new `photon_review` crate owns everything. All GitHub access goes through a `GhRunner` trait (`SystemGh` runs the `gh` subprocess; `FakeGh` records requests in tests). All git work runs the `git` subprocess with `util::command::new_command`.
- A review checks the PR head out into a Photon-managed worktree. Photon opens that worktree as a project in its own window, so language servers index the PR's code. The `ReviewItem` (a workspace item) lives in that window.
- The code column is a Zed `Editor` over a `MultiBuffer`, using the same excerpts and `BufferDiff` approach as `git_ui`'s `MultiDiffView`. Context excerpts come from `Project::{prepare_call_hierarchy, incoming_calls, outgoing_calls}`. Drafts, viewed steps and tracked checkouts live in a sqlez database.

**Tech Stack:** Rust, GPUI, `editor`/`multi_buffer`/`buffer_diff`, `project` (LSP call hierarchy, tree-sitter outlines via `language`), `db`/sqlez, `serde_json`, the `gh` CLI and `git`.

**Spec:** `docs/photon/specs/2026-10-19-sp4a-pr-review-design.md` (mockup: `docs/photon/specs/sp4-mockups/pr-review.html`, screens 1 and 3). The SP0 and SP1a specs still bind. Run locally pre-fills SP1a's block input, so SP1b (`photon/sp1b-smart-blocks`) should be merged before Task 13 to get its test summary.

## Global Constraints

- SP0 fork rules (CLAUDE.md):
  - New code goes in `crates/photon_review` (lib root `src/photon_review.rs`).
  - Zed files get minimal hooks marked `// photon: <why>` (`# photon:` in TOML).
  - Defaults go in `assets/settings/default.json`.
- **Anti-goals (verbatim from the spec, each a failure even if it works):**
  - **Posting without the user's action.** Comments and reviews reach GitHub only when the user clicks Submit or Reply; drafts stay local until then. The viewed checkbox and Resolve thread apply when clicked, as on GitHub.
  - **Losing a draft.** A crash, restart or network error must not lose typed comments. A failed submit keeps every draft and shows GitHub's error.
  - **Touching the user's work.** Review never changes the user's checkout, branch, stash or working tree.
  - **Handling credentials.** Photon never reads or stores GitHub tokens; all GitHub access goes through `gh`.
  - **Breaking Photon's promise of no network calls without a user action.** There are no background requests at startup and no polling. Network happens only on the user's actions: opening the inbox, refreshing, opening a review, toggling viewed, resolving or replying, submitting, or running locally. `photon-netcheck` (idle startup) stays clean.
  - **Slowing the editor.** The inbox and review data load off the UI thread. The workspace opens and shows the diff before the call-graph context arrives, with a visible "finding callers…" state while language servers work. SP0/SP1 performance limits still hold when no review is open.
- **Network rule in code:** only `gh` (`SystemGh`) and the git commands `clone`, `fetch`, `worktree add` and `checkout` may touch the network. They run only from the user actions listed above. Creating the panel, starting Photon and opening a window run neither.
- **No-token rule:** never pass `--show-token`, never read `hosts.yml` or the keychain, never set `GH_TOKEN`/`GITHUB_TOKEN`. Git authenticates with exactly `-c credential.helper=!gh auth git-credential` (the spec's `git -c credential.helper='!gh auth git-credential' fetch …`, passed as one argv element).
- **Never-touch-user-checkout rule:** the only writes allowed in a user's clone are:
  - refs under `refs/photon/pr/<N>/` (`head`, `base`, `reviewed`);
  - the worktree entry made by `git worktree add`.

  Every fetch passes `--no-tags --no-write-fetch-head --no-auto-maintenance`. Nothing ever runs `checkout`, `stash`, `reset` or `switch` in the user's work tree.
- `gh` runs with `GH_PROMPT_DISABLED=1`, `GH_NO_UPDATE_NOTIFIER=1`, `GH_NO_EXTENSION_UPDATE_NOTIFIER=1`, `GH_SPINNER_DISABLED=1`, `NO_COLOR=1`. Git runs with `GIT_TERMINAL_PROMPT=0`.
- Paths: everything Photon-managed lives under `paths::data_dir().join("pr_review")`:
  - worktrees go in `worktrees/<host>/<owner>/<repo>/<N>`;
  - Photon's own clones go in `clones/<host>/<owner>/<repo>` (bare, `--filter=blob:none`).
- The diff base is the merge base of the PR head and its base branch. Call-graph context is **one level** of callers and callees.
- Unchanged excerpts show **8** lines before collapsing (`UNCHANGED_EXCERPT_LINES`).
- Call-hierarchy requests retry after **1, 2, 4, 8, 15 s** (30 s total) while the server errors, then give up.
- SP1a limits, checked by `script/photon-perf` with no review open:
  - idle CPU **< 1%**;
  - footprint **≤ 200 MB**;
  - cold start **≤ 300 ms**;
  - `script/photon-netcheck` stays clean.
- Setting: `pr_review.test_command` (default `null`), settable in project settings (`.zed/settings.json`).
- Colours (Photon Black):
  - accent / changed `#19AAD8`;
  - failure / *not updated* `#FF8272`;
  - success `#B4FA72`;
  - metadata `#8E8E8E`;
  - unchanged-excerpt background `#07090b`;
  - *check* uses the theme's warning colour (`Color::Warning`).
- Non-goals: no diagrams or document review (SP4b), no agent features (SP3), no other forges, no creating/merging PRs or editing metadata, no multi-level call graphs.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Run clippy with `./script/clippy`. Tests that need git use a real temporary repo and skip when `git` is missing. No test touches the network.

## Review Focus

1. **Renamed, deleted, binary, new files and paths with spaces or unicode in the PR.** Each one gets a step. A deleted file shows its base text as removed. A binary file is a step labelled `binary file`. Drafts use the new path. Test: Task 5 `diff_handles_renames_deletes_binaries_and_spaces`.
2. **A draft's anchored text that appears more than once after a push** (a duplicated block, a moved function). The draft re-anchors to the occurrence nearest its old line, not the first match. Test: Task 10 `reanchor_prefers_nearest_duplicate`.
3. **Fork PRs, SSH remotes, GHE hosts, `.git` suffixes and case differences.** The user's clone is still found by its base-repo remote, and the head is fetched from `refs/pull/<N>/head` of the base repo. Tests: Task 3 `remote_url_forms_match`, `fork_pr_fetches_pull_ref`.
4. **No language server, or one that errors while indexing.** The diff stays usable. "finding callers…" turns into "no call information" instead of spinning forever. Tests: Task 7 `no_call_hierarchy_support_is_reported`, `context_times_out_after_retries`.
5. **Logged in to github.com but logged out of (or failing on) a GHE host.** The inbox still lists github.com PRs and shows the fix line for the failing host only. Test: Task 4 `one_host_logged_out_still_lists_others`.

---

### Task 1: Crate, `gh` runner and fake

**Files:**
- Create: `crates/photon_review/Cargo.toml` (`[lib] path = "src/photon_review.rs"`, `test-support` feature), `crates/photon_review/src/photon_review.rs`, `crates/photon_review/src/gh.rs`
- Modify: root `Cargo.toml` (member + `photon_review = { path = "crates/photon_review" }`, `# photon:`)

**Interfaces:**
- Produces:
```rust
pub struct GhRequest { pub args: Vec<String>, pub stdin: Option<String> }
pub enum GhError { NotInstalled, LoggedOut { host: Option<String> }, Failed { message: String } } // Clone, Debug, PartialEq, Display, std::error::Error
pub trait GhRunner: Send + Sync + 'static {
    fn run(&self, request: GhRequest) -> BoxFuture<'static, Result<String, GhError>>;
}
pub struct SystemGh { program: PathBuf }           // SystemGh::new() uses "gh"; SystemGh::with_program(path) for tests
pub fn gh_runner(cx: &App) -> Arc<dyn GhRunner>;   // global, SystemGh by default
pub fn set_gh_runner(runner: Arc<dyn GhRunner>, cx: &mut App);
pub fn graphql_request(host: &str, query: &str, variables: serde_json::Value) -> GhRequest;
pub fn rest_request(host: &str, method: &str, path: &str, body: Option<serde_json::Value>) -> GhRequest;
#[cfg(any(test, feature = "test-support"))]
pub struct FakeGh { /* queued (matcher, result), recorded requests */ }
#[cfg(any(test, feature = "test-support"))]
impl FakeGh {
    pub fn new() -> Arc<Self>;
    pub fn respond(&self, matches: impl Fn(&GhRequest) -> bool + Send + Sync + 'static, result: Result<String, GhError>);
    pub fn requests(&self) -> Vec<GhRequest>;
}
pub fn init(cx: &mut App); // in photon_review.rs; later tasks add registrations here
```
- **Request shapes:**
  - `graphql_request` gives `["api", "graphql", "--hostname", host, "--input", "-"]` with stdin `{"query": query, "variables": variables}`.
  - `rest_request` gives `["api", "--hostname", host, "--method", method, path]`, plus `["--input", "-"]` and the JSON stdin when `body` is `Some`.
- **`FakeGh` matching:** the first queued response whose matcher accepts the request is used and removed. If none matches, the result is `Failed { message: "FakeGh: unexpected request" }`.
- **`SystemGh` error mapping:**
  - a spawn error of kind `NotFound` gives `NotInstalled`;
  - exit code 4 gives `LoggedOut { host }`, where `host` is the `--hostname` argument if present;
  - any other non-zero exit gives `Failed { message }`, where the message is trimmed stderr followed by every `message` field found in stdout's JSON (`{"message": …, "errors": [{"message": …}]}`), joined with `: `.
- **`Display` copy (shown in the UI):**
  - `NotInstalled`: `GitHub CLI not found. Install it with "brew install gh", then refresh.`
  - `LoggedOut { host: Some(h) }`: `Not logged in to {h}. Run "gh auth login --hostname {h}" in a terminal, then refresh.`
  - `LoggedOut { host: None }`: `Not logged in to GitHub. Run "gh auth login" in a terminal, then refresh.`
  - `Failed`: the message.

- [ ] **Step 1: Write failing tests** in `gh.rs`. The `SystemGh` tests write a `#!/bin/sh` stub into a tempdir and point `with_program` at it.
```rust
#[gpui::test] async fn system_gh_passes_args_and_stdin()      // stub prints "$@" then cat: stdout == "api graphql --hostname github.com --input -\n{...}"
#[gpui::test] async fn system_gh_sets_no_prompt_env()         // stub echoes $GH_PROMPT_DISABLED$GH_NO_UPDATE_NOTIFIER -> "11"
#[gpui::test] async fn system_gh_exit_4_is_logged_out()       // --hostname ghe.corp -> LoggedOut { host: Some("ghe.corp") }
#[gpui::test] async fn system_gh_missing_binary_is_not_installed()
#[gpui::test] async fn system_gh_failure_carries_github_message() // stderr "gh: Validation Failed (HTTP 422)", stdout errors[0].message "line must be part of the diff" -> message contains both
#[test] fn request_builders()                                  // exact args and stdin JSON for both builders
#[gpui::test] async fn fake_gh_records_and_matches()           // two matchers, unmatched -> "FakeGh: unexpected request", requests() in order
```
- [ ] **Step 2: Run** `cargo nextest run -p photon_review gh`. Expected: FAIL (crate/functions missing).
- [ ] **Step 3: Implement.** For `SystemGh::run`, use `new_command(program).args(..).envs(..).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()`, write stdin, then await `output()`, all inside the returned future, so callers run it with `cx.background_spawn`.
- [ ] **Step 4: Run** `cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: photon_review crate and gh runner`.

### Task 2: GitHub data model and JSON parsing

**Files:**
- Create: `crates/photon_review/src/github.rs`, `crates/photon_review/test_fixtures/{auth_status.json, auth_status_logged_out.json, inbox.json, detail.json, graphql_error.json}` (written by hand from GitHub's GraphQL schema; no network)

**Interfaces:**
- Consumes: `GhRequest`, `graphql_request` (Task 1).
- Produces:
```rust
pub struct PrKey { pub host: String, pub owner: String, pub repo: String, pub number: u64 } // Clone, Debug, PartialEq, Eq, Hash
pub struct HostAuth { pub host: String, pub login: Option<String> } // login None when that host's active account state != "success"
pub fn parse_auth_status(json: &str) -> Result<Vec<HostAuth>>;      // output of `gh auth status --json hosts`
pub fn auth_status_request() -> GhRequest;                          // ["auth", "status", "--json", "hosts"]
pub enum InboxGroup { ReviewRequested, Yours, Mentioned }
pub enum ChecksState { Passing, Failing, Pending, Unknown }
pub enum SinceReview { NotReviewed, UpToDate, NewCommits(usize), ForcePushed }
pub enum PrState { Open, Closed, Merged }
pub struct PrSummary {
    pub key: PrKey, pub node_id: String, pub title: String, pub author: String, pub url: String,
    pub head_sha: String, pub base_ref: String, pub checks: ChecksState, pub unresolved_threads: usize,
    pub last_review_commit: Option<String>, pub since_review: SinceReview,
}
pub struct Inbox { pub sections: Vec<(InboxGroup, Vec<PrSummary>)>, pub closed_tracked: Vec<String> /* node ids */ }
pub fn inbox_request(host: &str, login: &str, tracked_node_ids: &[String]) -> GhRequest;
pub fn parse_inbox(host: &str, json: &str) -> Result<Inbox>;
pub enum Side { Left, Right }
pub struct ThreadComment { pub database_id: u64, pub author: String, pub body: String, pub created_at: String }
pub struct ReviewThread {
    pub id: String, pub path: String, pub line: Option<u32>, pub start_line: Option<u32>, pub side: Side,
    pub is_resolved: bool, pub is_outdated: bool, pub comments: Vec<ThreadComment>,
}
pub struct PrFile { pub path: String, pub viewed: bool }
pub struct PrDetail {
    pub summary: PrSummary, pub state: PrState, pub additions: u64, pub deletions: u64,
    pub files: Vec<PrFile>, pub threads: Vec<ReviewThread>,
}
pub fn detail_request(key: &PrKey, login: &str) -> GhRequest;
pub fn parse_detail(key: &PrKey, json: &str) -> Result<PrDetail>;
```
- **Inbox query:** one GraphQL request per host, with three aliased `search(type: ISSUE, first: 50)` fields:
  - `is:pr is:open archived:false review-requested:@me`
  - `is:pr is:open archived:false author:@me`
  - `is:pr is:open archived:false mentions:@me`

  plus `nodes(ids: $tracked) { ... on PullRequest { id state } }`.
  - Per PR it selects:
    - `id number title url author{login} headRefOid baseRefName repository{nameWithOwner}`;
    - `commits(last:100){nodes{commit{oid statusCheckRollup{state}}}}`;
    - `reviewThreads(first:100){nodes{isResolved}}`;
    - `reviews(last:1, author:$login){nodes{commit{oid}}}`.
  - Checks come from the last commit's rollup: `SUCCESS` gives Passing, `FAILURE`/`ERROR` give Failing, `PENDING`/`EXPECTED` give Pending, and null gives Unknown.
  - `SinceReview`:
    - no review gives `NotReviewed`;
    - a reviewed oid equal to the head gives `UpToDate`;
    - a reviewed oid at index *i* of the commit list gives `NewCommits(len - 1 - i)`;
    - a reviewed oid not in the list gives `ForcePushed`.
  - A PR in several groups appears only in the first, in the order ReviewRequested, Yours, Mentioned.
  - A tracked node whose `state` is `MERGED` or `CLOSED` goes to `closed_tracked`.
- **Detail query:** `repository(owner,name){pullRequest(number)}` with the summary fields plus:
  - `state additions deletions`;
  - `files(first:100){nodes{path viewerViewedState}}` (`VIEWED` gives `viewed = true`);
  - `reviewThreads(first:100){nodes{id isResolved isOutdated path line startLine diffSide comments(first:100){nodes{databaseId author{login} body createdAt}}}}`.

  Ponytail ceiling: 100 files/threads/comments per PR. Add `--paginate` with cursors when a real PR exceeds it.
- A response with a non-empty top-level `errors` array is `Err` with the first `message`, even when `data` is present.

- [ ] **Step 1: Write failing tests:**
```rust
#[test] fn auth_status_lists_hosts()            // github.com success login "alice"; ghe.corp state "error" -> login None
#[test] fn auth_status_logged_out_is_empty()    // {"hosts":{}} -> []
#[test] fn inbox_groups_rows_and_counts()       // 3 sections; #482 title/author "alice"/repo "acme/web"/Passing/unresolved 2
#[test] fn inbox_dedupes_across_groups()        // PR in review-requested and mentioned -> only in ReviewRequested
#[test] fn since_review_counts()                // NotReviewed, UpToDate, NewCommits(3), ForcePushed
#[test] fn inbox_reports_closed_tracked()       // tracked node MERGED -> closed_tracked == ["PR_x"]
#[test] fn detail_threads_files_and_viewed()    // thread line 42, side Right, 2 comments, databaseId; file viewed true
#[test] fn graphql_errors_are_errors()
#[test] fn inbox_request_shape()                // args == graphql_request args for "ghe.corp"; variables {"login":"alice","tracked":[...]}
```
- [ ] **Step 2: Run** `cargo nextest run -p photon_review github`. Expected: FAIL.
- [ ] **Step 3: Implement** with `serde::Deserialize` structs that mirror the GraphQL shape, mapped into the public types.
- [ ] **Step 4: Run** `cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: parse gh JSON for the PR inbox and details`.

### Task 3: Repo matching, checkout, cleanup and the review store

**Files:**
- Create: `crates/photon_review/src/checkout.rs`, `crates/photon_review/src/store.rs`

**Interfaces:**
- Consumes: `PrKey` (Task 2).
- Produces:
```rust
pub struct RepoId { pub host: String, pub owner: String, pub repo: String } // compared lowercase
pub fn parse_remote_url(url: &str) -> Option<RepoId>;
pub fn fetch_url(repo: &RepoId) -> String;                 // "https://{host}/{owner}/{repo}.git"
pub fn review_root() -> PathBuf;                           // paths::data_dir().join("pr_review")
pub fn worktree_path(root: &Path, key: &PrKey) -> PathBuf;
pub fn clone_path(root: &Path, repo: &RepoId) -> PathBuf;
pub fn candidate_repos(cx: &App) -> Task<Vec<PathBuf>>;   // open projects' repository work dirs + WorkspaceDb::recent_project_workspaces paths, minus anything under review_root()
pub async fn find_clone(candidates: Vec<PathBuf>, repo: &RepoId) -> Option<PathBuf>;
pub struct Checkout { pub repo_dir: PathBuf, pub worktree: PathBuf, pub head_sha: String, pub merge_base: String }
pub async fn prepare_checkout(key: &PrKey, base_ref: &str, fetch_url: &str, clone: Option<PathBuf>, root: &Path) -> Result<Checkout>;
pub async fn cleanup_checkout(checkout_repo: &Path, key: &PrKey, worktree: &Path) -> Result<()>;
pub(crate) async fn git(dir: &Path, args: &[&str]) -> Result<String>; // stdout; Err carries stderr
// store.rs
pub struct ReviewDb(ThreadSafeConnection);                 // Domain NAME "photon_review"; db::static_connection!
pub struct Tracked { pub key: PrKey, pub node_id: String, pub repo_dir: PathBuf, pub worktree: PathBuf }
impl ReviewDb {
    pub async fn track(&self, tracked: Tracked) -> Result<()>;
    pub fn tracked(&self) -> Result<Vec<Tracked>>;
    pub async fn untrack(&self, key: PrKey) -> Result<()>;
    pub fn last_review(&self, key: &PrKey) -> Result<Option<String>>;
    pub async fn set_last_review(&self, key: PrKey, commit: String) -> Result<()>;
}
```
- **Migration 0:** `reviews(host, owner, repo, number, node_id, repo_dir, worktree, last_review_commit, PRIMARY KEY(host, owner, repo, number)) STRICT`. Later tasks only append migrations.
- **`parse_remote_url`** accepts:
  - `https://host/owner/repo(.git)(/)`;
  - `http://…`;
  - `ssh://git@host(:port)/owner/repo(.git)`;
  - `git@host:owner/repo(.git)`.

  Anything else gives `None`.
- **`find_clone`:** for each candidate, run `git -C <candidate> rev-parse --path-format=absolute --git-common-dir`, then `git -C <candidate> remote -v`. Return the common dir's repo (`git -C` works on a bare/common dir) for the first candidate with a remote whose `parse_remote_url` equals `repo`.
- **`prepare_checkout` commands, in order.** `H` = `-c credential.helper=!gh auth git-credential`; every command runs with `GIT_TERMINAL_PROMPT=0`.
  1. If `clone` is None and `clone_path` doesn't exist: `git H clone --bare --filter=blob:none --no-tags <url> <clone_path>`. Use `clone_path` as the repo.
  2. `git -C <repo> H fetch --no-tags --no-write-fetch-head --no-auto-maintenance <url> +refs/pull/<N>/head:refs/photon/pr/<N>/head +refs/heads/<base_ref>:refs/photon/pr/<N>/base`
  3. `head_sha` = `git -C <repo> rev-parse refs/photon/pr/<N>/head`. `merge_base` = `git -C <repo> merge-base refs/photon/pr/<N>/head refs/photon/pr/<N>/base`.
  4. If the worktree is missing: `git -C <repo> H worktree add --detach <worktree> <head_sha>`. If it exists: `git -C <worktree> H checkout --detach <head_sha>`, which is the Photon worktree, never the user's.
- **`cleanup_checkout`:**
  - Runs `git -C <repo> worktree remove <worktree>` without `--force`, so a dirty worktree is kept and an error is returned.
  - Only if that succeeds, runs `git update-ref -d` on `refs/photon/pr/<N>/{head,base,reviewed}`; a missing ref is fine.

- [ ] **Step 1: Write failing tests** in `checkout.rs`. The fixture is an "upstream" bare repo with `main`, plus `refs/pull/7/head` made with `git update-ref` (no branch). The "user" clone has remote `upstream` pointing at it, a dirty file, one stash entry and a feature branch checked out.
```rust
#[test] fn remote_url_forms_match()  // https/.git/trailing slash/ssh scp/ssh:// with port/GHE host/upper case -> equal RepoId; "not a url" -> None
#[gpui::test] async fn find_clone_picks_matching_remote()
#[gpui::test] async fn checkout_fetches_private_refs_and_creates_worktree() // refs/photon/pr/7/head == upstream sha; worktree HEAD == sha; merge_base == expected
#[gpui::test] async fn checkout_never_touches_user_checkout()
    // before/after equal: `git symbolic-ref HEAD`, `git status --porcelain=v1 -uall`, `git stash list`,
    // `git for-each-ref` minus refs/photon/*, FETCH_HEAD absent, dirty file bytes
#[gpui::test] async fn checkout_reuses_worktree_and_moves_to_new_head()
#[gpui::test] async fn fork_pr_fetches_pull_ref()   // head exists only as refs/pull/7/head
#[gpui::test] async fn no_local_clone_makes_blobless_bare_clone() // under root/clones/...; rev-parse --is-bare-repository == "true"; remote.origin.partialclonefilter == "blob:none"
#[gpui::test] async fn cleanup_removes_worktree_and_refs()
#[gpui::test] async fn cleanup_keeps_dirty_worktree()  // Err, worktree and refs still there
```
  In `store.rs`: `#[gpui::test] async fn tracked_and_last_review_round_trip()` (`ReviewDb::open_test_db`).
- [ ] **Step 2: Run** `cargo nextest run -p photon_review checkout store`. Expected: FAIL.
- [ ] **Step 3: Implement.** The tests pass `file://` upstream URLs and a tempdir `root`. Production passes `fetch_url(&repo)` and `review_root()`.
- [ ] **Step 4: Run** `cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: fetch PRs into private refs and a Photon worktree`.

### Task 4: Inbox panel

**Files:**
- Create: `crates/photon_review/src/inbox.rs`
- Modify:
  - `crates/zed/src/zed.rs`: in `initialize_panels`, add `add_panel_when_ready(photon_review::InboxPanel::load(..))`; add `"photon_review"` to the action-namespace list next to `"photon_terminal"`. Both are `// photon:` hooks.
  - `crates/zed/src/main.rs`: `photon_review::init(cx); // photon: PR review`.
  - `crates/zed/Cargo.toml`: `photon_review.workspace = true # photon: PR review`.

**Interfaces:**
- Consumes: `gh_runner`, `GhError` (Task 1); `auth_status_request`, `parse_auth_status`, `inbox_request`, `parse_inbox`, `PrSummary` (Task 2); `ReviewDb::{tracked, untrack}`, `cleanup_checkout` (Task 3).
- Produces:
```rust
actions!(photon_review, [ToggleInbox, RefreshInbox]);
pub struct InboxPanel { /* workspace, state, focus_handle, load task */ }
pub enum InboxState { Idle, Loading, Loaded(Vec<HostInbox>), Failed(GhError) }
pub struct HostInbox { pub host: String, pub result: Result<Inbox, GhError> }
impl InboxPanel {
    pub fn load(workspace: WeakEntity<Workspace>, cx: AsyncWindowContext) -> Task<Result<Entity<Self>>>;
    pub fn refresh(&mut self, cx: &mut Context<Self>);
    pub fn state(&self) -> &InboxState;
}
pub struct RowMeta { pub title: String, pub meta: String, pub badges: Vec<String>, pub primary_action: &'static str }
pub fn row_meta(summary: &PrSummary, has_drafts: bool) -> RowMeta;
```
- **Panel:**
  - Right dock, `persistent_name` `"PhotonReviewInbox"`, icon `IconName::PullRequest`, tooltip `"Pull requests"`, toggle action `ToggleInbox`.
  - `refresh` runs each time `set_active(true)` is called and on `RefreshInbox`. It never runs in `load`, and there are no timers.
- **Load (off the UI thread):**
  1. Run `auth_status_request`. No hosts gives `Failed(LoggedOut { host: None })`.
  2. Hosts with a login run `inbox_request` in parallel. A host without a login gets `Err(LoggedOut { host: Some(h) })`.
  3. For each `closed_tracked` id, run `cleanup_checkout` and then `untrack`. Failures are logged and the row is kept.
- **Copy (mockup screen 3):**
  - Header: `Pull requests`. Section labels: `Review requested · {n}`, `Yours · {n}`, `Mentioned · {n}`. Host line: hosts joined with ` + `.
  - `meta`: `#{number} · {author} · {owner}/{repo}`. A host other than `github.com` prefixes the repo with `{host}/`.
  - `badges`, in this order:
    - `checks passing` / `checks failing` / `checks pending` (none for Unknown);
    - `{n} unresolved thread(s)` when n > 0;
    - `{n} new commit(s) since your review` / `force-pushed since your review`.
  - `primary_action` is `Continue review` when `last_review_commit` is Some or `has_drafts`, otherwise `Start review`. The panel passes `has_drafts = false` until Task 10 adds `ReviewDb::drafts`. Every row also has `Run locally`.
  - Empty: `No open pull requests.` Error states render `GhError`'s `Display` text.
  - Row buttons call `crate::review::open_review` (Task 9) and `crate::run_locally::run_locally` (Task 13). Until those land they are no-ops.

- [ ] **Step 1: Write failing tests** (GPUI, with `FakeGh` installed via `set_gh_runner`):
```rust
#[gpui::test] async fn panel_creation_runs_no_gh()          // add panel, run_until_parked -> fake.requests().is_empty()
#[gpui::test] async fn opening_panel_loads_groups()        // set_active(true) -> requests [auth status, inbox github.com]; Loaded with 3 sections
#[gpui::test] async fn refresh_on_open_and_manual_only()   // advance clock 1h -> no new requests; RefreshInbox -> one more round; reopen -> one more
#[gpui::test] async fn gh_missing_shows_install_hint()     // Failed(NotInstalled), Display contains "brew install gh"
#[gpui::test] async fn logged_out_shows_login_hint()       // {"hosts":{}} -> Failed(LoggedOut{host: None})
#[gpui::test] async fn one_host_logged_out_still_lists_others() // github.com Ok with rows; ghe.corp Err(LoggedOut{Some("ghe.corp")}) whose text has "gh auth login --hostname ghe.corp"
#[gpui::test] async fn closed_tracked_pr_is_untracked()    // tracked row + MERGED node -> tracked() no longer has it
#[test] fn row_meta_texts()                                // each badge, singular/plural, GHE repo prefix, Start vs Continue
```
- [ ] **Step 2: Run** `cargo nextest run -p photon_review inbox row_meta panel`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo nextest run -p photon_review -p zed`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: PR inbox panel`.

### Task 5: Diff loading

**Files:**
- Create: `crates/photon_review/src/diff.rs`

**Interfaces:**
- Consumes: `git` (Task 3).
- Produces:
```rust
pub enum FileStatus { Added, Modified, Deleted, Renamed { from: String } }
pub struct Hunk { pub old_start: u32, pub old_len: u32, pub new_start: u32, pub new_len: u32, pub body: String }
pub struct FileDiff { pub path: String, pub status: FileStatus, pub binary: bool, pub hunks: Vec<Hunk> }
pub fn parse_unified_diff(patch: &str) -> Result<Vec<FileDiff>>;
pub async fn git_diff(worktree: &Path, from: &str, to: &str) -> Result<Vec<FileDiff>>;
pub fn commentable_lines(file: &FileDiff) -> Vec<RangeInclusive<u32>>; // 1-based RIGHT-side lines inside hunks (context included)
pub async fn base_text(worktree: &Path, commit: &str, file: &FileDiff) -> Result<Option<String>>; // None for Added
pub struct LoadedFile { pub diff: FileDiff, pub contents: Option<(Entity<Buffer>, Entity<BufferDiff>)> } // None when binary
pub fn build_loaded_file(project: &Entity<Project>, abs_path: PathBuf, base: Option<String>, diff: FileDiff, cx: &mut App) -> Task<Result<LoadedFile>>;
```
- **`git_diff`:** `git -C <worktree> -c core.quotepath=false diff --no-color --no-ext-diff --find-renames -U3 --src-prefix=a/ --dst-prefix=b/ <from> <to>`.
  - Paths come from `diff --git`/`rename to`/`+++` lines, with git's C-style quoting (`"a\tb"`, octal escapes) undone.
  - `Binary files … differ` sets `binary`.
- **`base_text`:** `git -C <worktree> show <commit>:<old path>`, where the old path is the rename's `from` when renamed.
- **`build_loaded_file`:**
  - The buffer comes from `project.open_local_buffer(abs_path)`. Deleted files instead use `Buffer::local("", cx)` with the language for the path.
  - The diff is `BufferDiff::new(&snapshot.text, language, registry, cx)` followed by `set_base_text(base.map(Into::into), snapshot.text, cx)`, the same as `git_ui_core::file_diff_view::build_buffer_diff`.

- [ ] **Step 1: Write failing tests:**
```rust
#[test] fn parses_modified_added_deleted_renamed_binary()   // hand-written patch -> statuses, hunk numbers, binary flag
#[test] fn unquotes_git_paths()                              // "\"a\\tb.txt\"" and octal "caf\303\251.rs" -> real names
#[test] fn commentable_lines_include_context()               // hunk +10,7 -> [10..=16]; two hunks -> two ranges
#[gpui::test] async fn diff_handles_renames_deletes_binaries_and_spaces()
    // real repo: modify "dir/a b.txt", rename old.rs->new.rs with an edit, delete gone.rs, add logo.png, add "café.rs"
    // -> 5 FileDiffs with those paths/statuses; new.rs Renamed{from:"old.rs"}; logo.png binary; base_text(new.rs) == old.rs contents
#[gpui::test] async fn loaded_file_hunks_match_patch()      // FakeFs project; base "a\nb\nc\n", head "a\nB\nc\n" -> one hunk at row 1
#[gpui::test] async fn deleted_file_shows_base_as_removed() // contents buffer empty; one hunk whose diff_base_byte_range covers the base
```
- [ ] **Step 2: Run** `cargo nextest run -p photon_review diff`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: load a PR's diff into buffers with base text`.

### Task 6: Changed-function detection and signature changes

**Files:**
- Create: `crates/photon_review/src/functions.rs`

**Interfaces:**
- Consumes: `LoadedFile` (Task 5); `BufferSnapshot::outline_items_as_points_containing`, `OutlineItem { range, selection_range, body_range, text, depth }`; `BufferDiffSnapshot::{hunks, base_text}`.
- Produces:
```rust
pub enum FunctionChange { Added, BodyOnly, Signature }
pub struct ChangedFunction {
    pub path: String, pub name: String, pub outline_path: Vec<String>,
    pub range: Range<Point>, pub selection_point: Point, pub change: FunctionChange,
}
pub enum FileAnalysis { Functions { functions: Vec<ChangedFunction>, file_level_rows: Vec<Range<u32>> }, Unanalysable }
pub fn changed_rows(diff: &BufferDiffSnapshot, buffer: &BufferSnapshot) -> Vec<Range<u32>>;
pub fn analyse_file(path: &str, buffer: &BufferSnapshot, base: &BufferSnapshot, changed_rows: &[Range<u32>]) -> FileAnalysis;
pub fn signature_text(snapshot: &BufferSnapshot, item: &OutlineItem<Point>) -> String;
```
- **Rules:**
  - `changed_rows`: the head rows of each hunk. A deletion-only hunk counts as its single row at the deletion point.
  - `Unanalysable` when the buffer has no language or its outline is empty.
  - A changed row belongs to the **deepest** outline item whose range contains it. Rows in no item go to `file_level_rows`. Each item is listed once.
  - Base match: the item in the base outline with the same `outline_path` (ancestor names plus own name). No match gives `Added`.
  - `signature_text` is the text from `item.range.start` to `body_range.start` (or `item.range.end` when there is no body), with whitespace runs collapsed to one space and trimmed. A different `signature_text` at base and head gives `Signature`, otherwise `BodyOnly`.
  - A function deleted in the PR is not listed. Its hunk still shows.

- [ ] **Step 1: Write failing tests** (GPUI; `language::rust_lang()` buffers for base and head):
```rust
#[gpui::test] fn changed_body_is_body_only()
#[gpui::test] fn changed_params_is_signature()
#[gpui::test] fn changed_return_type_is_signature()
#[gpui::test] fn new_function_is_added()
#[gpui::test] fn nested_method_picks_innermost()          // edit inside `impl Foo { fn bar() }` -> name "bar", outline_path ["impl Foo","bar"]
#[gpui::test] fn change_outside_functions_is_file_level() // `use` line edit -> no functions, file_level_rows [0..1]
#[gpui::test] fn plain_text_file_is_unanalysable()
#[gpui::test] fn signature_ignores_whitespace_and_body()  // reformatted params only -> BodyOnly
```
- [ ] **Step 2: Run** `cargo nextest run -p photon_review functions`. Expected: FAIL.
- [ ] **Step 3: Implement.** Parse waits on `buffer.parsing_idle()` before reading outlines, and runs in `cx.background_spawn` over snapshots.
- [ ] **Step 4: Run** `cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: find changed functions and signature changes in a PR`.

### Task 7: Call-hierarchy context and impact

**Files:**
- Create: `crates/photon_review/src/context.rs`
- Modify: `crates/photon_review/Cargo.toml` (dev-deps `lsp`/`project` test-support for `FakeLspAdapter`)

**Interfaces:**
- Consumes: `ChangedFunction`, `FunctionChange` (Task 6); `Project::{prepare_call_hierarchy, incoming_calls, outgoing_calls}`, `project::{CallHierarchyItem, IncomingCall, OutgoingCall}`.
- Produces:
```rust
pub struct FunctionContext { pub callers: Vec<IncomingCall>, pub callees: Vec<OutgoingCall> }
pub enum ContextError { NoCallHierarchy, TimedOut }
pub const CONTEXT_RETRY_DELAYS: [Duration; 5]; // 1, 2, 4, 8, 15 s
pub struct ContextCache { /* HashMap<(head_sha, path, outline_path), FunctionContext> */ }
impl ContextCache {
    pub fn fetch(&mut self, head_sha: &str, project: Entity<Project>, buffer: Entity<Buffer>,
                 function: &ChangedFunction, cx: &mut App) -> Task<Result<FunctionContext, ContextError>>;
}
pub enum CallerMark { InThisPr, Unchanged, NotUpdated, Check }
pub fn classify_caller(change: Option<FunctionChange>, caller_touched: bool) -> CallerMark;
pub fn call_edges(functions: &[ChangedFunction], contexts: &[Option<FunctionContext>], cx: &App) -> Vec<(usize, usize)>; // (caller ix, callee ix)
pub enum Direction { Callee, Caller }
pub struct ContextExcerpt { pub buffer: Entity<Buffer>, pub range: Range<Point>, pub label: String, pub full_rows: u32 }
pub fn context_excerpts(function: &ChangedFunction, context: &FunctionContext, changed_rows: &HashMap<String, Vec<Range<u32>>>, cx: &App) -> Vec<ContextExcerpt>;
pub const UNCHANGED_EXCERPT_LINES: u32 = 8;
pub fn context_label(direction: Direction, path: &str, name: &str, changed_name: &str, line: u32) -> String;
```
- **`fetch`:**
  - Runs `prepare_call_hierarchy(buffer, selection_point)`, then incoming and outgoing calls for the first item (one level only).
  - `Ok(None)`, or no server with call-hierarchy capability, gives `NoCallHierarchy` immediately.
  - An `Err` is retried after each `CONTEXT_RETRY_DELAYS` entry using `cx.background_executor().timer`. When the delays run out, the result is `TimedOut`.
  - A cache hit makes no LSP request.
- **`classify_caller`:**
  - `caller_touched` gives `InThisPr`;
  - otherwise `None` gives `Unchanged`, `Some(BodyOnly | Added)` gives `Check`, and `Some(Signature)` gives `NotUpdated`.

  A caller is touched when any of its `from_ranges` rows falls in that file's changed rows.
- **`call_edges`:** an outgoing call's target is a changed function when its buffer path matches and its `selection_range.start` lies in that function's `range`.
- **`context_excerpts`:**
  - Covers callees and callers that are not changed functions, deduplicated by (path, range).
  - The range is the item's `range`. Ranges longer than 8 rows are cut to their first 8 rows, with `full_rows` keeping the real count.
  - The line is 1-based: the first call site in the changed function for a callee, or the call site in the caller for a caller.
- **Label copy:**
  - callee: `unchanged · {path} · {name}() · called on line {line}`;
  - caller: `unchanged · {path} · {name}() · calls {changed_name}() on line {line}`.

  When collapsed, the UI appends ` · collapsed · {full_rows} lines`.

- [ ] **Step 1: Write failing tests** (GPUI; fake Rust LSP with `call_hierarchy_provider: Some(Simple(true))`, set up as in `call_hierarchy`'s `setup_modal_test`):
```rust
#[gpui::test] async fn context_fetches_callers_and_callees()
#[gpui::test] async fn context_retries_until_server_ready()    // prepare errs once ("content modified"), advance 1s -> Ok
#[gpui::test] async fn no_call_hierarchy_support_is_reported() // capability off -> Err(NoCallHierarchy), no timers pending
#[gpui::test] async fn context_times_out_after_retries()       // always errs; advance 30s -> Err(TimedOut)
#[gpui::test] async fn context_cached_per_head()               // same head -> handler call count unchanged; new head -> called again
#[gpui::test] async fn edges_between_changed_functions()
#[gpui::test] async fn long_context_is_cut_to_eight_rows()     // 12-row callee -> range 8 rows, full_rows 12
#[test] fn classify_caller_marks()                             // the 5 cases above
#[test] fn context_labels()                                    // exact copy for both directions
```
- [ ] **Step 2: Run** `cargo nextest run -p photon_review context classify_caller`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: one-level call-hierarchy context and caller impact`.

### Task 8: Review map ordering

**Files:**
- Create: `crates/photon_review/src/review_map.rs` (pure, no GPUI)

**Interfaces:**
- Consumes: edge indices from `call_edges` (Task 7).
- Produces:
```rust
pub enum FileKind { Code, Config, Docs, Test, Unanalysable }
pub fn classify_path(path: &str) -> FileKind; // never returns Unanalysable; the caller sets it from FileAnalysis
pub struct MapFile { pub path: String, pub kind: FileKind, pub status_label: &'static str /* "new file" | "changed" | "deleted" | "renamed" | "binary file" */ }
pub struct MapFunction { pub path: String, pub name: String, pub first_row: u32 }
pub struct Step { pub key: String, pub title: String, pub subtitle: String, pub files: Vec<String>, pub functions: Vec<usize> }
pub fn build_review_map(files: &[MapFile], functions: &[MapFunction], edges: &[(usize, usize)]) -> Vec<Step>;
```
- **`classify_path`:**
  - **Test:** a path segment of `test`, `tests`, `__tests__` or `spec`; or a file name matching `*_test.*`, `*.test.*`, `*.spec.*` or `test_*.py`.
  - **Config:** extension `json`, `yaml`, `yml`, `toml`, `ini`, `env`, `lock` or `xml`; name `Dockerfile`, `Makefile` or `.gitignore`; or under `.github/`.
  - **Docs:** `md`, `mdx`, `rst` or `txt`.
  - **Code:** everything else.
- **Algorithm** (the spec's heuristic made deterministic):
  1. **Nodes** are the functions in `Code` files.
  2. **Union rules:**
     - functions joined by a same-file edge;
     - every strongly connected component of the full edge graph (cross-file cycles);
     - leftover singletons in the same file, which merge into one per-file cluster.
  3. **Cluster graph:** the remaining cross-cluster edges, caller → callee, form a DAG. Topologically sort it **callees first** (Kahn). Ties break by (first file path, first row).
  4. **After the clusters**, one step per file in path order for:
     - `Code` files with no functions (file-level changes only);
     - `Unanalysable`, `Config` and `Docs` files.
  5. **Last**, one step holding every `Test` file (and the functions in them).
- **Step naming:**
  - The main function is the cluster member with no incoming edge from inside the cluster; ties go to the lowest (path, row).
  - Cluster steps: `title` = `{file name} · {main}()`, `subtitle` = `{main path} · {status_label}`, `key` = the sorted `path::name` list joined with `,`.
  - File steps: `title` = file name, `subtitle` = `{path} · {status_label}`, `key` = path.
  - The tests step: `title` = `Tests`, `subtitle` = `{n} test file(s)`, `key` = `tests`.

- [ ] **Step 1: Write failing tests:**
```rust
#[test] fn callees_before_callers_across_files()  // mockup: login.ts::handleLogin -> rate_limit.ts::allow => [rate_limit step, login step]
#[test] fn same_file_connected_functions_cluster()
#[test] fn isolated_functions_in_one_file_share_a_step()
#[test] fn cross_file_cycle_merges_into_one_step()
#[test] fn non_code_after_clusters_tests_last()   // order: clusters, file steps (path order), "Tests" with subtitle "3 test files"
#[test] fn file_level_only_code_file_is_its_own_step()
#[test] fn ordering_is_deterministic()            // shuffled inputs -> identical keys
#[test] fn classify_paths()                       // table of each rule
```
- [ ] **Step 2: Run** `cargo nextest run -p photon_review review_map classify_paths`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: order a PR's changes into review steps`.

### Task 9: Review workspace item

**Files:**
- Create: `crates/photon_review/src/review.rs` (the `ReviewItem` and `open_review`)

**Interfaces:**
- Consumes: Tasks 2–8. Also `workspace::{open_paths, OpenOptions, OpenMode::NewWindow, Item}`, `MultiBuffer::{set_excerpts_for_path, add_diff, set_all_diff_hunks_expanded}`, `PathKey::with_sort_prefix`, `Editor::{for_multibuffer, insert_blocks, highlight_rows}`, `BlockProperties`/`BlockPlacement::Above`.
- Produces:
```rust
actions!(photon_review, [NextStep, PreviousStep]);
pub enum ContextState { Finding, Ready, Unavailable(SharedString) }
pub struct ImpactRow { pub location: String /* "routes/auth.ts:18" */, pub mark: CallerMark }
pub struct ReviewItem { /* project, detail, checkout, files, analyses, steps, selected, editor, multibuffer, context cache, impact */ }
pub fn open_review(summary: PrSummary, workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) -> Task<Result<()>>;
impl ReviewItem {
    pub fn new(project: Entity<Project>, detail: PrDetail, checkout: Checkout, files: Vec<LoadedFile>, window: &mut Window, cx: &mut Context<Self>) -> Self;
    pub fn steps(&self) -> &[Step];
    pub fn selected_step(&self) -> usize;
    pub fn select_step(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>);
    pub fn context_state(&self) -> &ContextState;
    pub fn editor(&self) -> &Entity<Editor>;
    pub fn impact(&self) -> &[ImpactRow];
    pub fn expand_context(&mut self, excerpt_index: usize, cx: &mut Context<Self>);
}
```
- **`open_review`** runs, off the UI thread except for entity updates:
  1. `candidate_repos` and `find_clone`.
  2. `prepare_checkout`, then `ReviewDb::track`.
  3. `open_paths([worktree], app_state, OpenOptions { open_mode: OpenMode::NewWindow, .. })`. The default `WorkspaceMatching` reuses a window already showing that worktree.
  4. `detail_request` for the PR detail, then `git_diff(merge_base, head_sha)`, `base_text` and `build_loaded_file`.
  5. Add `ReviewItem` to that window's active pane.

  Errors show as a workspace toast in the window the user clicked from, with `GhError`/git text.
- **`new`** builds an initial map from `build_review_map` with **no edges** and shows step 0 immediately. Then, in the background, it runs `analyse_file` → `ContextCache::fetch` for every changed function → `call_edges`. When those arrive it rebuilds the map, keeping the selected step's key if it still exists. `context_state` stays `Finding` until every fetch has finished.
- **Excerpts for a step:**
  - **Changed files first, in step order.** Each gets `PathKey::with_sort_prefix(i, path)` with its changed rows, `context_line_count` 3, and `add_diff`.
  - **Context files after, in order of first reference.** A context range inside a file already shown joins that file's ranges.
  - A buffer can appear under only one `PathKey`, because `set_excerpts_for_path` replaces a buffer's excerpts. So context from another file follows all of the current file's hunks.
  - Each context range gets `highlight_rows::<UnchangedContext>` (background `#07090b`) and a one-row `BlockPlacement::Above` label block with `context_label`. Collapsed excerpts add the collapsed suffix, and clicking the block calls `expand_context`, which resets that range to the full item range.
- **Layout (mockup screen 1):**
  - **Header:** `#{n} {title}`, `{author} → {base_ref}`, the checks pill, `{files} files · +{additions} −{deletions}`, `{v} of {s} steps viewed`.
  - **Left:** `Review map`, with steps numbered and a ✓ when viewed. The selected step lists its functions: changed ones in accent, unchanged neighbours (its context items) dimmed. Below that, the legend `changed in this PR` / `unchanged, pulled in by the call graph`.
  - **Middle:** the editor, with `finding callers…` above it while `Finding`. `Unavailable` shows `no call information` and its reason.
  - **Right:** `Impact of {name}()`. It shows the rows for the function under the cursor, recomputed on `EditorEvent::SelectionsChanged`; incoming calls come from the cache. Row tags:
    - `in this PR` in accent;
    - `unchanged` in metadata colour;
    - `not updated` in red;
    - `check` in warning.

    The legend copy is from the mockup, with red = signature changed and amber = body changed.
  - **Footer:** `Your review`, `{d} draft comments · {v} of {s} steps viewed`, and buttons `Comment`, `Approve`, `Request changes` (wired in Task 11).

- [ ] **Step 1: Write failing GPUI tests** (FakeFs project with a fake Rust LSP; `ReviewItem::new` gets fixture `PrDetail`/`Checkout` and files from `build_loaded_file`):
```rust
#[gpui::test] async fn workspace_shows_changed_hunk_then_unchanged_callee()
    // spec test: excerpt order [login.rs hunk, password.rs verify_password]; label "unchanged · src/password.rs · verify_password() · called on line 4"
#[gpui::test] async fn diff_shows_before_callers_arrive()  // LSP handler parked -> context_state Finding; editor text already has the hunk
#[gpui::test] async fn selecting_step_replaces_excerpts()
#[gpui::test] async fn map_reorders_when_edges_arrive()   // initial path order, then callees-first; selected key kept
#[gpui::test] async fn impact_marks_callers()             // signature change: caller in changed rows -> InThisPr, other -> NotUpdated
#[gpui::test] async fn long_unchanged_excerpt_collapses_and_expands() // 8 rows, expand -> 12
```
- [ ] **Step 2: Run** `cargo nextest run -p photon_review review::`. Expected: FAIL.
- [ ] **Step 3: Implement.** Wire the inbox's `Start/Continue review` to `open_review`.
- [ ] **Step 4: Run** `cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: PR review workspace with call-graph context`.

### Task 10: Drafts, anchoring and re-anchoring

**Files:**
- Create: `crates/photon_review/src/anchoring.rs`
- Modify:
  - `crates/photon_review/src/store.rs` (migration 1: `drafts`, `viewed_steps`);
  - `crates/photon_review/src/review.rs` (draft composer and cards, existing threads inline);
  - `assets/keymaps/default-macos.json`: `"cmd-alt-m": "photon_review::AddComment"` in context `"PhotonReview > Editor"`, with a `// photon:` comment.

**Interfaces:**
- Consumes: `PrKey`, `Side`, `ReviewThread` (Task 2); `commentable_lines` (Task 5); `ReviewItem` (Task 9).
- Produces:
```rust
// store.rs
pub struct Draft {
    pub id: Option<i64>, pub key: PrKey, pub path: String, pub side: Side, pub start_line: u32, pub end_line: u32, // 1-based
    pub commit: String, pub anchor_text: String, pub body: String, pub outdated: bool, pub general: bool,
}
impl ReviewDb {
    pub fn drafts(&self, key: &PrKey) -> Result<Vec<Draft>>;
    pub async fn save_draft(&self, draft: Draft) -> Result<i64>;   // insert or update by id
    pub async fn delete_draft(&self, id: i64) -> Result<()>;
    pub async fn delete_drafts(&self, ids: Vec<i64>) -> Result<()>;
    pub fn viewed_steps(&self, key: &PrKey) -> Result<HashSet<String>>;
    pub async fn set_step_viewed(&self, key: PrKey, step_key: String, viewed: bool) -> Result<()>;
}
// anchoring.rs
pub enum Reanchor { Placed { start_line: u32, end_line: u32 }, Outdated }
pub fn reanchor(anchor_text: &str, original_start: u32, new_text: Option<&str>) -> Reanchor;
pub enum Placement { Line { path: String, side: Side, start_line: Option<u32>, line: u32 }, NearestLine { path: String, line: u32 }, ReviewBody { path: String } }
pub fn place(draft: &Draft, commentable: &HashMap<String, Vec<RangeInclusive<u32>>>) -> Placement;
pub fn permalink(key: &PrKey, head_sha: &str, path: &str, start_line: u32, end_line: u32) -> String;
pub fn quoted_body(draft: &Draft, key: &PrKey, head_sha: &str, language: Option<&str>) -> String;
pub fn review_body(summary: &str, body_drafts: &[(&Draft, String /* quoted_body */)]) -> String;
pub fn landing_note(placement: &Placement) -> Option<String>;
// review.rs
actions!(photon_review, [AddComment]);
impl ReviewItem { pub fn drafts(&self) -> &[Draft]; }
```
- **`reanchor`:**
  - Compare lines with trailing whitespace and `\r` stripped.
  - Find every place where the anchor's lines occur consecutively, and pick the one whose start is nearest `original_start`. On a tie, the earlier one wins.
  - No occurrence, or `new_text` of None (the file is gone), gives `Outdated`.
- **`place`:**
  - `general` or `outdated` gives `ReviewBody`.
  - When the path has no commentable ranges (the file isn't in the PR), `ReviewBody`.
  - A `Side::Left` draft (on deleted base rows, always inside the diff) gives `Line` with `side: Left` and base line numbers.
  - When every line of the draft is in one commentable range, `Line`. `start_line` is None for a single line.
  - Otherwise `NearestLine`: the commentable line closest to `start_line`, with ties going to the earlier line.
- **Permalink:** `https://{host}/{owner}/{repo}/blob/{head_sha}/{path}#L{a}`, or `#L{a}-L{b}` for a range. Each path segment is percent-encoded (space → `%20`).
- **`quoted_body`** is exact copy. `{lines}` is `line {a}` or `lines {a}–{b}`. The fence is ```` ``` ````, or `~~~~` when the anchor text contains ```` ``` ````. Its shape is:
  ```
  Unchanged code, [{path} {lines}]({permalink}):

  ```{language}
  {anchor_text}
  ```

  {body}
  ```
- **`review_body`:**
  - The summary comes first, then one `### {path}` section per file in path order.
  - Each section holds its drafts' quoted bodies in line order, separated by a blank line.
  - Empty parts are omitted.
- **`landing_note` copy:**
  - `NearestLine`: `Unchanged code: GitHub can't take this comment directly, so Photon posts it on line {line} and quotes these lines.`
  - `ReviewBody` for a file not in the PR: `Unchanged file: GitHub can't take this comment directly, so Photon adds it to the review summary under {path}, with a quote.`
  - `Line`: None.
- **UI:**
  - `AddComment` maps the newest selection through `MultiBufferSnapshot::point_to_buffer_point` to (buffer, rows). It works on any excerpt, including dimmed ones and deleted base rows (`Side::Left`).
  - It creates a `Draft` with `commit` = head and `anchor_text` = those lines, and inserts a composer block below the range: a small `Editor`, `Add to review` and `Cancel`.
  - Every edit saves the latest body. At most one write is in flight, and a newer body replaces any pending one, so no timer is used.
  - `Add to review` collapses the composer into a card showing `you · on line {n} · draft` and the landing note.
  - `Cancel` on a non-empty body asks `Discard this draft?` (`Discard`/`Keep`) first.
  - Existing threads render as blocks at `line`/`side`, with the author, body, a `Reply` editor and a `Resolve`/`Unresolve` button (wired in Task 11).
- **Re-anchoring on open:**
  - Every draft whose `commit` ≠ head runs `reanchor` against the head buffer text. Placed drafts are saved with the new lines and commit.
  - Outdated drafts are saved with `outdated = true` and listed in an `Outdated drafts` section at the top of the review map. Each shows its `anchor_text` and offers `Re-place here` (moves it to the current selection and clears `outdated`) and `Post as general comment` (sets `general`).

- [ ] **Step 1: Write failing tests:**
```rust
#[test] fn reanchor_same_place()
#[test] fn reanchor_moved_lines()                  // 3 lines inserted above -> Placed{start+3,end+3}
#[test] fn reanchor_prefers_nearest_duplicate()    // text at lines 5 and 40, original 38 -> 40
#[test] fn reanchor_text_gone_is_outdated()
#[test] fn reanchor_deleted_file_is_outdated()
#[test] fn reanchor_ignores_trailing_whitespace_and_crlf()
#[test] fn place_on_diff_line()                    // single and multi-line -> Line
#[test] fn place_range_spanning_hunk_edge_uses_nearest()
#[test] fn place_unchanged_line_in_changed_file_uses_nearest_diff_line() // ties -> earlier
#[test] fn place_unchanged_file_goes_to_body()
#[test] fn place_general_and_outdated_go_to_body()
#[test] fn quoted_body_exact()                     // full string equality, incl. ~~~~ fence case
#[test] fn review_body_groups_by_file()
#[test] fn permalink_encodes_path()                // "src/a b.rs" -> ".../blob/<sha>/src/a%20b.rs#L12-L14"
#[test] fn landing_notes()
#[gpui::test] async fn drafts_round_trip()          // ReviewDb::open_test_db
#[gpui::test] async fn draft_survives_reload()      // spec: type in composer, drop item, rebuild on same db -> body present
#[gpui::test] async fn comment_on_unchanged_excerpt_creates_draft() // selection in dimmed callee -> path/lines of password.rs, note shown
#[gpui::test] async fn reanchored_draft_saved_with_new_commit()
#[gpui::test] async fn cancel_nonempty_draft_asks_first()
```
- [ ] **Step 2: Run** `cargo nextest run -p photon_review anchoring store review::`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: review drafts anywhere, saved as typed and re-anchored after pushes`.

### Task 11: Submit, reply, resolve and viewed

**Files:**
- Modify: `crates/photon_review/src/github.rs` (request builders), `crates/photon_review/src/review.rs` (wiring)

**Interfaces:**
- Consumes: `rest_request`, `graphql_request`, `gh_runner` (Task 1); `place`, `quoted_body`, `review_body` (Task 10); `ReviewDb::{delete_drafts, set_last_review, set_step_viewed}`.
- Produces:
```rust
pub enum ReviewEvent { Comment, Approve, RequestChanges } // "COMMENT" | "APPROVE" | "REQUEST_CHANGES"
pub fn review_request(key: &PrKey, head_sha: &str, event: ReviewEvent, summary: &str,
                      drafts: &[Draft], commentable: &HashMap<String, Vec<RangeInclusive<u32>>>,
                      language_for_path: &dyn Fn(&str) -> Option<String>) -> GhRequest;
pub fn reply_request(key: &PrKey, comment_database_id: u64, body: &str) -> GhRequest;
pub fn resolve_request(host: &str, thread_id: &str, resolved: bool) -> GhRequest;
pub fn viewed_request(host: &str, pr_node_id: &str, path: &str, viewed: bool) -> GhRequest;
impl ReviewItem {
    pub fn submit(&mut self, event: ReviewEvent, window: &mut Window, cx: &mut Context<Self>) -> Task<Result<()>>;
    pub fn reply(&mut self, thread_id: &str, body: String, cx: &mut Context<Self>) -> Task<Result<()>>;
    pub fn set_thread_resolved(&mut self, thread_id: &str, resolved: bool, cx: &mut Context<Self>) -> Task<Result<()>>;
    pub fn set_file_viewed(&mut self, path: &str, viewed: bool, cx: &mut Context<Self>) -> Task<Result<()>>;
    pub fn set_step_viewed(&mut self, step_key: &str, viewed: bool, cx: &mut Context<Self>); // local only
}
```
- **`review_request`:** `rest_request(host, "POST", "repos/{owner}/{repo}/pulls/{n}/reviews", body)`, where body is `{"commit_id": head_sha, "event": …, "body": review_body(summary, body drafts), "comments": [...]}`.
  - Drafts that are `outdated && !general` are left out and stay local.
  - `Line` gives `{"path", "body", "line", "side": "RIGHT"|"LEFT"}`, plus `"start_line"` and `"start_side"` for ranges.
  - `NearestLine` gives `{"path", "line", "side": "RIGHT", "body": quoted_body(..)}`.
  - `ReviewBody` drafts go into the body.
- **Other requests:**
  - **Reply:** `rest_request(host, "POST", "repos/{owner}/{repo}/pulls/{n}/comments/{id}/replies", {"body"})`.
  - **Resolve:** GraphQL `resolveReviewThread`/`unresolveReviewThread(input:{threadId})`.
  - **Viewed:** GraphQL `markFileAsViewed`/`unmarkFileAsViewed(input:{pullRequestId, path})`.
- **Submit behaviour:**
  - On success: delete exactly the sent drafts, run `set_last_review(head_sha)`, then refetch the detail (`detail_request`) so the posted threads show.
  - On failure: keep every draft and show the `GhError` text in the footer until the next submit.
  - `Comment` is disabled when there are no sendable drafts and the summary is empty.
  - The footer says `{n} outdated drafts not included` when n > 0.
- **Viewed and resolve:** the state flips at once and reverts with an inline error if `gh` fails. Step viewed-state is local only (`set_step_viewed`).

- [ ] **Step 1: Write failing tests** (GPUI with `FakeGh`):
```rust
#[gpui::test] async fn submit_sends_one_review_with_every_draft()
    // spec: drafts on a diff line, an unchanged line of a changed file, and an unchanged file;
    // exactly one request; args == ["api","--hostname","github.com","--method","POST","repos/acme/web/pulls/482/reviews","--input","-"];
    // stdin JSON: commit_id head, event "REQUEST_CHANGES", 2 comments (line 42 RIGHT; line 43 with quote), body has "### src/audit.rs"
#[gpui::test] async fn failed_submit_keeps_drafts_and_shows_error() // spec: Failed{"…line must be part of the diff"} -> drafts() unchanged, db unchanged, error text shown
#[gpui::test] async fn submit_success_clears_drafts_and_records_last_review()
#[gpui::test] async fn outdated_drafts_stay_local_on_submit()
#[gpui::test] async fn reply_posts_only_on_click()          // typing -> 0 requests; Reply -> 1 with {"body": ...}
#[gpui::test] async fn resolve_and_viewed_apply_immediately() // one request each; failure reverts state
#[gpui::test] async fn no_requests_without_user_action()    // open item, add drafts, mark steps viewed -> fake.requests() empty
```
- [ ] **Step 2: Run** `cargo nextest run -p photon_review submit reply resolve no_requests outdated`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: submit reviews, reply, resolve and sync viewed files`.

### Task 12: Changes since your review and force-pushes

**Files:**
- Create: `crates/photon_review/src/since_review.rs`
- Modify: `crates/photon_review/src/review.rs` (toggle)

**Interfaces:**
- Consumes: `git`, `Checkout`, the fetch flags (Task 3); `git_diff`, `FileDiff`, `Hunk`, `base_text` (Task 5); `last_review_commit` (Task 2 / `ReviewDb::last_review`).
- Produces:
```rust
pub enum SinceReviewPlan {
    Incremental { from: String },
    Rebased { old_head: String, old_merge_base: String },
    Unavailable { message: String },
}
pub async fn plan_since_review(checkout: &Checkout, key: &PrKey, fetch_url: &str, reviewed: &str) -> SinceReviewPlan;
pub fn author_changed_paths(old: &[FileDiff], new: &[FileDiff]) -> Vec<String>;
pub async fn since_review_files(checkout: &Checkout, plan: &SinceReviewPlan) -> Result<Vec<(FileDiff, Option<String> /* base text */)>>;
actions!(photon_review, [ToggleChangesSinceReview]);
```
- **`plan_since_review`:**
  1. If `reviewed` isn't present locally, fetch it: `git -C <repo> H fetch --no-tags --no-write-fetch-head --no-auto-maintenance <url> <reviewed>:refs/photon/pr/<N>/reviewed`. On failure, return `Unavailable` with the message `Couldn't fetch the commit you last reviewed ({short sha}). Showing the full diff.`
  2. Compute `old_merge_base` = `merge-base reviewed refs/photon/pr/<N>/base`.
  3. If `old_merge_base == checkout.merge_base` and `merge-base --is-ancestor reviewed head` holds, the plan is `Incremental`.
  4. Otherwise (a force-push, a rebase, or a merge from base) the plan is `Rebased`.
- **`since_review_files`:**
  - **Incremental:** the files of `git_diff(reviewed, head)`, with base text taken at `reviewed`.
  - **Rebased:** compare `git_diff(old_merge_base, reviewed)` with `git_diff(merge_base, head)`.
    - `author_changed_paths` keeps the paths whose sequence of hunk `body`s differs between the two, plus paths that appear in only one.
    - Those paths are shown as head against base text at `reviewed` (old path).
    - Ponytail ceiling: within a listed file, base changes from the rebase still show. Upgrade to a per-file range-diff if that bites.
- **UI:**
  - A `Changes since your review` toggle in the header, shown only when `last_review_commit` is Some and ≠ head.
  - On: the map and excerpts are rebuilt from `since_review_files` (same pipeline as Task 9).
  - `Unavailable` keeps the full diff and shows its message.

- [ ] **Step 1: Write failing tests** (real git; the fixture repo from Task 3):
```rust
#[gpui::test] async fn incremental_range_when_head_moved_forward()  // Incremental{from: reviewed}; files == only the new commit's
#[gpui::test] async fn rebase_diffs_each_version_against_its_merge_base()
    // base gets unrelated change to x.rs; PR rebased (same patch to y.rs) plus new change to z.rs -> paths == ["z.rs"]
#[gpui::test] async fn merge_from_base_uses_rebased_plan()
#[gpui::test] async fn unfetchable_review_commit_falls_back_with_message() // reviewed = 40 hex zeros+1 -> Unavailable with exact message
#[test] fn author_changed_paths_compares_hunk_bodies()                  // shifted line numbers, same bodies -> not listed
#[gpui::test] async fn changes_since_review_toggle_offered_only_when_pushed()
```
- [ ] **Step 2: Run** `cargo nextest run -p photon_review since_review`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo nextest run -p photon_review`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: changes since your review, safe across force-pushes`.

### Task 13: Run locally and `pr_review.test_command`

**Files:**
- Create: `crates/photon_review/src/run_locally.rs`
- Modify:
  - `crates/settings_content/src/project.rs`: in `ProjectSettingsContent`, add `pub pr_review: Option<PrReviewSettingsContent>` and the struct (`test_command: Option<String>`), plus the `flattened_deserialize!` options list. All `// photon:` hooks.
  - `assets/settings/default.json`: add `"pr_review": { "test_command": null }` with the comment `// photon: command pre-filled (not run) in the terminal that "Run locally" opens for a PR.`
  - `crates/terminal_view/src/photon_blocks_view.rs`: add `prefill_block_input`.
  - `crates/photon_review/src/photon_review.rs`: `PrReviewSettings` with `RegisterSetting`.

**Interfaces:**
- Consumes: `candidate_repos`, `find_clone`, `prepare_checkout`, `ReviewDb::track` (Task 3); `TerminalPanel::add_center_terminal`, `Project::create_terminal_shell(Some(worktree), cx)`; `BlockInput::set_text`.
- Produces:
```rust
pub struct PrReviewSettings { pub test_command: Option<String> }
pub fn run_locally(summary: PrSummary, workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) -> Task<Result<()>>;
// crates/terminal_view/src/photon_blocks_view.rs
impl TerminalView { pub fn prefill_block_input(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>); }
```
- **Flow:**
  1. Same checkout and worktree window as `open_review`, reusing both when they already exist.
  2. Open a center terminal with cwd = the worktree.
  3. Wait for the worktree scan, then read `PrReviewSettings::get(Some(SettingsLocation { worktree_id, path: RelPath::empty() }), cx).test_command`.
  4. If it is set, call `prefill_block_input`, which only sets the text and never writes to the PTY.
  - When the terminal has no block input (SP1a blocks off or not zsh), nothing is pre-filled and `log::info!` notes it.
- The inbox row's `Run locally` button calls `run_locally`.

- [ ] **Step 1: Write failing tests:**
  - `photon_review`, `#[gpui::test] async fn run_locally_creates_worktree_and_leaves_checkout_untouched()`. This is the spec test: real git, `FakeGh` with no expectations, and the same before/after snapshot as Task 3's `checkout_never_touches_user_checkout`. It asserts the worktree exists at `worktree_path` with HEAD = head sha.
  - `photon_review`, `#[gpui::test] fn pr_review_settings_read_from_project()`: default `None`; project-local `{"pr_review":{"test_command":"npm test"}}` gives `Some("npm test")`.
  - `terminal_view`, `#[gpui::test] async fn prefill_block_input_sets_text_without_running()`: the block input text is `"cargo test"` and the PTY write log has no `cargo test`.
- [ ] **Step 2: Run** `cargo nextest run -p photon_review run_locally pr_review_settings` and `cargo nextest run -p terminal_view prefill`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo nextest run -p photon_review -p terminal_view -p settings_content`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: run a PR locally in its own worktree`.

### Task 14: Gates and CI

**Files:**
- Modify: `.github/workflows/photon.yml` (add `-p photon_review` to the clippy and nextest lists; `settings_content`, `terminal_view`, `zed` are already there)

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Write the failing check.** Add `#[gpui::test] async fn startup_makes_no_gh_or_git_network_calls()` in `photon_review`. It initialises the crate, creates a workspace with the panel, runs until parked and advances 10 min. It asserts `FakeGh` has no requests, and that no `git` network command ran (a `#[cfg(test)]` counter in `git()` for `clone|fetch|worktree|checkout`). Make it fail first by temporarily calling `refresh` in `load`, then restore.
- [ ] **Step 2: Run the gates:**
  - `script/photon-perf` (no review open): every SP1a limit passes and the baseline doesn't regress by more than 10%;
  - `script/photon-netcheck`: clean;
  - `cargo fmt --all -- --check`;
  - `./script/clippy`;
  - `cargo nextest run -p photon_review -p terminal_view -p settings_content -p zed`;
  - `git grep -n "photon:"` lists every hook added by Tasks 1, 4, 10 and 13.
- [ ] **Step 3: Commit** `photon: PR review gates and CI`.
- [ ] **Step 4: Hand off for the user's eyes.** Review a real PR on github.com end to end (inbox → review → draft on unchanged code → Submit) and compare it with mockup screens 1 and 3. Record what differs as follow-ups. No code changes go in this step.
