# Photon SP4a: PR Review

Status: draft, builder-checked · 2026-10-19 · mockups the user approved: `docs/photon/specs/sp4-mockups/pr-review.html` (screens 1 and 3 are SP4a; screens 2 and 4 are SP4b).

## Why

Today the user reviews a PR in two places that each show half the picture:
- **GitHub** has the diff and the comments.
- **The pulled branch in an editor** has the surrounding code.

In their words, they want "an experience that lets me see the diff and other part of the code that is part of the call graph but unchanged so that I can look at it holistically", and to comment right there. Photon already has the code, the language servers and the terminal, so it can show both halves together.

The user moved PR review ahead of SP2 (workspace tabs) and SP3 (agents).

## Scope split

- **SP4a (this document):**
  - the review workspace;
  - comments anywhere;
  - the impact panel;
  - review progress;
  - the PR inbox;
  - "Run locally".
- **SP4b (next):**
  - the call-graph diagram view, which also works for general code reading, with Mermaid/Excalidraw export;
  - document review (rendered Markdown with inline changes and comments).
- **Later:** code-understanding tools such as smell metrics and the SonarLint language server. SP3 adds agent PR summaries and "explain this flow".

## Goals

1. **PR inbox.** A panel listing open PRs on every host `gh` is logged into (github.com and GitHub Enterprise).
   - Groups: *Review requested*, *Yours*, *Mentioned*.
   - Each row shows the title, number, author, repo, checks status, unresolved-thread count, and "N new commits since your review" when the user reviewed before.
   - Row actions: **Start/Continue review** and **Run locally**.
   - Refresh happens when the panel opens and on a manual refresh. There is no polling.
2. **Review workspace.** This is a center item for one PR (mockup screen 1).
   - **Review map (left).** The change grouped into ordered steps, each a group of related changed functions or files. The selected step expands to list its functions: changed ones bright, unchanged neighbours dim.
   - **Code (middle).** One scrollable view built on Zed's multi-excerpt buffer, with these excerpts for the selected step:
     - each changed hunk, with additions and deletions shown inline against the PR base;
     - below each, the **unchanged** code it calls or is called by (callers and callees from the language server's call hierarchy, one level deep), shown dimmed with a label such as `unchanged · called on line 43`.
     Unchanged excerpts are collapsed beyond a few lines and expand on click. It is a real editor: go-to-definition, hover and search all work.
   - **Impact (right).** For the function under the cursor, every caller in the repo, marked:
     - *in this PR*;
     - *unchanged*;
     - **red, *not updated***: the PR changed the function's signature (parameters or return type, compared at base and head) and didn't touch this caller;
     - **amber, *check***: the PR changed only the body, and didn't touch this caller.

     (Claude's call, resolving the builder check: the mockup's red example becomes amber in that case.)
   - **Review actions:** draft-comment count, steps viewed, and Comment / Approve / Request changes.
3. **Comments anywhere.**
   - Select a line or range on any excerpt, changed or unchanged, and write a draft comment.
   - Existing PR threads show inline where GitHub put them. The user can reply and resolve.
   - Drafts are saved locally as they are typed, and survive restarts.
     - Each draft stores its file, line range, the commit it was written on, and the text of those lines.
     - When the PR head moves, a draft re-anchors to the same text in the new version.
     - If that text is gone, the draft is shown as *outdated*, with its original context. The user can re-place it or post it as a general comment. Nothing is dropped. (Claude's call.)
   - **Submit** sends the review in one GitHub review with all draft comments, as Comment, Approve or Request changes.
   - **Comments on unchanged code:**
     - In a file the PR changed, the comment is posted on the nearest diff line in that file. The body starts with a quote of the commented lines and a permalink to them at the PR head.
     - In a file the PR didn't change, it goes into the review's top-level body under a heading per file, with the same quote and permalink.
     - The draft shows a note saying where it will land.
4. **Review progress.**
   - Mark each step and file *viewed*. File viewed-state syncs with GitHub's own "Viewed" checkbox, applied immediately on click. So is "Resolve thread", the same as GitHub's UI. (Claude's call.)
   - When the author has pushed since the user's last submitted review, the workspace offers **Changes since your review**.
     - "Last review" is GitHub's record of the user's latest submitted review on that PR (its commit). It is fetched for all inbox rows in one GraphQL query, so reviews submitted on github.com count too.
     - After a force-push, each version is diffed against its own merge base, so rebased base changes don't appear as the author's.
     - If the old commit can't be fetched, Photon falls back to the full diff and says so.
     (Claude's calls.)
5. **Run locally.**
   - Checks the PR head into a separate git worktree (Photon-managed, outside the user's checkout). The user's own branch and working tree are never touched.
   - Opens a terminal block there. If the project sets `pr_review.test_command`, that command is pre-filled in the input box, not run, so SP1b's test summary applies when the user runs it.

## Non-goals (SP4a)

- **Visual diagrams and document review.** These are SP4b.
- **Agent features.** PR summaries and explanations are SP3.
- **Other forges.** GitLab and Bitbucket are out.
- **Creating or merging PRs, and editing PR metadata.** Labels, reviewers and the like are out.
- **Multi-level call graphs.** The workspace shows one level of callers and callees; deeper exploration is the SP4b graph view.

## Anti-goals (a failure even if it works)

- **Posting without the user's action.**
  - Comments and reviews reach GitHub only when the user clicks Submit or Reply; drafts stay local until then.
  - The viewed checkbox and Resolve thread apply when clicked, as on GitHub.
- **Losing a draft.** A crash, restart or network error must not lose typed comments. A failed submit keeps every draft and shows GitHub's error.
- **Touching the user's work.** Review never changes the user's checkout, branch, stash or working tree.
- **Handling credentials.** Photon never reads or stores GitHub tokens; all GitHub access goes through `gh`.
- **Breaking Photon's promise of no network calls without a user action.**
  - There are no background requests at startup and no polling.
  - Network happens only on the user's actions: opening the inbox, refreshing, opening a review, toggling viewed, resolving or replying, submitting, or running locally.
  - `photon-netcheck` (idle startup) stays clean.
- **Slowing the editor.**
  - The inbox and review data load off the UI thread.
  - The workspace opens and shows the diff before the call-graph context arrives, with a visible "finding callers…" state while language servers work.
  - SP0/SP1 performance limits still hold when no review is open.

## How it works (Claude's calls; the builder may refine)

- **GitHub access.** Everything goes through the `gh` CLI as a subprocess.
  - Git fetches use `gh`'s credential helper (`git -c credential.helper='!gh auth git-credential' fetch …`), so no git credential setup is needed and GitHub Enterprise works the same way. (Claude's call.)
  - Commands: `gh pr list/view/diff --json`, and `gh api` (REST/GraphQL) for review threads, viewed-state, and creating reviews with comments.
  - Hosts come from `gh auth status`.
  - If `gh` is missing or logged out, the panel shows how to fix it (`brew install gh`, `gh auth login [--hostname …]`) instead of failing silently.
- **Code at the PR head.** Reviewing needs the head's code for language servers.
  - **Where the code comes from (user's choice, option 1):** when the user already has a clone of the PR's repo, Photon uses it.
    - It fetches the PR head and base into private refs (`refs/photon/pr/<N>/head`, `…/base`) in that repo.
    - It checks the head out into a separate Photon-managed worktree outside the user's folder.
    - The user's branches, working tree and stash are never touched. Their `.git` gains only those refs and the worktree entry, which Photon removes when it cleans up the review.
    - For repos the user hasn't cloned, Photon makes its own lightweight clone (`--filter=blob:none`) under its data dir.
    - A clone matches a PR when one of its remotes points at the PR's host and owner/repo.
  - The worktree is reused across sessions for that PR.
  - The worktree is opened as a project in its own window, so language servers index the PR's code. The review workspace item lives in that window.
  - The diff base is the merge base of the PR head and its base branch, the same as GitHub's diff.
- **Review map ordering (no agent).** A heuristic, not AI:
  - Changed functions are found via tree-sitter outlines over the diff hunks.
  - They are clustered by call relationships among themselves, using the language server's call hierarchy.
  - Clusters are ordered by dependency (callees before callers), then config and non-code files, then tests last.
  - Each cluster becomes a step named after its main file and function.
  - Files that can't be analysed (no language server, data files) become their own steps.
- **Context excerpts.** Built from the call hierarchy (incoming and outgoing calls), via the `project` crate's existing call-hierarchy requests (`GetIncomingCalls` / outgoing calls) and the existing `call_hierarchy` crate's types.
  - Results are cached per PR head commit.
  - Impact = incoming calls of the changed function across the whole worktree.
- **Diff rendering.** Reuses Zed's buffer-diff and multi-excerpt editor (`buffer_diff`, `editor` multibuffer, the same approach as `git_ui`'s project diff), with Photon's dimmed style for unchanged excerpts.
- **Storage.** Drafts, viewed-state for steps, and "last reviewed commit" go in Photon's database (sqlez), keyed by host, repo and PR number.
- **Placement.**
  - New code goes in a new `crates/photon_review` crate. The inbox is a dock panel.
  - Zed files get only minimal `// photon:` hooks (registration, a menu or keybinding, settings).
  - Settings live in `assets/settings/default.json` under `pr_review` (for example `test_command`).
- **Fork rules** as in SP0.

## Testing

- **Unit tests:**
  - parsing `gh` JSON output (PR lists, threads, review comments);
  - review-map clustering and ordering on fixture call graphs;
  - anchoring comments on unchanged code (nearest diff line in the same file; review body for other files);
  - "changes since your review" range computation;
  - signature-change detection for *not updated* callers.
- **Fake `gh`:** a test-support fake `gh` (a stub executable or an injectable runner) so inbox, threads and submit flows are tested without the network. Tests assert the exact `gh` arguments and request bodies sent on Submit.
- **GPUI tests:**
  - the workspace shows a changed hunk followed by an unchanged callee excerpt (with a fake language server returning call-hierarchy results);
  - a draft survives a reload;
  - Submit sends one review with every draft;
  - a failed submit keeps drafts and shows the error;
  - Run locally creates a worktree and never changes the original checkout.
- **Gates:** `photon-perf` (no review open), `photon-netcheck`, fmt, clippy.
- **The user's eyes:** a real PR on github.com reviewed end to end, compared with the mockup.

## Left to the builder

- Exact panel and workspace styling within Photon Black, matched to the mockups.
- The `gh` API calls used for each operation (REST vs GraphQL).
- Worktree location and cleanup policy (e.g. under Photon's data dir, with removal when the PR is merged or closed).
- How many lines an unchanged excerpt shows before collapsing.
