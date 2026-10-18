# SP1a Block Terminal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Photon's zsh terminals the Warp core: command blocks, an editor-style input pinned to the bottom, folder and git chips, zsh-backed Tab completion, history, and find, with a `terminal.blocks` escape hatch.

**Architecture:**
- **Marks.** zsh hooks, loaded through a Photon-owned `ZDOTDIR`, emit OSC 133/633/7 marks. A small patch to vendored `vte` and `alacritty_terminal` surfaces those marks as an event tagged with an *absolute* line number, which keeps working after scrollback trimming.
- **Pure logic.** `photon_blocks` (no GPUI) turns marks into a block list, and owns the shell-integration files and pure layout and parsing helpers.
- **UI.** `photon_terminal` (GPUI) owns the input box, chips, pickers, history and completion. `terminal` and `terminal_view` get minimal `// photon:` hooks that call into these crates.

**Tech Stack:** Rust 1.98.1, GPUI, alacritty_terminal + vte (vendored and patched), zsh ≥ 5.8, sqlez, git CLI, cargo-nextest.

**Spec:** `docs/photon/specs/2026-10-18-sp1a-block-terminal-design.md` (mockups in `docs/photon/specs/sp1a-mockups/`)

## Global Constraints

- **zsh only.** Other shells, remote/SSH terminals, and terminals without marks stay classic.
- **The user's dotfiles are never modified.** Photon's `ZDOTDIR` sources the user's real files first.
- **Fork rules** (`.rules`):
  - new code goes in `crates/photon_*`;
  - Zed files get only minimal hooks, each marked `// photon: <why>` (`# photon:` in TOML/zsh);
  - prefer `assets/settings/default.json` for defaults.
- **Look.**
  - Background `#050505`, accent/selected edge `#19AAD8`, failure edge `#FF8272`, metadata `#8E8E8E`.
  - Hack 13, line height 1.2.
  - Spacing matches Warp.
- **Keys.**
  - Input box: Enter runs, shift-enter adds a newline, and ↑ on the first line walks history.
  - cmd-↑/cmd-↓ select blocks, cmd-F finds, ctrl-R opens history search.
- **History.**
  - Global, newest first, capped at 10,000 entries.
  - Lines starting with a space, or matching `$HISTORY_IGNORE`, are not saved.
  - "terminal: clear history" clears it.
  - Imported `$HISTFILE` entries have no cwd or exit status.
- **Fallback.**
  - Each terminal falls back on its own, one way, and logs why.
  - It falls back on: C without B, a new A while a command runs, or no marks within 2 s of spawn.
  - `terminal.blocks` (default `true`) applies to newly opened terminals only.
- **Completion.**
  - Tab asks a lazily spawned hidden zsh per terminal.
  - If that fails or takes more than 1 s, it falls back to path completion.
  - A grey history autosuggestion appears, and → accepts it.
- **Limits (must be enforced by automated checks):**
  - idle CPU < 1% with one terminal at a prompt;
  - printing 100,000 lines takes ≤ 10% longer than with blocks off;
  - integration overhead < 1 ms per command;
  - input-box typing no slower than the editor;
  - idle footprint ≤ 200 MB and cold start ≤ 300 ms (SP0 gates);
  - `script/photon-netcheck` stays clean.
- **No timers or polling** beyond the cursor blink and the single 2 s no-marks check.
- **Rust rules:**
  - no `unwrap()` outside tests;
  - no `let _ =` on fallible calls;
  - no `mod.rs`;
  - new crates set `[lib] path = "src/<name>.rs"`;
  - use `./script/clippy`.

## Review Focus

1. **Programs that read input while running** (sudo password, `read -p`, `python`, `ssh`). Keys must go to the PTY, not the input box. Pinned by Task 4 `input_goes_to_pty_while_command_runs`.
2. **Prompt frameworks and users who already emit OSC 133** (powerlevel10k, starship, iTerm2 integration). Duplicate A/B marks must not break blocks. Out-of-order marks must fall back, not panic. Pinned by Task 2 `duplicate_prompt_marks_are_tolerated` and `out_of_order_marks_degrade`.
3. **`clear`, cmd-K, or `reset` while blocks exist.** No stale block chrome. Pinned by Task 4 `clear_resets_blocks`.
4. **Output longer than the 10,000-line scrollback.** Old blocks drop off, and the remaining ones stay on the right lines. Pinned by Task 1 `absolute_lines_survive_history_eviction` and Task 2 `trim_drops_blocks_before_oldest_line`.
5. **Commands with quotes, `;`, unicode, or several lines**, typed in the input box. They must run exactly as typed, as one command. Pinned by Task 6 `multiline_command_is_sent_as_one_paste` and Task 3 `command_text_round_trips_special_characters`.

---

## File map

| Path | Responsibility |
|---|---|
| `vendor/vte/`, `vendor/alacritty_terminal/` | Upstream copies plus a `# photon:`/`// photon:`-marked patch: forward OSC 133/633/7, track absolute lines |
| `crates/photon_blocks/src/photon_blocks.rs` | crate root, re-exports |
| `crates/photon_blocks/src/marks.rs` | `ShellMark`, `parse_shell_mark` |
| `crates/photon_blocks/src/block_list.rs` | `Block`, `BlockList`, `Phase` |
| `crates/photon_blocks/src/layout.rs` | `visible_blocks` (absolute lines → screen rows) |
| `crates/photon_blocks/src/shell_integration.rs` | install zsh files, `prepare_zsh_env` |
| `assets/shell_integration/zsh/{.zshenv,.zprofile,.zshrc,.zlogin,photon-complete.zsh}` | zsh hooks and the completion helper |
| `crates/photon_terminal/src/photon_terminal.rs` | crate root, `init(cx)` |
| `crates/photon_terminal/src/block_input.rs` | `BlockInput` view (the editor-style input and chips row) |
| `crates/photon_terminal/src/chips.rs` | `GitInfo`, folder and branch pickers |
| `crates/photon_terminal/src/history.rs` | `HistoryStore` (sqlez), ctrl-R picker, `$HISTFILE` import |
| `crates/photon_terminal/src/completion.rs` | `ZshCompleter`, autosuggestion |
| `crates/terminal/src/terminal.rs`, `alacritty.rs`, `terminal_settings.rs` | hooks: events, `BlockList` ownership, `run_command`, env |
| `crates/terminal_view/src/terminal_view.rs`, `terminal_element.rs` | hooks: routing, block chrome, actions, find scope, open-in-split |
| `crates/settings_content/src/terminal.rs`, `assets/settings/default.json` | `terminal.blocks` |
| `script/photon-perf`, `.github/workflows/photon.yml` | new limits |

---

### Task 1: Vendor and patch vte + alacritty_terminal

**Files:**
- Create: `vendor/vte/` (crates.io `vte` 0.15.0) and `vendor/alacritty_terminal/` (zed-industries/alacritty rev `4c129667ce56611becdc82de6e28218c80e2e88f`, the `alacritty_terminal` dir only), each with its LICENSE files.
- Modify: `Cargo.toml`:
  - `alacritty_terminal = { path = "vendor/alacritty_terminal" }`;
  - `[patch.crates-io] vte = { path = "vendor/vte" }`;
  - add `vendor/*` to `workspace.exclude` if cargo makes them members;
  - every change marked `# photon:`.
- Test: `vendor/alacritty_terminal/tests/photon_marks.rs`

**Interfaces:**
- Produces:
  - `vte::ansi::Handler::shell_integration(&mut self, params: &[&[u8]])` is a new default no-op method. `osc_dispatch` calls it for OSC numbers `133`, `633` and `7` instead of `unhandled`.
  - `alacritty_terminal::event::Event::ShellIntegration { params: Vec<Vec<u8>>, absolute_line: i64 }`. `params` holds the OSC fields *after* the number (e.g. `["A"]`, `["D","1"]`, `["E","git status"]`, `["file://host/path"]` for OSC 7).
  - `Term::absolute_line(&self, line: Line) -> i64` returns `lines_scrolled_into_history + line.0`. The counter never decreases, including when history is full and lines are evicted. It is reset by `Term::reset_state`/clear-history.
  - `Term`'s handler emits `ShellIntegration` with `absolute_line = self.absolute_line(self.grid.cursor.point.line)`.

- [ ] **Step 1: Vendor the two crates unchanged.** Run `cargo build -p terminal`. Expected: builds exactly as before.
- [ ] **Step 2: Write the failing tests** in `photon_marks.rs`. Use `Term::new` with a collecting `EventListener` and a `vte::ansi::Processor`:
  - `osc_133_is_forwarded_with_line`: feed `"a\r\nb\r\n\x1b]133;A\x07"`. Expect exactly one `ShellIntegration { params: vec![b"A".to_vec()], absolute_line: 2 }`.
  - `osc_7_and_633_are_forwarded`: feed `"\x1b]7;file://h/tmp\x07\x1b]633;E;ls -la\x07"`. Expect params `[b"file://h/tmp"]` and `[b"E", b"ls -la"]`.
  - `absolute_lines_survive_history_eviction`: use `scrolling_history = 100`, 10 screen lines. Feed 1,000 lines, then `\x1b]133;A\x07`. Expect `absolute_line == 1000`, and a second mark after 5 more lines is at `1005`.
  - `other_osc_still_unhandled`: OSC `1337` emits no `ShellIntegration`.
- [ ] **Step 3: Run** `cargo test --manifest-path vendor/alacritty_terminal/Cargo.toml photon_marks`. Expected: FAIL (no such variant/method).
- [ ] **Step 4: Implement** the `Handler` method, the dispatch arm, the `Event` variant, the scroll counter (increment where the grid pushes lines into history), and `absolute_line`. Map the new variant to a placeholder `_ => {}` arm in `crates/terminal/src/alacritty.rs` with `// photon: handled in Task 4`.
- [ ] **Step 5: Run** the tests again (expected PASS), then `cargo nextest run -p terminal` (expected all pass, no behaviour change).
- [ ] **Step 6: Commit** `photon: vendor vte/alacritty_terminal and surface shell-integration OSC marks`.

### Task 2: `photon_blocks` mark parsing and block model

**Files:**
- Create:
  - `crates/photon_blocks/Cargo.toml` (deps: `anyhow`, `log`, `percent-encoding`; `[lib] path = "src/photon_blocks.rs"`);
  - `src/photon_blocks.rs`, `src/marks.rs`, `src/block_list.rs`, `src/layout.rs`.
- Modify: root `Cargo.toml` members and workspace deps.
- Test: `#[cfg(test)]` modules in each file.

**Interfaces:**
- Produces (`marks.rs`):
  ```rust
  pub enum ShellMark {
      PromptStart,                                            // 133;A
      CommandStart,                                           // 133;B
      OutputStart,                                            // 133;C
      CommandEnd { exit_code: Option<i32> },                  // 133;D[;code]
      CommandText { command: String, save_to_history: bool }, // 633;E;<escaped>[;nohist]
      Cwd(PathBuf),                                           // OSC 7 file://host/path (percent-decoded)
  }
  pub fn parse_shell_mark(params: &[Vec<u8>]) -> Option<ShellMark>;
  pub fn escape_command_text(command: &str) -> String; // \ ; BEL ESC newline → \xHH, mirrors the zsh side
  ```
- Produces (`block_list.rs`):
  ```rust
  pub struct BlockId(pub u64);
  pub struct Block {
      pub id: BlockId,
      pub command: String,
      pub save_to_history: bool,
      pub cwd: Option<PathBuf>,
      pub branch: Option<String>,
      pub prompt_line: i64,
      pub output_line: Option<i64>,
      pub end_line: Option<i64>,
      pub started_at: Option<Instant>,
      pub duration: Option<Duration>,
      pub exit_code: Option<i32>,
  }
  pub enum Phase { AwaitingFirstMark, Prompt, Running, Degraded(String) }
  pub enum BlockEvent { Started(BlockId), Finished(BlockId) }
  impl BlockList {
      pub fn new(max_blocks: usize) -> Self;
      pub fn apply(&mut self, mark: ShellMark, absolute_line: i64, now: Instant) -> Option<BlockEvent>;
      pub fn phase(&self) -> &Phase;
      pub fn degrade(&mut self, reason: impl Into<String>);
      pub fn blocks(&self) -> impl DoubleEndedIterator<Item = &Block>; // oldest first; Degraded ⇒ empty
      pub fn block(&self, id: BlockId) -> Option<&Block>;
      pub fn current_cwd(&self) -> Option<&Path>;
      pub fn set_branch(&mut self, branch: Option<String>); // stamped onto blocks started afterwards
      pub fn trim_before(&mut self, oldest_retained_line: i64);
      pub fn clear(&mut self);
  }
  ```
  A block is created at C (OutputStart) using the last CommandText and the cwd. Empty Enter (A→B→A with no C) creates no block.
- Produces (`layout.rs`):
  ```rust
  pub struct VisibleBlock { pub id: BlockId, pub first_row: i32, pub last_row: i32, pub header_row: Option<i32> }
  pub fn visible_blocks(list: &BlockList, top_absolute_line: i64, screen_rows: i32) -> Vec<VisibleBlock>;
  ```
  Rows are screen-relative. `header_row` is the row the sticky header sits on: `Some(0)` when a block's first line is above the top of the screen but the block continues onto it.

- [ ] **Step 1: Write the failing tests:**
  - `parses_all_mark_kinds`, using exact inputs:
    - `["A"]`
    - `["D","130"]` → `exit_code: Some(130)`
    - `["D"]` → `None`
    - `["E","echo a\\x3bb"]` → `"echo a;b"`
    - `["E","ls","nohist"]` → `save_to_history: false`
    - `["file://host/Users/a%20b"]` → `Cwd("/Users/a b")`
  - `unknown_marks_are_none`: `["Z"]` and `[]` both give `None`.
  - `block_lifecycle`: apply `Cwd(/tmp)`@0, `A`@0, `B`@0, `E git status`@0, `C`@1, `D;0`@3, `A`@3. Expect one block with `command == "git status"`, `cwd == /tmp`, `output_line == Some(1)`, `end_line == Some(3)`, `exit_code == Some(0)`, `phase == Prompt`.
  - `empty_enter_makes_no_block`: A,B,A gives `blocks().count() == 0`.
  - `duplicate_prompt_marks_are_tolerated`: A,A,B,B,E,C,D gives one block and is not Degraded.
  - `out_of_order_marks_degrade`:
    - C without B gives `Degraded`;
    - A while `Running` gives `Degraded`;
    - after degrading, further marks are ignored and `blocks()` is empty.
  - `trim_drops_blocks_before_oldest_line`: blocks ending at lines 10 and 50, then `trim_before(20)`, leaves only the second.
  - `cap_drops_oldest`: `new(2)` with three blocks keeps the newest two.
  - `branch_is_stamped_on_new_blocks`.
  - `visible_blocks_maps_rows`: a block spanning absolute 100..=140, with `top_absolute_line = 120` and `screen_rows = 30`, gives `first_row = -20`, `last_row = 20`, `header_row = Some(0)`.
- [ ] **Step 2: Run** `cargo nextest run -p photon_blocks`. Expected: FAIL (compile errors, missing items).
- [ ] **Step 3: Implement** `marks.rs`, `block_list.rs` (`VecDeque<Block>`) and `layout.rs`.
- [ ] **Step 4: Run** `cargo nextest run -p photon_blocks`. Expected: PASS.
- [ ] **Step 5: Commit** `photon_blocks: shell marks and block model`.

### Task 3: zsh integration, env injection, `terminal.blocks` setting

**Files:**
- Create:
  - `assets/shell_integration/zsh/.zshenv`, `.zprofile`, `.zshrc`, `.zlogin` (each restores `ZDOTDIR=$PHOTON_USER_ZDOTDIR` or `$HOME`, sources the user's file of the same name if it exists, then resets `ZDOTDIR` to Photon's dir so the next file is Photon's);
  - `crates/photon_blocks/src/shell_integration.rs`;
  - test `crates/photon_blocks/tests/zsh_integration.rs`.
- Modify:
  - `crates/settings_content/src/terminal.rs` (`pub blocks: Option<bool>`);
  - `crates/terminal/src/terminal_settings.rs` (`pub blocks: bool`);
  - `assets/settings/default.json` (`"blocks": true` in `terminal`, with a comment);
  - `crates/terminal/src/terminal.rs` `TerminalBuilder::new` (hook).

**Interfaces:**
- Produces:
  ```rust
  /// Writes the bundled zsh files to `<data_dir>/shell_integration/zsh` if missing or changed.
  pub fn install_zsh_integration(data_dir: &Path, files: &[(&str, &[u8])]) -> anyhow::Result<PathBuf>;
  /// If `shell_program` is zsh: sets ZDOTDIR to the installed dir, PHOTON_USER_ZDOTDIR to the user's
  /// previous ZDOTDIR (if any) and PHOTON_BLOCKS=1, and returns true. Otherwise leaves env alone and returns false.
  pub fn prepare_zsh_env(shell_program: &str, integration_dir: &Path, env: &mut HashMap<String, String>) -> bool;
  ```
- **What `.zshrc` does** after sourcing the user's `.zshrc`, when `PHOTON_BLOCKS=1`:
  - Appends `precmd` and `preexec` functions, putting `precmd` *last* in `precmd_functions` so prompt themes run first.
  - `precmd` emits:
    - `133;D;$?` (only after a command);
    - `7;file://$HOST$PWD`;
    - `133;A`;
    - then sets `PROMPT=$'%{\e]133;B\a%}'` and `RPROMPT=''`.
  - `preexec` emits `633;E;<escaped $1>[;nohist]` and then `133;C`. `nohist` is added when `$1` starts with a space or matches `${~HISTORY_IGNORE}`.
  - Escaping matches `escape_command_text`.
- **Hook in `TerminalBuilder::new`:** when `!is_remote_terminal && TerminalSettings::get_global(cx).blocks`, call `prepare_zsh_env` with the resolved program. The terminal remembers the result as `shell_integration: bool` (consumed in Task 4).

- [ ] **Step 1: Write the failing tests** in `zsh_integration.rs`. Spawn `zsh -i` with `env_clear()`, `HOME=<tempdir>` (containing a user `.zshrc` with `alias hi='echo hi-from-alias'`), the env from `prepare_zsh_env`, and stdin piped. Scan stdout and stderr with a test helper `osc_sequences(bytes) -> Vec<Vec<Vec<u8>>>`, then parse with `parse_shell_mark`.
  - `zsh_emits_marks_for_each_command`: stdin `echo hi\nfalse\ncd /tmp\nexit\n`. Expect CommandText `echo hi`, `false` and `cd /tmp` in order; `CommandEnd` codes `0`, `1`, `0`; and a later `Cwd("/tmp")`.
  - `user_rc_still_loads`: stdin `hi\nexit\n`. Output contains `hi-from-alias`, and the user's `.zshrc` file is byte-identical afterwards.
  - `command_text_round_trips_special_characters`: `echo "a;b" 'ü' \\` round-trips exactly.
  - `leading_space_is_nohist`: `" echo secret"` gives `save_to_history: false`.
  - `non_zsh_is_untouched`: `prepare_zsh_env("/bin/bash", ...)` returns `false` and the env is unchanged.
  - Mark the file `#[cfg(target_os = "macos")]` or skip when `zsh` is not on PATH, logging why.
- [ ] **Step 2: Run** `cargo nextest run -p photon_blocks --test zsh_integration`. Expected: FAIL.
- [ ] **Step 3: Implement** the scripts, `shell_integration.rs`, the setting (default `true`), and the `TerminalBuilder::new` hook. The files come from the `assets` crate (`assets::Assets`), and `paths::data_dir()` is the install root.
- [ ] **Step 4: Run** the same command plus `cargo nextest run -p terminal -p settings`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: zsh shell integration and terminal.blocks setting`.

### Task 4: Terminal owns the block list and input routing

**Files:**
- Modify:
  - `crates/terminal/src/alacritty.rs` (map the new event);
  - `crates/terminal/src/terminal.rs`: new `TerminalBackendEvent::ShellIntegration`; a `blocks: Option<BlockList>` field; handling in `process_event`; clear/reset hooks; the 2 s check; new public API; and `Event::BlocksChanged`.
- Test: `#[cfg(test)]` in `terminal.rs`, using the existing test-support terminal and feeding bytes through the same parse path as real PTY output.

**Interfaces:**
- Consumes: Task 1 `Event::ShellIntegration`; Task 2 `BlockList`, `parse_shell_mark`, `BlockEvent`; Task 3 `shell_integration: bool`.
- Produces (on `terminal::Terminal`):
  ```rust
  pub enum InputTarget { BlockInput, Pty }
  pub fn blocks(&self) -> Option<&BlockList>;              // None when integration off
  pub fn input_target(&self) -> InputTarget;              // BlockInput iff blocks Some, phase Prompt, !ALT_SCREEN
  pub fn run_command(&mut self, command: &str);           // bracketed paste: "\x1b[200~" + command + "\x1b[201~\r"
  pub fn top_absolute_line(&self) -> i64;                  // absolute line of the first visible row
  pub fn block_text(&self, id: BlockId, part: BlockPart) -> Option<String>; // BlockPart::{Command, Output, Both}
  pub fn set_block_branch(&mut self, branch: Option<String>);
  ```
  - `Event::BlocksChanged(Option<BlockEvent>)` is emitted on every applied mark.
  - When no marks arrive within 2 s of spawn, the phase becomes `Degraded("no shell marks")` (one GPUI timer task, dropped on first mark).

- [ ] **Step 1: Write the failing tests:**
  - `blocks_follow_shell_marks`: feed the Task 2 lifecycle bytes. `blocks()` has one block, `block_text(Command) == "git status"`, and `block_text(Output)` equals the output lines.
  - `input_goes_to_pty_while_command_runs`: after `133;C`, `input_target() == Pty`. After `133;D;0` + `133;A` + `133;B`, it is `BlockInput`.
  - `alt_screen_routes_to_pty`: `\x1b[?1049h` at a prompt gives `Pty`.
  - `clear_resets_blocks`: `Clear` (cmd-K) and `\x1bc` both leave `blocks()` empty.
  - `run_command_uses_bracketed_paste`: `pty_write_log` ends with `b"\x1b[200~ls -la\x1b[201~\r"`.
  - `no_marks_degrades_after_two_seconds`: advance the clock 2 s; the phase is `Degraded`; `input_target() == Pty`.
  - `integration_off_means_no_blocks`.
- [ ] **Step 2: Run** `cargo nextest run -p terminal -E 'test(blocks) | test(input_goes) | test(alt_screen_routes) | test(clear_resets) | test(run_command_uses) | test(no_marks) | test(integration_off)'`. Expected: FAIL.
- [ ] **Step 3: Implement.** Trim on each Wakeup when history is full: `trim_before(top_absolute_line - history_size)`.
- [ ] **Step 4: Run** the same command and then `cargo nextest run -p terminal`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: terminal tracks command blocks and input routing`.

### Task 5: Block chrome, selection and block actions

**Files:**
- Modify:
  - `crates/terminal_view/src/terminal_element.rs`: hook in `prepaint`/`paint` that paints block chrome from `visible_blocks`, and hides prompt rows (`prompt_line..output_line`) while blocks are on;
  - `crates/terminal_view/src/terminal_view.rs`: selected block state, actions, click-to-select.
- Test: `#[cfg(test)]` in `terminal_view.rs`.

**Interfaces:**
- Consumes: Task 2 `visible_blocks`, `VisibleBlock`; Task 4 `blocks`, `block_text`, `run_command`, `top_absolute_line`.
- Produces:
  - Actions in namespace `photon_terminal`:
    - `SelectPreviousBlock` (cmd-up), `SelectNextBlock` (cmd-down);
    - `CopyBlockCommand`, `CopyBlockOutput`, `CopyBlock`;
    - `RerunBlock`.
  - `TerminalView::selected_block(&self) -> Option<BlockId>`.
- **Chrome:**
  - Each block gets a header row: command in bold on the left; on the right, cwd, branch and duration (`0.4s`, `12.8s`, `4m 12s`) in metadata colour.
  - A 3 px left edge: `#FF8272` if `exit_code != Some(0)`, `#19AAD8` if selected, none otherwise.
  - A 1 px separator `#1a1a1a` between blocks, and a selected-block background tint of `#19AAD8` at 8% alpha.
  - The header is sticky when `header_row == Some(0)`.

- [ ] **Step 1: Write the failing tests:**
  - `cmd_up_selects_newest_then_older`.
  - `copy_block_actions_fill_clipboard`: command, output and both, using `cx.read_from_clipboard()`.
  - `rerun_writes_command`: `pty_write_log` ends with the bracketed command.
  - `click_selects_block`: simulated mouse-down on a row inside a block.
  - `selection_clears_when_block_trimmed`.
- [ ] **Step 2: Run** `cargo nextest run -p terminal_view -E 'test(block)'`. Expected: FAIL.
- [ ] **Step 3: Implement.** Bind the keys in `assets/keymaps/default-macos.json` (context `Terminal`), marked with a `// photon:` comment.
- [ ] **Step 4: Run** the same command and then `cargo nextest run -p terminal_view`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: block chrome, selection and block actions`.

### Task 6: Editor-style input box

**Files:**
- Create:
  - `crates/photon_terminal/Cargo.toml` (deps: `gpui`, `ui`, `editor`, `terminal`, `photon_blocks`, `picker`, `settings`, `db`, `util`, `anyhow`, `log`);
  - `src/photon_terminal.rs`, `src/block_input.rs`.
- Modify: `crates/terminal_view/src/terminal_view.rs`. The view owns `Option<Entity<BlockInput>>`, renders it pinned below the grid (consider the `block_below_cursor` slot), and gives it focus while `input_target() == BlockInput`, otherwise focuses the grid as today. Also `crates/zed/src/main.rs` or `zed.rs`: `photon_terminal::init(cx)`.

**Interfaces:**
- Consumes: Task 4 `input_target`, `run_command`; `Event::BlocksChanged`.
- Produces:
  ```rust
  pub struct BlockInput { editor: Entity<Editor>, terminal: WeakEntity<Terminal>, history_cursor: Option<usize>, /* … */ }
  impl BlockInput {
      pub fn new(terminal: Entity<Terminal>, window: &mut Window, cx: &mut Context<Self>) -> Self;
      pub fn text(&self, cx: &App) -> String;
      pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>);
  }
  ```
  - Actions: `photon_terminal::{RunInput, HistoryPrevious, HistoryNext}`. Enter → `RunInput` (send the text, clear it, reset `history_cursor`); shift-enter → newline; ↑ on the first line → `HistoryPrevious`; ↓ on the last line → `HistoryNext`.
  - History comes from a `fn history_source(&self, cx) -> Vec<String>` stub returning the commands of the session's blocks, newest first. Task 7 replaces this with `HistoryStore`.
  - The editor is multi-line and auto-height, up to 10 lines; it uses the Hack 13 buffer font and has no gutter. A `❯` glyph in metadata colour sits to its left.

- [ ] **Step 1: Write the failing tests:**
  - `enter_runs_and_clears`.
  - `multiline_command_is_sent_as_one_paste`: input `"for f in a b\ndo echo $f\ndone"` gives a single bracketed paste and one `\r`.
  - `shift_enter_inserts_newline`.
  - `up_on_first_line_walks_history`: after running `ls` and then `pwd`, ↑ gives `pwd`, ↑ gives `ls`, ↓ gives `pwd`.
  - `input_hidden_while_running`: after `133;C`, the view focuses the grid and keystrokes reach `pty_write_log`.
- [ ] **Step 2: Run** `cargo nextest run -p photon_terminal -p terminal_view -E 'test(input) | test(enter_runs) | test(multiline) | test(shift_enter) | test(up_on_first)'`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `photon: editor-style input box for block terminals`.

### Task 7: History store, `$HISTFILE` import, ctrl-R

**Files:**
- Create: `crates/photon_terminal/src/history.rs`.
- Modify: `block_input.rs` (swap the stub for `HistoryStore`), `photon_terminal.rs` (register the clear action and ctrl-R).

**Interfaces:**
- Consumes: Task 4 `Event::BlocksChanged(Some(BlockEvent::Finished(id)))`; `Block::save_to_history`.
- Produces:
  ```rust
  pub struct HistoryEntry { pub command: String, pub cwd: Option<PathBuf>, pub exit_code: Option<i32>, pub ran_at: Option<SystemTime> }
  impl HistoryStore {                                   // global; sqlez domain "photon_terminal_history"
      pub fn global(cx: &App) -> Entity<HistoryStore>;
      pub fn record(&mut self, entry: HistoryEntry, cx: &mut Context<Self>);   // caps at 10_000
      pub fn recent(&self, limit: usize) -> Vec<HistoryEntry>;                 // newest first, deduped by command
      pub fn search(&self, query: &str, limit: usize) -> Vec<HistoryEntry>;    // fuzzy
      pub fn recent_dirs(&self, limit: usize) -> Vec<PathBuf>;                 // distinct cwd, newest first
      pub fn clear(&mut self, cx: &mut Context<Self>);
  }
  pub fn parse_zsh_histfile(contents: &str) -> Vec<String>; // plain and `: <ts>:<d>;cmd` extended format, multi-line `\` continuations
  ```
  - Actions: `photon_terminal::{ClearHistory, SearchHistory}` (ctrl-R). The picker confirms by putting the command into `BlockInput`, not running it.
  - `$HISTFILE` (or `~/.zsh_history`) is imported once, the first time the store is empty. The import is recorded in the kvp store so it never repeats.

- [ ] **Step 1: Write the failing tests:**
  - `parse_zsh_histfile_handles_extended_and_continuations`.
  - `record_skips_nohist`.
  - `cap_is_10000`.
  - `recent_is_newest_first_and_deduped`.
  - `clear_empties`.
  - `import_runs_once`.
  - `ctrl_r_fills_input_without_running`.
- [ ] **Step 2: Run** `cargo nextest run -p photon_terminal -E 'test(history) | test(histfile) | test(record) | test(cap_is) | test(recent) | test(clear) | test(import) | test(ctrl_r)'`. Expected: FAIL.
- [ ] **Step 3: Implement.** sqlez writes go to the background executor.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `photon: terminal command history`.

### Task 8: Folder and git branch chips

**Files:**
- Create: `crates/photon_terminal/src/chips.rs`.
- Modify: `block_input.rs` (chips row above the editor).

**Interfaces:**
- Consumes: Task 4 `BlockList::current_cwd`, `set_block_branch`, `run_command`; Task 7 `recent_dirs`.
- Produces:
  ```rust
  pub struct GitInfo { pub branch: String, pub changed: usize, pub branches: Vec<String> }
  pub fn git_info(cwd: &Path, cx: &App) -> Task<Option<GitInfo>>; // background; `git -C cwd` symbolic-ref/status --porcelain/branch --format
  pub fn cd_command(path: &Path) -> String;                          // "cd -- <shell-quoted>"
  pub fn switch_command(branch: &str, create: bool) -> String;       // "git switch [-c] -- <quoted>"
  ```
  - `git_info` is refreshed only on `BlocksChanged(Finished)` and on cwd change. There is no timer.
  - Chip text:
    - folder: `📁 <~-abbreviated cwd> ▾`;
    - branch: `⎇ <branch> ▾` plus `± <n> changed` when `n > 0`;
    - the branch chip is hidden outside a repo.
  - Pickers:
    - folder: recent dirs, fuzzy, plus "Open folder in Photon" (dispatches `workspace::Open` for the path);
    - branch: local branches with the current one highlighted, plus "New branch…" (the query becomes the name).

- [ ] **Step 1: Write the failing tests:**
  - `cd_command_quotes_spaces_and_quotes`: `/tmp/it's here` → `cd -- '/tmp/it'\''s here'`.
  - `switch_command_variants`.
  - `git_info_reads_temp_repo`: `git init`, commit, branch `feature`, one modified file gives `branch == "main"` or the configured default, `changed == 1`, and `branches` containing `feature`.
  - `git_info_none_outside_repo`.
  - `branch_pick_runs_switch`.
  - `folder_pick_runs_cd`.
- [ ] **Step 2: Run** `cargo nextest run -p photon_terminal -E 'test(chip) | test(cd_command) | test(switch_command) | test(git_info) | test(pick)'`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `photon: folder and git branch chips`.

### Task 9: Tab completion via hidden zsh, history autosuggestion

**Files:**
- Create:
  - `crates/photon_terminal/src/completion.rs`;
  - `assets/shell_integration/zsh/photon-complete.zsh`, a zpty + `compadd`-capture helper. It reads one request line `<cwd>\t<escaped buffer>` and prints `<insert>\t<description>` lines followed by an empty line.
- Modify: `block_input.rs` (Tab, completion menu, grey suggestion, → accept).

**Interfaces:**
- Consumes: Task 3 integration dir; Task 7 `HistoryStore::recent`.
- Produces:
  ```rust
  pub struct Completion { pub insert: String, pub description: Option<String> }
  pub struct ZshCompleter { /* lazily spawned `zsh -i` with Photon ZDOTDIR and PHOTON_COMPLETER=1 */ }
  impl ZshCompleter {
      pub fn complete(&mut self, buffer: &str, cwd: &Path, cx: &App) -> Task<anyhow::Result<Vec<Completion>>>;
  }
  pub fn path_completions(buffer: &str, cwd: &Path) -> Vec<Completion>; // fallback
  pub fn history_suggestion(buffer: &str, recent: &[HistoryEntry]) -> Option<String>; // suffix to show in grey
  ```
  - When `PHOTON_COMPLETER=1`, `.zshrc` skips the prompt hooks and sources `photon-complete.zsh`.
  - A timeout over 1 s, or a dead helper, means `path_completions` is used and the failure is logged.
  - One match is inserted directly; several open a menu (insert in Hack, description in metadata colour).

- [ ] **Step 1: Write the failing tests:**
  - `zsh_completes_git_subcommand`: `"git che"` includes `checkout`. Skip if git or zsh is missing.
  - `zsh_completes_files`: in a temp dir containing `alpha.txt`, `"cat al"` gives `alpha.txt`.
  - `slow_helper_falls_back_to_paths`, using a fake helper command that sleeps 2 s.
  - `history_suggestion_is_newest_prefix_match`.
  - `right_arrow_accepts_suggestion`.
- [ ] **Step 2: Run** `cargo nextest run -p photon_terminal -E 'test(complet) | test(suggestion) | test(right_arrow)'`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `photon: zsh-backed tab completion and history suggestions`.

### Task 10: Find within a block

**Files:**
- Modify: `crates/terminal_view/src/terminal_view.rs`. The existing search (`SearchableItem` impl) limits matches to the selected block's line range when a block is selected; otherwise all lines, as today.

**Interfaces:**
- Consumes: Task 5 `selected_block`; Task 4 `blocks`.
- Produces: no new public API.

- [ ] **Step 1: Write the failing tests:** `find_is_scoped_to_selected_block` (the word `needle` in two blocks, the second selected: one match) and `find_without_selection_searches_all`.
- [ ] **Step 2: Run** `cargo nextest run -p terminal_view -E 'test(find_)'`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `photon: find within the selected block`.

### Task 11: Open `path:line` in a split; toggle terminal/editor focus

**Files:**
- Modify:
  - `crates/terminal_view/src/terminal_view.rs` / `terminal_path_like_target.rs` (hook): when the terminal is a center item, open the target in the pane to the right, splitting if there is none, instead of replacing the terminal.
  - `crates/zed/src/zed/photon.rs`: the `photon::ToggleTerminalEditorFocus` action and handler.
- Test: `terminal_view` and `zed` test modules.

**Interfaces:**
- Produces: `photon::ToggleTerminalEditorFocus`. It focuses the most recently active editor item, or the center terminal when an editor is focused. Pick a free macOS binding: check `assets/keymaps/default-macos.json` for conflicts and record the choice in the commit message.

- [ ] **Step 1: Write the failing tests:** `path_click_opens_beside_center_terminal` (the terminal stays the active item of its pane; the file opens in a new right pane) and `toggle_moves_focus_between_terminal_and_editor`.
- [ ] **Step 2: Run** `cargo nextest run -p terminal_view -p zed -E 'test(opens_beside) | test(toggle_moves_focus)'`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `photon: open terminal paths beside the terminal; terminal/editor focus toggle`.

### Task 12: Escape hatch, performance limits, CI

**Files:**
- Modify:
  - `script/photon-perf`: add `idle_cpu_percent`. With one terminal at a prompt, after 5 s of settling, sum the CPU time of the whole process tree over 10 s; fail if it is ≥ 1%. Add the field to `tooling/photon-perf/baseline.json`.
  - `.github/workflows/photon.yml`: add `-p photon_blocks -p photon_terminal -p terminal -p terminal_view` to clippy and tests; add `package(photon_blocks) | package(photon_terminal)` to the filter.
- Create, as tests:
  - `crates/terminal/benches` or a `#[test] #[ignore]`-free timing test, `heavy_output_overhead_within_10_percent`. Feed 100,000 lines through `Term` with and without marks plus `BlockList`, and assert `with ≤ without × 1.10` (median of 5 runs).
  - `per_command_overhead_under_1ms` (photon_blocks): apply 10,000 full mark cycles and assert the mean is under 1 ms. Also, in the zsh integration test, run 200 `true` commands with and without `PHOTON_BLOCKS` and assert the mean difference is under 1 ms.
  - `typing_latency_matches_editor` (photon_terminal): dispatch 1,000 keystrokes into `BlockInput` and into a plain `Editor`, and assert input ≤ editor × 1.10.
  - `blocks_setting_off_gives_classic_terminal` (terminal): `terminal.blocks: false` gives `blocks() == None`, no `ZDOTDIR` change, and `input_target() == Pty`.

- [ ] **Step 1: Write the four tests and the perf-script check.** Run them. Expected: they fail if their code doesn't exist yet. If a test passes immediately because Tasks 1–11 already met the limit, note "already within limit" in the commit message. That is the expected result for a guard.
- [ ] **Step 2: Fix any limit that fails**, starting with the profile (`cargo build --release` plus Instruments/`sample`).
- [ ] **Step 3: Run the full gate:**
  - `cargo nextest run -p photon_blocks -p photon_terminal -p terminal -p terminal_view -p zed -p settings`;
  - `./script/clippy -p photon_blocks -p photon_terminal -p terminal -p terminal_view -p zed`;
  - `cargo fmt --all -- --check`;
  - `cargo build --release -p zed -p cli`;
  - `script/photon-netcheck`;
  - `script/photon-perf`.

  Expected: all pass; perf within budgets, including idle CPU < 1%.
- [ ] **Step 4: Commit** `photon: SP1a performance limits and CI`.
- [ ] **Step 5: Hand the user the manual checks:**
  - Photon and Warp side by side (positions, spacing, colours);
  - `vim`, `htop`, `less`, `ssh`, `sudo -v`, and `python` REPL behaviour;
  - their powerlevel10k/starship/oh-my-zsh prompt still works (or falls back cleanly).
