The first Photon release: a fast, low-memory, terminal-first editor built on Zed, with Warp's block terminal.

## Sub-project 1a: block terminal (zsh)

- **Command blocks.** Each command and its output form one block, showing the folder, git branch and duration. Failed commands get a red edge. Click a block to select it; cmd-↑/↓ moves between blocks. With a block selected:
  - cmd-shift-c copies the command;
  - cmd-alt-c copies the output;
  - cmd-alt-shift-c copies both;
  - cmd-shift-r runs it again.
- **Warp-style input** pinned to the bottom of the terminal. It's a real text box with mouse selection, undo and multi-line editing (shift-enter). ↑ walks your history.
- **Folder and branch chips.** Click one to jump to a recent folder or switch git branch.
- **Search and completion.** ctrl-R searches your history across projects, and Tab completes using your own zsh setup.
- **Find and open.** cmd-F searches within a block. cmd-click a `path:line` in the output to open it beside the terminal; ctrl-cmd-t switches between the terminal and the editor.
- **Full-screen programs work as usual.** That includes vim, htop, less, ssh, sudo and REPLs.
- **Escape hatch.** `"terminal": { "blocks": false }` brings back the classic terminal.

## Sub-project 0: lean baseline

- Rebranded as Photon, with the Photon Black theme and Hack 13 font.
- No network calls at startup: no account, collaboration, telemetry or automatic updates.
- Measured: cold start under 300 ms and about 130 MB of memory at idle.

## Installing

This build is for Apple Silicon Macs and isn't notarised by Apple. After dragging Photon to Applications, run this once:

```
xattr -dr com.apple.quarantine /Applications/Photon.app
```

Alternatively, right-click Photon and choose Open the first time.
