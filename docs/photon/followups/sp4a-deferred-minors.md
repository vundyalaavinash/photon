# SP4a deferred minors

Findings the reviews graded Minor and deferred while building PR review (branch `photon/sp4a-pr-review`). None loses data, posts without a click, or runs PR code.

- **Task 1:** stdin write sequential with output read (theoretical hazard for a child writing >64 KB before reading stdin; not gh api)
- **Task 1:** Failed { message: "" } shown when gh prints nothing — fall back to "gh exited with {status}"
- **Task 1:** env test checks only 2 of 5 gh env vars
- **Task 2:** author_login body duplicated inline; test gaps (active-account choice, CLOSED tracked node)
- **Task 3:** base_ref not validated as a ref name (defence in depth only); unwrap in store test
- **Task 3:** --no-recurse-submodules not in the asserted flag list; unix-only PermissionsExt in tests (also gh.rs); spurious NotFound log on failed clone
- **Task 3:** checkout/worktree add lack -c submodule.recurse=false (user config submodule.recurse=true); Windows drive-prefix segments not rejected
- **Task 4:** refresh button dispatches through window rather than cx.listener; cleanup test polls real git with timers
- **Task 4:** closed_tracked_pr_is_untracked now exercises the mismatch branch, not the normal cleanup path; mismatched-but-real paths (moved data dir) leak silently with a log line; in-flight set per panel (two windows can race on the exact path)
- **Task 4:** ensure_safe_segment doesn't reject Windows drive prefixes ("C:") — Photon is macOS-only today
- **Task 5:** test gaps (empty new/deleted file, mode-only, Added via build_loaded_file, \ No newline on a real patch); non-UTF-8 paths decoded lossily
- **Task 5:** new/deleted gitlink (new file mode 160000) not tested end to end
- **Task 6:** deletion-only hunk at the first row of the next function marks it BodyOnly; display-text fallback when name_ranges empty
- **Task 6:** dropping `,` before `)` equates `(u32,)` 1-tuples with `(u32)`; language fallback branch untested
- **Task 6:** TS `function f(): { a: number } {` header cut before return type; `functionName = …` counts via substring; tuple enum variants / fn-typed fields / TS pairs count as callable; unbalanced `<` or bracket in a string default runs the header into the body
- **Task 6:** `=>` cut fires on default-value arrows inside params and on arrow return types (misses later changes; safe direction); generic methods `type<T>(…)` classed as types
- **Task 7:** 30 s spinner on genuinely empty prepare answers (kept; better signal = LSP progress); changed items dropped by row overlap rather than ChangedFunction membership; concurrent fetches for the same key both query
- **Task 7:** no test for success just before the 30 s cap; inverted-range test passes without the clamp (guard unpinned)
- **Task 8:** unchecked indexing guarded by the upstream filter
- **Task 8:** 200k-chain test exercises the pipeline in one file, not deep cross-file SCC recursion
- **Task 9:** toast lost when the source window closes during a long fetch; selection follows its step when edges reorder the map (visible jump)
- **Task 9:** exhausted reload retries or a dirty buffer analyse non-head text with only a log line; double-click guard untested; same-head shortcut trusts a stale inbox head; window-opening step untested
- **Task 10:** re-indented or moved blocks make drafts outdated; cross-buffer selections shrink silently; drafts read on the UI thread; inbox draft marker updates on refresh only; fixed block heights
- **Task 10:** stale save error stays after a successful delete; failed delete after Discard is silent (draft reappears next open); Cancel on a reopened composer deletes the whole saved draft (prompted) rather than reverting the edit
- **Task 11:** draft edited during posting gets no "earlier version posted" marker; Request changes with nothing to send relies on GitHub's 422; COMMENT with empty body + comments needs one real-PR check
- **Task 11:** during a submit, outdated maybe-posted drafts briefly show "Post as general comment"
- **Task 11:** emptied-then-retyped draft during the post stays stored as Sending (shows "may already be posted"); `|d|` short name at review.rs:1192; quoted-context fence language captured at click time
- **Task 12:** merge-base uses the current base tip (rewritten base branch edge); the since-review fetch isn't named in the plan's network list (user action)
- **Task 12:** binary whose base version moved in a rebase is listed though the author didn't touch it (errs toward showing); file viewed setter's since-review guard untested; loading state tested by setting the flag
- **Task 13:** remote_connection dev-dep workaround; no test for no-block-input / blank test_command branches; checkout test module made pub(crate)
- **Task 13:** clone settings read drops permission/UTF-8 errors unlogged and isn't size-bounded; no test for the Photon-clone (starts_with review_root) branch; window reuse untested (needs multi-window harness)
- **Task 14:** COMMAND_LOG global (reliable under nextest only); git half of the startup test not mutation-checked; cold start +8.6% vs baseline (thin margin)
- **Final:** private refs keyed only by PR number collide across repos in one clone; Run locally + Start review on the same PR can race (transient toast); drafts of merged/closed PRs become unreachable; quoted_body fence breaks if text contains both ``` and ~~~~
- **Final:** crash between checkout and saved trust-forget can keep old trust after a session restore; null-node PRs leave worktree/refs/composer rows on disk; old_commit_is_fetched_into_the_private_ref flaked once (tests not isolated from global git config); forget_trust is ~35 lines of logic inside trusted_worktrees.rs

## Rulings made during the build

- Task 1: Ruling: implementer commits keep the trailer of the model that wrote them (Claude Sonnet 5.5 for sonnet implementers) — accurate attribution; the session's attribution reminder says so — cost if wrong: trailer text on some commits.
- Task 3: Ruling: git auth resets the helper list first — `-c credential.helper= -c credential.helper=!gh auth git-credential` — overriding the plan's "exactly one -c" wording; otherwise macOS osxkeychain (first in the list) stores gh's token after a successful fetch, breaking the spec's "Photon never reads or stores GitHub tokens" — cost if wrong: a user who relies on another helper for the same host is not consulted (gh is the only auth path by design).
- Task 3: Ruling: every Photon-driven git command runs with `-c core.hooksPath=/dev/null` — checkout of a PR head must never execute hooks the PR or its tooling configures (husky/LFS), since reviewing untrusted code must not run it — cost if wrong: LFS smudge/post-checkout conveniences don't run in review worktrees.
- Task 4: Ruling: a panel restored open (dock restoration) does not load; it shows "Refresh to load" until the user's first explicit activation or Refresh in that session — spec anti-goal "no background requests at startup", photon-netcheck idle startup must stay clean — cost if wrong: one extra click after restart.
- Task 4: Ruling: inbox cleanup runs in a detached task never cancelled by refresh; a failed `worktree remove` retries with `--force` on Photon's own worktree path only (never `worktree prune`, which would touch the user's other worktrees) — cost if wrong: a dirty Photon review worktree (uncommitted edits made in it) is discarded when its PR closes.
- Task 6: Ruling: only visibility (`pub`, `pub(...)`) is ignored when comparing signatures; `async`, `const`, `unsafe` and `extern "…"` stay part of the signature — adding async/unsafe changes how callers must call the function (await / unsafe block) — cost if wrong: a const-fn toggle shows callers as red "not updated".
- Task 6: Ruling: an item is callable when its header (text before the body, after visibility/modifiers) contains a `(` and does not start with a type keyword (class, struct, interface, enum, trait, impl, record, type, mod, module, namespace, object) — language-agnostic for C-like, Go, Python, JS/TS, Java/Kotlin and Rust; a Rust bodiless `fn` still counts — cost if wrong: a few languages' odd constructs misclassified (e.g. Kotlin primary-constructor classes are excluded by keyword; Python `class A(B)` excluded by keyword).
- Task 9: Ruling: Photon never auto-trusts a PR worktree — a PR is untrusted code (rust-analyzer would run build.rs/proc-macros); the review shows Unavailable("Trust this worktree to find callers") with a button that triggers Zed's own trust flow, and reloads call context on the Trusted event; the 30 s cap starts only once trusted — cost if wrong: one click per PR before callers appear.
- Task 10: Ruling: Edit and Delete actions on draft cards are added in Task 10 although the plan gives no task for them — the spec's draft workflow needs a way to fix or remove a comment before Submit, and an undeletable empty draft would make Submit fail every time — cost if wrong: two buttons.
- Task 13: Ruling: `pr_review.test_command` is read from the user's settings, then from the user's own clone's `.zed/settings.json` (its working tree on disk), never from the PR head worktree; Photon-made clones use user settings only — the spec's "project sets" means the user's project, and a PR author must not choose a command the reviewer may run with one Enter — cost if wrong: a team that only sets the command inside the PR branch gets no prefill.
- Final: Ruling: residual fixed by refusing to move a PR worktree's head while any window has it open — toast "Close the window for PR #<N> to load its new commits" — rather than stopping language servers in place; refusing never lets untrusted code reach a running server and needs no Zed-file change — cost if wrong: one extra window close after an author pushes.
