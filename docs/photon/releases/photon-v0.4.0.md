PR review: review a GitHub pull request with the diff and the code around it in one place.

## Sub-project 4a: PR review

- **PR inbox** (a dock panel):
  - Lists open PRs on every host `gh` is logged into, in three groups: *Review requested*, *Yours* and *Mentioned*.
  - Each row shows the checks status, unresolved threads and "N new commits since your review".
  - Row actions: **Start/Continue review** and **Run locally**.
  - It loads only when you open or refresh it. There is no polling.
- **Review workspace:**
  - **Review map:** the change is grouped into ordered steps, with callees before callers and tests last.
  - **Diff with context:** each changed hunk is followed by the unchanged code it calls or is called from, dimmed.
  - **Impact panel:** lists every caller of the function under the cursor. A caller is marked **not updated** when the PR changed the function's signature but not that caller.
- **Comments anywhere:**
  - Select any line, changed or not, and press cmd-alt-m to write a draft.
  - Drafts save as you type, survive restarts, follow their lines when the author pushes, and can be edited or deleted before you submit.
  - Comments on unchanged code land on the nearest diff line, or in the review summary, with a quote and a permalink.
- **Submit** sends one review: Comment, Approve or Request changes.
  - Reply, Resolve and the Viewed checkbox work as on GitHub.
  - Nothing is posted without your click, and nothing is posted twice, even after a crash.
  - A failed submit keeps every draft and shows GitHub's error.
- **Changes since your review** shows only what the author changed after your last review, including across a rebase. Reviews you submitted on github.com count too.
- **Run locally:**
  - Opens a terminal in a separate copy of the PR's code, so your own checkout, branches and stash are never touched.
  - Pre-fills `pr_review.test_command` without running it. Set it in your settings or in your clone's `.zed/settings.json`.

## Safety

- **No code from the PR runs** until you trust it:
  - no git hooks;
  - no language servers until you click Trust;
  - links in the PR are never followed.
- **Trust doesn't carry over.** It is asked again when the author pushes new commits.
- **Your GitHub credentials stay with `gh`.** Photon never reads or stores tokens.

## Needs

The GitHub CLI: `brew install gh`, then `gh auth login`.

## Installing

This build is for Apple Silicon Macs and isn't notarised by Apple. After dragging Photon to Applications, run this once:

```
xattr -dr com.apple.quarantine /Applications/Photon.app
```

Alternatively, right-click Photon and choose Open the first time.
