Smart blocks: Photon's terminal blocks now understand what each command did.

## Sub-project 1b: smart blocks

- **Test and build runs** from cargo, nextest, pytest, jest, vitest, go, rustc, tsc and gcc/clang get:
  - a summary chip, such as `2 failed · 41 passed`;
  - **Re-run failed** (ctrl-cmd-r), which runs only the failing tests;
  - **Open errors**, which opens each failure location beside the terminal; step through them with F8 / shift-F8;
  - a comparison with the previous run, such as `1 new failure · 2 fixed`.
- **`git status`** blocks get **stage**, **diff** and **discard** buttons on each file (discard always asks first), plus **Stage all** and **Open in Git panel**.
- **`git log`**: cmd-click a commit hash to open that commit.
- **Dev servers** get an **Open localhost:5173** chip and a **Stop** button.
- **Notifications:** a macOS notification when a command that ran 10 s or longer finishes while Photon is in the background, and when a backgrounded dev server prints an error, at most once a minute. Click the notification to jump to the block. Set the threshold with `terminal.notify_after_seconds` (`0` turns notifications off).
- **Output views:** **Open as JSON** (folding and highlighting), **Open as table** (sortable and filterable) and **Copy as CSV**, for JSON output and column-aligned output like `kubectl get` or `docker ps`.
- Every action is also in the block's right-click menu and the command palette.
- **Off switch:** `"terminal": { "smart_blocks": false }` restores plain blocks.

## Also in this release

- Photon's new wave icon.
- Blocks stay aligned when side panels open and the terminal narrows.
- Agent CLIs like Devin and Claude Code no longer switch the terminal back to the classic view.
- `clear` leaves an empty screen with the input at the bottom.

## Installing

This build is for Apple Silicon Macs and isn't notarised by Apple. After dragging Photon to Applications, run this once:

```
xattr -dr com.apple.quarantine /Applications/Photon.app
```

Alternatively, right-click Photon and choose Open the first time.
