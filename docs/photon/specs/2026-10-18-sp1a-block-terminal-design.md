# Photon SP1a: Block Terminal (core)

Status: draft for review · 2026-10-18

## Why

The user's favourite thing about Warp is how it feels: output grouped into command blocks, and a prompt area pinned to the bottom with clickable chips for the folder and git branch. Photon's center terminal (SP0) is still a classic grid. SP1a gives it Warp's core feel. Smart, per-command blocks are SP1b. Anything that needs an agent is SP3.

## Goals

1. **Command blocks.** Each command and its output form one block. A block shows:
   - the command;
   - its output;
   - the folder and git branch it ran in;
   - how long it took;
   - its exit status, with a red left edge when non-zero.

   Clicking a block selects it, with the accent-coloured left edge `#19AAD8` and a tinted background. cmd-↑/cmd-↓ moves the selection, as in Warp. A selected block offers four actions: copy command, copy output, copy both, and re-run. The newest block sits directly above the input.
2. **Editor-style input**, pinned to the bottom of the terminal:
   - a real text box: mouse to place the cursor, select, cut/copy/paste, undo;
   - multi-line with shift-enter;
   - Enter runs the command;
   - ↑ at the first line walks shell history;
   - syntax-neutral, in Hack 13.
3. **Folder chip** (`📁 ~/path ▾`). Clicking opens a picker of recent folders (from block history) with fuzzy search. Choosing one runs `cd -- '<path>'` in the shell, so the shell stays the source of truth. The picker also has "Open folder in Photon" (adds it as a project).
4. **Git branch chip** (`⎇ branch ▾`). It shows when the current folder is in a git repo, with a small "± N changed" count. Clicking opens a filterable list of local branches (current one highlighted) plus "New branch…". Choosing one runs `git switch <branch>` (or `git switch -c <name>`) as a normal block, so git's own errors, such as uncommitted changes, show as a failed block.
5. **Full-screen programs keep working.** Programs that use the alternate screen (vim, htop, less, fzf) take over the terminal exactly as in a classic terminal. Blocks reappear when they exit. Commands that read input while running (`ssh`, `python`, `read -p`, sudo password prompts) receive keystrokes directly. Their session is one live block until it ends.
6. **Find and history (H):**
   - cmd-F searches within the selected block (or all blocks);
   - a long block's command header stays sticky at the top while you scroll through its output;
   - ctrl-R opens history search across all projects, showing command, folder, exit status, and when it ran.
7. **Opening paths in the editor.** cmd-click on `path:line` in output opens the file in a split beside the terminal, not replacing it. A keybinding toggles focus between the center terminal and the last editor.

## Non-goals (SP1a)

- **Anything that needs an agent**: fix with agent, agent commands as blocks, `#` plain English, and AI commit messages. All of these are SP3.
- **Smart per-command blocks**: test/build summaries, git action buttons, live dev-server blocks, tables and JSON. These are SP1b.
- **Shells other than zsh.** bash and fish get the classic terminal for now; the integration should be shaped so they can be added later.
- **Blocks inside `ssh` sessions and SSH remote projects.** Those terminals stay classic or show one block.
- **Restoring blocks after an app restart.** History search still covers them.
- **Photon's own tab completion engine.** See "Tab completion" below.

## Anti-goals (a failure even if it works)

- **It breaks an interactive program.** If anything goes wrong, Photon falls back to the classic terminal rather than mangling a program.
- **It edits the user's `~/.zshrc`** or changes how their shell behaves outside Photon.
- **It's noticeably slower or heavier than SP0.** That means:
  - idle footprint stays ≤ 200 MB (the 10% baseline gate applies);
  - cold start stays ≤ 300 ms;
  - typing in the input box doesn't feel slower than typing in the editor.
- **It makes network calls.** The `script/photon-netcheck` result must stay clean.
- **It breaks a performance limit.** These are checked automatically and fail the build:
  - idle CPU stays under 1% with one terminal open at a prompt;
  - printing 100,000 lines takes at most 10% longer than with `terminal.blocks: false`;
  - the integration adds under 1 ms per command (marks plus block bookkeeping);
  - typing latency in the input box is no worse than in the editor.

  The design is event-driven: no timers or polling beyond the existing cursor blink.
- **The user has no way back.** A `terminal.blocks` setting (default `true`) turns blocks off and restores the classic SP0 terminal.

## Decided

- **zsh only** (user's choice).
- **Scope split.** SP1a (this document) comes before SP1b (smart blocks); agents come later (user's choice).
- **Look.**
  - Warp Dark, the Photon Black theme from SP0: background `#050505`, accent `#19AAD8`, failure edge `#FF8272`, metadata text `#8E8E8E`.
  - Hack 13, line height 1.2.
  - Element positions and spacing match Warp's (user's requirement from SP0).
  - The mockups the user approved are in `docs/photon/specs/sp1a-mockups/` (`blocks-and-prompt.html`, `smart-blocks.html`). The non-agent parts of `smart-blocks.html` are SP1b.
- **Tab completion asks the user's real zsh** (user's choice, option B). See "Tab completion" below.

## How it works (Claude's calls; the builder may refine)

- **Shell integration.**
  - **Loading.** For zsh, Photon starts the shell with `ZDOTDIR` pointing at a Photon-owned directory, bundled in `assets/` and copied to Photon's data dir. That directory's `.zshenv`/`.zshrc` first restore the user's real `ZDOTDIR` and source the user's own files, then add hooks. This is the same technique VS Code and Ghostty use. The user's files are never modified.
  - **Hooks.** `precmd` and `preexec` hooks emit OSC 133 marks (A prompt start, B command start, C output start, D;exit-code end) and OSC 7 (cwd).
  - **Prompt.** The shell's visible prompt is suppressed while blocks are on, because the chips and input replace it.
- **Receiving marks.** Zed's pinned `alacritty_terminal`/`vte` drop unknown OSC sequences with no callback (see `vte/src/ansi.rs` `unhandled`). Photon needs them surfaced as terminal events. This could be a minimal patch, carried as a fork through `[patch]` and marked `# photon:`, that forwards unhandled OSC sequences as an event. It could also tap the byte stream. **Builder's choice**, with one constraint: no second full parse of the output stream.
- **Block model.** This lives in a new `crates/photon_blocks` crate. A block is a range of grid lines plus metadata: command, cwd, branch, start/end time, exit code. It is stored like Zed's existing `cwd_history` (scrollback line → entry) and survives scrollback trimming.
- **Rendering.** The grid is still one alacritty grid. Block chrome (header, edges, separators, selection) is drawn over and around grid line ranges in `TerminalElement`. The input area and chips are a GPUI view pinned below the grid, possibly built on the existing `block_below_cursor` mechanism.
- **Input routing.**
  - *At a prompt* (between marks A/B and C), keystrokes go to the input editor. Enter writes the text plus a newline to the PTY.
  - *While a command runs, or in alternate screen,* keystrokes go straight to the PTY exactly as today.
  - *No marks seen* (an unsupported shell, or integration failed): stay classic.
- **Git branch and changed count.** Read off the UI thread when the cwd changes or a block ends, with git CLI or Zed's existing git crate. Never polled on a timer.
- **History.** Recorded by Photon from blocks (command, cwd, exit, time) in Photon's own database (sqlez). Seeded once from `$HISTFILE` so ↑ works on day one.
- **Hooks into Zed's files** stay minimal and marked `// photon:` (SP0 fork rules).

## Tab completion (decided: ask zsh)

Once Photon owns the input box, zsh never sees Tab. The user chose to keep every completion they have today.

- **How it works.** On Tab, Photon asks a hidden helper zsh what it would complete for the current input and cwd. The helper is started lazily, once per terminal, with the same Photon `ZDOTDIR`, so the user's completion setup (compinit, plugins, `npm run`, `kubectl`, ...) loads. The technique is the one `zsh-capture-completion` and `fzf-tab` use: run `compadd` capture inside `zpty`.
- **Showing results.** Candidates (with their descriptions) are shown in a menu under the cursor, and a single match is inserted directly.
- **Speed.** The first Tab may take about 100 ms while the helper starts. Later ones should feel instant.
- **History autosuggestion (included).** A greyed suggestion from history appears as you type, and → accepts it.
- **If the helper fails or takes longer than 1 s,** fall back to file-path completion and log it. Don't block typing.
- **Memory.** One extra zsh process per terminal, only after the first Tab. This counts towards the footprint budget.

## Testing

- **Unit tests:**
  - mark parsing → block model, including nested and unfinished blocks, a non-zero exit, and scrollback trimming;
  - the input-routing state machine;
  - chip pickers (recent folders, branch list);
  - history storage.
- **A real-zsh integration test.** It spawns zsh with the Photon `ZDOTDIR`, runs `echo hi; false; cd /tmp`, and asserts three blocks with the right commands, exit codes and cwds. It also asserts that a user `.zshrc` alias still works.
- **GPUI tests:**
  - Enter writes to the PTY;
  - keystrokes go to the PTY during a running command and in alt-screen;
  - `terminal.blocks: false` gives the classic view.
- **Gates.** SP0 checks still pass: `photon-perf`, `photon-netcheck`, fmt, clippy.
- **Visual check (the user's eyes).** Photon and Warp side by side, comparing positions, spacing and colours.

## Claude's calls (from the builder review; the user may override)

- **Keys.** ↑ in the input walks history, and cmd-↑/↓ selects blocks (Warp's keys).
- **Falling back.** It is one-way and per terminal. If a terminal's marks go wrong (C without B, missing D, marks out of order) or no marks arrive within 2 s of start, that terminal switches to classic for the rest of its life and logs why. New terminals still try blocks. Known prompt frameworks (powerlevel10k instant prompt, starship, oh-my-zsh themes) must keep working: the integration emits marks itself and suppresses only the visible prompt line. If that fails, the fallback applies.
- **Which terminals.** All local zsh terminals get blocks, in the center and in the dock. `terminal.blocks` applies only to terminals opened after the change.
- **History storage.**
  - Lines starting with a space and lines matching `$HISTORY_IGNORE` are not saved.
  - History is capped at 10,000 entries (oldest dropped).
  - "terminal: clear history" in the command palette clears it.
  - ↑ and ctrl-R both search global history, newest first.
  - Entries imported from `$HISTFILE` show with no folder or exit status.

## Left to the builder

- Patch-vs-tap for OSC marks (with the constraint above).
- Exact spacing values, matched to Warp.
- Crate and module layout inside `photon_blocks`.
- Keybindings, except the ones named here.
