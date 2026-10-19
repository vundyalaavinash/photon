# SP1b Smart Blocks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Command blocks that recognise test runs, build errors, git status/log, dev servers, JSON and tables, and offer one-click actions on them.

**Architecture:**
- Pure recognisers in `photon_blocks::smart` turn a finished block's output lines into an `Insight`.
- A per-terminal `SmartBlocks` entity in `photon_terminal` computes and stores insights off the UI thread, runs git and opener actions, scans running blocks for URLs, and posts notifications.
- `terminal_view` paints a summary chip in the block header and overlays a GPUI toolbar (and git row buttons) on the selected block.

**Tech Stack:** Rust, GPUI, the SP1a block terminal, Zed's `git_ui` / `tabular_data_preview` / `editor`, and `serde_json` + `regex` (both already workspace deps).

**Spec:** `docs/photon/specs/2026-10-19-sp1b-smart-blocks-design.md` (SP1a spec `docs/photon/specs/2026-10-18-sp1a-block-terminal-design.md` still binds)

## Global Constraints

- SP0 fork rules (CLAUDE.md):
  - new code goes in `crates/photon_*`;
  - Zed files get minimal hooks marked `// photon: <why>` (`# photon:` in TOML);
  - defaults go in `assets/settings/default.json`.
- Raw terminal output is never hidden, moved or rewritten. Insights only add a chip, buttons and actions.
- Nothing runs without an explicit click or keypress. `discard` always asks to confirm first.
- Recognisers run once per finished block, off the UI thread. If the output is over **20,000 lines or 5 MB**, there is no insight.
- The running-block URL scan stops after the first URL or **2,000** output lines. The error-line scan runs only while the window is inactive.
- SP1a limits, checked by `script/photon-perf`:
  - idle CPU **< 1%**;
  - 100k lines **≤ 10%** slower than `terminal.blocks: false`;
  - **< 1 ms** per command;
  - footprint **≤ 200 MB**;
  - cold start **≤ 300 ms**;
  - `script/photon-netcheck` stays clean.
- Settings: `terminal.smart_blocks` (default `true`, off = SP1a blocks exactly) and `terminal.notify_after_seconds` (default `10`, `0` = no notifications).
- Colours (Photon Black): failure `#FF8272`, success `#B4FA72`, accent `#19AAD8`, metadata `#8E8E8E`.
- No agent features (SP3).
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Run clippy with `./script/clippy`.

## Review Focus

1. **Output that wrapped across grid rows.** Insight lines are logical lines that start at an absolute grid line, so git row buttons and hash cmd-click land on the right row of a narrow window. Tests: Task 4 `output_lines_join_wrapped_rows`, Task 6 `row_buttons_follow_wrapped_lines`.
2. **A block trimmed from scrollback or cleared (cmd-K) while its insight is computing.** The late result is dropped, not attached to a missing or reused block. Test: Task 4 `insight_for_cleared_block_is_dropped`.
3. **Commands with shell operators, quotes or unknown wrappers.** "Re-run failed" is hidden rather than building a command that runs something else. Test: Task 2 `rerun_refuses_unsafe_commands`.
4. **File paths with spaces, unicode or renames (`a -> b`) in git status.** stage, diff and discard act on the right path, quoted. Test: Task 3 `git_status_paths_with_spaces_and_renames`, Task 7 `stage_quotes_path`.
5. **Prose that happens to look aligned, or near-miss JSON.** No table or JSON insight on ordinary output. Test: Task 3 `prose_is_not_a_table`, `json_near_misses`.

---

### Task 1: Insight types and test-runner recognisers

**Files:**
- Create: `crates/photon_blocks/src/smart.rs`
- Create: `crates/photon_blocks/test_fixtures/smart/` (one `.txt` per fixture)
- Modify: `crates/photon_blocks/src/photon_blocks.rs` (add `mod smart; pub use smart::*;`)
- Modify: `crates/photon_blocks/Cargo.toml` (add `regex.workspace = true`, `serde_json.workspace = true`)

**Interfaces:**
- Produces:
```rust
pub struct OutputLine { pub absolute_line: i64, pub text: String }
pub struct SourceLocation { pub path: PathBuf, pub line: u32, pub column: Option<u32> }
pub enum TestTool { Cargo, Nextest, Pytest, Jest, Go }
pub struct TestFailure { pub name: String, pub location: Option<SourceLocation> }
pub struct TestRun { pub tool: TestTool, pub passed: usize, pub failed: usize, pub failures: Vec<TestFailure> }
pub enum FileState { Staged, Unstaged, Untracked }
pub struct StatusFile { pub absolute_line: i64, pub path: String, pub state: FileState }
pub struct CommitHash { pub absolute_line: i64, pub columns: Range<usize>, pub hash: String }
pub struct Table { pub headers: Vec<String>, pub rows: Vec<Vec<String>> }
pub enum Insight {
    Tests(TestRun),
    Build { errors: Vec<SourceLocation> },
    GitStatus { files: Vec<StatusFile> },
    GitLog { commits: Vec<CommitHash> },
    Json,
    Table(Table),
}
pub const MAX_INSIGHT_LINES: usize = 20_000;
pub const MAX_INSIGHT_BYTES: usize = 5 * 1024 * 1024;
pub fn recognise(command: &str, lines: &[OutputLine], exit_code: Option<i32>) -> Option<Insight>;
```
  All of these derive `Clone, Debug, PartialEq` (plus `Eq` where possible).
- **Precedence in `recognise`:** Tests > Build > GitStatus > GitLog > Json > Table. This task implements Tests only. Later tasks add the other arms, so for now `recognise` returns `None` for everything else.
- **Test recognition goes by output format, not command:**
  - **Cargo:** `test result: (ok|FAILED). N passed; M failed`. Sum the counts over every result line. Failures come from `---- <name> stdout ----` lines. The location comes from the `panicked at <path>:<line>:<col>` line that follows.
  - **Nextest:** the `Summary [...] N tests run: P passed, F failed` line. Failures come from `FAIL [...] <binary> <name>` lines. The name is the last field.
  - **Pytest:** a closing line like `=+ F failed, P passed in ...` (either order; `passed`-only counts too). Failures come from `FAILED <nodeid> - ...` lines. The location is the nodeid's file plus the `path:line:` line in its traceback when present.
  - **Jest/Vitest:** `Tests: F failed, P passed, T total` (jest) or ` Tests  F failed | P passed` (vitest). Failures come from `● <suite> › <name>` (jest) or `FAIL <file> > <suite> > <name>` (vitest).
  - **Go:** `--- FAIL: <Name> (` lines are failures, `--- PASS:` lines are passes, and `FAIL\t<pkg>` / `ok\t<pkg>` mark packages. The location is the `<file>.go:<line>:` line following a failure.

- [ ] **Step 1: Capture fixtures.** For each tool installed on this machine, capture a passing and a failing run (`cargo test`/`cargo nextest` on a scratch crate; `pytest`, `jest`/`vitest`, `go test` where available). For tools not installed, write the fixture by hand from the tool's documented output format and note that in the fixture's first line as `# hand-written`. Strip that comment line when loading.
- [ ] **Step 2: Write failing tests** in `smart.rs` `#[cfg(test)] mod tests`. Use a helper `lines(text) -> Vec<OutputLine>` numbering lines from 0.
```rust
#[test] fn cargo_failures_counted_and_located() // 2 failed, 41 passed; failure names; location crates/cli/src/main.rs:356
#[test] fn cargo_passing_run()                  // Tests(TestRun{failed: 0, ..}), passed > 0
#[test] fn nextest_summary()                     // counts from Summary, names from FAIL lines
#[test] fn pytest_failures()                     // nodeids as names, counts from the ===== line
#[test] fn jest_and_vitest_failures()
#[test] fn go_test_failures()                    // subtests count as failures by their top-level Name only once
#[test] fn plain_output_has_no_insight()         // "hello\nworld" -> None
```
- [ ] **Step 3: Run** `cargo nextest run -p photon_blocks smart`. Expected: FAIL (does not compile / assertions).
- [ ] **Step 4: Implement** the types and the test recognisers. Compile each regex once with `std::sync::LazyLock`.
- [ ] **Step 5: Run** `cargo nextest run -p photon_blocks`. Expected: all PASS.
- [ ] **Step 6: Commit** `photon: recognise test runs in block output`.

### Task 2: Re-run-failed command builder and run comparison

**Files:**
- Modify: `crates/photon_blocks/src/smart.rs`

**Interfaces:**
- Consumes: `TestRun`, `TestTool` (Task 1).
- Produces:
```rust
pub fn rerun_failed_command(command: &str, run: &TestRun) -> Option<String>;
pub struct RunDelta { pub new_failures: Vec<String>, pub fixed: Vec<String> }
pub fn compare_runs(previous: &TestRun, current: &TestRun) -> RunDelta;
```
- **Rules.** Names are single-quoted with `'` → `'\''`.
  - Return `None` when:
    - `run.failed == 0` or there are no failure names;
    - the command contains any of `| ; & < > $ \` ( ) \n`;
    - none of the rules below apply.
  - **Cargo:** the command must contain the tokens `cargo test`. Keep everything before a ` -- ` if present. Keep flags (`-…`) after it and drop positional filters. Result: `<before> -- <names…> --exact <kept flags>`.
  - **Nextest:** the command must contain `cargo nextest run` and must not contain `-E`/`--filterset`. Result: `<command up to any " -- "> -E '<expr>'`, where expr = `test(=a) | test(=b)`.
  - **Pytest:** a `pytest` token must exist. Keep the tokens up to and including the `pytest` token, plus every later token starting with `-`. Then append the nodeids.
  - **Jest/Vitest:** `-t '<regex>'`, where the regex is the failure names joined with `|`. Each name has its regex metacharacters escaped and ` › ` / ` > ` replaced with a space. Append it directly if a `jest`/`vitest` token is present. If the command starts with `npm`, `pnpm`, `yarn` or `bun`, append ` -- -t '…'`.
  - **Go:** the command must start with `go test`. Result: `go test <original flags> -run '^(A|B)$' <failed packages>`, using top-level test names only.
- **`compare_runs`:**
  - `new_failures` = names failed now but not before;
  - `fixed` = names failed before, not now, and the current run has no failure with that name;
  - both are sorted.

- [ ] **Step 1: Write failing tests:**
```rust
#[test] fn rerun_cargo()     // "cargo test -p cli" -> "cargo test -p cli -- 'tests::a' 'tests::b' --exact"
#[test] fn rerun_cargo_keeps_flags_after_dashes() // "cargo test -- --nocapture foo" -> "cargo test -- 'a' --exact --nocapture"
#[test] fn rerun_nextest()   // "cargo nextest run -p x" -> "cargo nextest run -p x -E 'test(=a) | test(=b)'"
#[test] fn rerun_pytest()    // "python -m pytest -x tests/" -> "python -m pytest -x 'tests/t.py::test_a'"
#[test] fn rerun_jest_via_npm() // "npm test" -> "npm test -- -t 'suite name'"
#[test] fn rerun_go()        // "go test ./..." -> "go test -run '^(TestA)$' 'example.com/pkg'"
#[test] fn rerun_refuses_unsafe_commands() // "cargo test | tee log", "make test", "cargo test; echo", passing run -> None
#[test] fn compare_runs_reports_new_and_fixed()
```
- [ ] **Step 2: Run** `cargo nextest run -p photon_blocks rerun compare`. Expected: FAIL.
- [ ] **Step 3: Implement.** Split on whitespace. The rejection list above guarantees the command has no shell syntax beyond quoting, and a command containing quotes returns `None` too.
- [ ] **Step 4: Run** `cargo nextest run -p photon_blocks`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: build re-run-failed commands and compare test runs`.

### Task 3: Build, git, JSON, table, URL and error-line recognisers

**Files:**
- Modify: `crates/photon_blocks/src/smart.rs`, plus fixtures in `crates/photon_blocks/test_fixtures/smart/`

**Interfaces:**
- Consumes: Task 1 types.
- Produces (in addition to the new `recognise` arms):
```rust
pub fn find_local_url(line: &str) -> Option<String>; // "http://localhost:5173/" ; 0.0.0.0 and [::] rewritten to localhost
pub fn url_label(url: &str) -> String;               // "localhost:5173"
pub fn is_error_line(line: &str) -> bool;
pub fn table_to_csv(table: &Table) -> String;        // RFC 4180 quoting, "\n" line ends
```
- **Build:** applies when there are no test results.
  - Error lines:
    - rustc: a line starting `error` (not `warning`) and its following `--> path:line:col`;
    - tsc: `path(line,col): error TS`;
    - gcc/clang: `path:line:col: error:`.
  - `Build { errors }` comes back only when at least one error is found.
- **GitStatus:**
  - The command's first two words must be `git status`.
  - Long format sections: "Changes to be committed" gives `Staged`, "Changes not staged" gives `Unstaged`, "Untracked files" gives `Untracked`.
  - Short format `XY path`: X≠' '/'?' gives Staged; Y≠' ' gives Unstaged (a file may give both rows); `??` gives Untracked. `## ` lines are skipped.
  - A rename `old -> new` keeps `new`. Strip git's quoting of paths (`"a b"`, `\303\251`) back to the real path.
- **GitLog:** the command's first two words must be `git log`. Collect the 7–40 hex char token after `commit ` at line start, or the first token of a line when it is 7–40 hex chars (`--oneline`), with its column range.
- **Json:** the trimmed text starts with `{` or `[` and `serde_json::from_str::<serde_json::Value>` succeeds.
- **Table:**
  - Needs ≥3 lines (header + ≥2 rows). A column boundary is a char index (≥1) that is a space on every line, or past the end of a shorter line, and has a non-space just after it on the header line.
  - Gives ≥2 columns, every header cell non-empty, and no header cell purely numeric. The last column takes the rest of the line.
- **find_local_url:** the first `https?://(localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1?\])(:\d+)?(/\S*)?` in the line.
- **is_error_line:** matches `(?i)\b(error|failed|exception|panic(ked)?)\b`, but not when the line matches `(?i)\b(0|no) (errors?|failures?)\b`.

- [ ] **Step 1: Write failing tests:**
```rust
#[test] fn rustc_errors_located()            // 2 errors, path/line/col from --> lines; warnings ignored
#[test] fn tsc_and_gcc_errors_located()
#[test] fn successful_build_has_no_insight()
#[test] fn git_status_long_format()          // Staged/Unstaged/Untracked rows with their absolute lines
#[test] fn git_status_short_format()
#[test] fn git_status_paths_with_spaces_and_renames() // "a b.txt", "old -> new.txt" -> "new.txt", quoted unicode
#[test] fn git_log_hashes_with_columns()     // default and --oneline
#[test] fn json_detected()
#[test] fn json_near_misses()                // "{not json", "[1,2" , "ok {" -> no Json
#[test] fn kubectl_docker_ps_tables()        // headers and row cells; last column with spaces (ps aux COMMAND)
#[test] fn prose_is_not_a_table()            // a paragraph of 3 lines -> None
#[test] fn local_urls()                       // vite "➜  Local:   http://localhost:5173/", "0.0.0.0:3000" -> localhost
#[test] fn error_lines()                      // "error: x" true, "0 errors" false, "Compiled with no errors" false
#[test] fn csv_quoting()                      // cells with comma, quote, newline
```
- [ ] **Step 2: Run** `cargo nextest run -p photon_blocks smart`. Expected: FAIL.
- [ ] **Step 3: Implement** the arms in precedence order.
- [ ] **Step 4: Run** `cargo nextest run -p photon_blocks`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: recognise builds, git, JSON, tables and local URLs`.

### Task 4: SmartBlocks entity, output lines and settings

**Files:**
- Create: `crates/photon_terminal/src/smart_blocks.rs`
- Modify:
  - `crates/photon_terminal/src/photon_terminal.rs` (mod + `pub use smart_blocks::SmartBlocks;`)
  - `crates/terminal/src/photon.rs` (add `block_output_lines`)
  - `crates/settings_content/src/terminal.rs`, `crates/terminal/src/terminal_settings.rs`, `assets/settings/default.json` (two settings, `// photon:` hooks)
  - `crates/terminal_view/src/terminal_view.rs` (create and own `smart_blocks: Option<Entity<photon_terminal::SmartBlocks>>` next to `block_input`, `// photon:` hook)

**Interfaces:**
- Consumes: `recognise`, `OutputLine`, `MAX_INSIGHT_*`, `compare_runs`, `find_local_url`, `is_error_line` (Tasks 1–3). SP1a: `Event::BlocksChanged(Some(BlockEvent::Finished(id)))`, `Event::Wakeup`, `Terminal::blocks()`, `Block { id, command, cwd, output_line, end_line, exit_code, duration }`.
- Produces:
```rust
// crates/terminal/src/photon.rs
impl Terminal {
    /// Logical lines of the block's output (wrapped rows joined), each with the absolute line it
    /// starts on. Stops after `max_lines` lines or `max_bytes` bytes and returns None when
    /// the output is larger than either.
    pub fn block_output_lines(&self, id: BlockId, max_lines: usize, max_bytes: usize) -> Option<Vec<photon_blocks::OutputLine>>;
}
// crates/photon_terminal/src/smart_blocks.rs
pub struct SmartBlocks { /* terminal, workspace, insights: HashMap<BlockId, Insight>, deltas, urls, scans, tasks */ }
impl SmartBlocks {
    pub fn new(terminal: Entity<Terminal>, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self;
    pub fn insight(&self, id: BlockId) -> Option<&Insight>;
    pub fn delta(&self, id: BlockId) -> Option<&RunDelta>;
    pub fn local_url(&self, id: BlockId) -> Option<&str>;
}
pub enum SmartBlocksEvent { Changed }   // impl EventEmitter; TerminalView notifies on it
```
- **Behaviour:**
  - On `Finished(id)`, when `smart_blocks` is on, read `block_output_lines(id, MAX_INSIGHT_LINES, MAX_INSIGHT_BYTES)` on the UI thread and run `recognise` in `cx.background_spawn`.
  - On completion, store the result only if `terminal.blocks()?.block(id)` still exists. Then emit `Changed`.
  - For `Tests`, find the newest earlier finished block with the same command and cwd that has a `Tests` insight, and store `compare_runs`.
  - Drop insights of blocks no longer in the list on every `BlocksChanged`.
  - On `Wakeup` with a running block that has no URL yet and fewer than 2,000 scanned lines: read only lines past the last scanned absolute line, run `find_local_url`, and emit on the first hit.
  - Nothing is created when the terminal has no blocks.

- [ ] **Step 1: Write failing tests:**
  - in `crates/terminal/src/terminal.rs`'s photon test module: `output_lines_join_wrapped_rows` (a 3-column-wide grid; a line longer than the width comes back as one `OutputLine` with the first row's absolute line) and `output_lines_over_cap_is_none`;
  - in `smart_blocks.rs` (GPUI tests feeding marks like SP1a's `feed_block_output`):
    - `finished_test_block_gets_insight`
    - `insight_for_cleared_block_is_dropped`
    - `second_run_gets_delta`
    - `running_block_url_found`
    - `url_scan_stops_after_limit`
    - `smart_blocks_setting_off_records_nothing`
- [ ] **Step 2: Run** `cargo nextest run -p terminal output_lines` and `cargo nextest run -p photon_terminal smart`. Expected: FAIL.
- [ ] **Step 3: Implement** the settings first. In `default.json`:
  - `"smart_blocks": true` with comment `// photon: recognise test runs, git, servers, JSON and tables in command blocks.`
  - `"notify_after_seconds": 10` with comment `// photon: notify when a command that ran at least this long finishes while Photon is in the background; 0 turns it off.`

  Then implement `block_output_lines` (walk rows with alacritty's `WRAPLINE` flag), then the entity.
- [ ] **Step 4: Run** both commands plus `cargo nextest run -p terminal_view`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: compute block insights off the UI thread`.

### Task 5: Header chip and selected-block toolbar

**Files:**
- Create: `crates/terminal_view/src/photon_smart_blocks_view.rs` (`mod` line in `terminal_view.rs` with `// photon:`)
- Modify: `crates/terminal_view/src/photon_blocks_view.rs` (`BlockChrome` gets `summary: Option<(SharedString, Hsla)>`, painted left of the metadata)
- Modify: `crates/terminal_view/src/terminal_view.rs` (render the overlay as a child of the terminal's content div, `// photon:` hook)

**Interfaces:**
- Consumes: `SmartBlocks::{insight, delta, local_url}` (Task 4); SP1a's `layout_block_chrome`, `selected_block`, `active_blocks`, `last_content.terminal_bounds`.
- Produces:
```rust
pub(crate) fn summary_chip(insight: Option<&Insight>, local_url: Option<&str>) -> Option<(SharedString, Hsla)>;
pub(crate) enum ToolbarButton { RerunFailed, OpenErrors, StageAll, OpenInGitPanel, OpenAsJson, OpenAsTable, CopyAsCsv, OpenUrl, Stop }
pub(crate) fn toolbar_buttons(insight: Option<&Insight>, rerun_available: bool, local_url: Option<&str>, running: bool) -> Vec<ToolbarButton>;
impl TerminalView { pub(crate) fn smart_block_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement>; }
```
- **Chip text:**
  - Tests with failures: `"{failed} failed · {passed} passed"` in red;
  - Tests without failures: `"{passed} passed"` in green;
  - Build: `"1 error"` / `"{n} errors"` in red;
  - running with a URL: `"● {url_label}"` in accent;
  - otherwise none.

  The chip tooltip / block menu adds the delta: `"{n} new failure(s) · {m} fixed"`.
- **Toolbar buttons** (labels exactly as shown):
  - Tests: `Re-run failed` (only if the rerun is available) and `Open errors` (only if any failure has a location);
  - Build: `Open errors`;
  - GitStatus: `Stage all`, `Open in Git panel`;
  - Json: `Open as JSON`;
  - Table: `Open as table`, `Copy as CSV`;
  - running with a URL: `Open {url_label}` and `Stop`.
- **Toolbar placement:**
  - It is an absolutely positioned `h_flex` of `ui::Button`s (small, Photon Black) on the selected block's header row, right-aligned. While it shows, the metadata is not painted.
  - Row y = `header_row × line_height` from the last frame's bounds.
  - It renders only while a block is selected and `terminal.smart_blocks` is on. It sits inside the grid's bounds and nowhere else, so it has no hitbox outside its buttons.
- Button clicks dispatch the Task 6–9 actions (defined in this task in `photon_smart_blocks_view.rs` via `actions!(photon_terminal, [...])`, handled there). Until those tasks land, handlers may be no-ops.

- [ ] **Step 1: Write failing tests:**
  - unit tests `summary_chip_texts` (each case above) and `toolbar_buttons_per_insight`;
  - GPUI tests:
    - `chip_painted_for_failed_test_block` (layout_block_chrome summary is `Some(("2 failed · 41 passed", red))`);
    - `toolbar_only_on_selected_block`;
    - `drag_selection_unaffected_by_overlay` (repeat SP1a's `drag_selects_text_not_block` with a selected smart block).
- [ ] **Step 2: Run** `cargo nextest run -p terminal_view smart chip toolbar drag`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo nextest run -p terminal_view`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: summary chip and toolbar on smart blocks`.

### Task 6: Test and build actions

**Files:**
- Modify: `crates/terminal_view/src/photon_smart_blocks_view.rs`, `crates/terminal_view/src/terminal_path_like_target.rs` (make `focus_pane_beside_center_terminal` `pub(crate)` if needed), `assets/keymaps/default-macos.json`

**Interfaces:**
- Consumes: `rerun_failed_command` (Task 2), `SmartBlocks::insight` (Task 4), SP1a `Terminal::run_command`, `focus_pane_beside_center_terminal`.
- Produces: actions `photon_terminal::{RerunFailedTests, OpenBlockErrors, NextBlockError, PreviousBlockError}`.
- **Behaviour:**
  - The actions act on the selected block. For the error actions, when nothing is selected they use the newest block with locations.
  - `RerunFailedTests` calls `run_command(rerun)` (SP1a's guarded path, so it does nothing unless at a prompt).
  - `OpenBlockErrors` opens location 0. Next/Previous cycle through the locations (failures in order for Tests, errors for Build), wrapping. Each one opens `path:line:col` beside the center terminal, resolving relative paths against the block's cwd.
  - Keys:
    - `f8` / `shift-f8` for Next/Previous in context `Terminal && photon_blocks`;
    - `cmd-alt-shift-r` for RerunFailedTests in `Terminal && photon_block_selected`.

    They also appear in the right-click block menu (`block_menu_items`) when applicable.

- [ ] **Step 1: Write failing GPUI tests:**
  - `rerun_failed_writes_command_to_pty`, which asserts the PTY write log contains the bracketed-paste of the expected cargo command;
  - `open_errors_opens_location_beside_terminal`, which uses a fake fs file and asserts an editor for that path in a pane other than the terminal's, cursor on the line;
  - `next_error_cycles_and_wraps`.
- [ ] **Step 2: Run** `cargo nextest run -p terminal_view rerun_failed open_errors next_error`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo nextest run -p terminal_view`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: re-run failed tests and step through errors`.

### Task 7: Git status and log actions

**Files:**
- Modify: `crates/photon_terminal/src/smart_blocks.rs`, `crates/photon_terminal/src/chips.rs` (reuse or extract its async `git_output` as `pub(crate) async fn run_git(cwd: &Path, args: &[&str]) -> Result<String>` returning stderr in the error)
- Modify: `crates/terminal_view/src/photon_smart_blocks_view.rs` and `photon_blocks_view.rs`/`terminal_element.rs` for row buttons and cmd-click on hashes (`// photon:` hooks)

**Interfaces:**
- Consumes: `StatusFile`, `CommitHash`, `FileState` (Task 3); `git_ui::project_diff::ProjectDiff::deploy_at_project_path`, `git_ui::commit_view::CommitView::open`, `git_panel::ToggleFocus`.
- Produces:
```rust
pub enum RowMark { Staged, Discarded }
impl SmartBlocks {
    pub fn stage(&mut self, id: BlockId, absolute_line: i64, cx: &mut Context<Self>);
    pub fn stage_all(&mut self, id: BlockId, cx: &mut Context<Self>);
    pub fn discard(&mut self, id: BlockId, absolute_line: i64, window: &mut Window, cx: &mut Context<Self>); // confirms first
    pub fn open_diff(&self, id: BlockId, absolute_line: i64, window: &mut Window, cx: &mut App);
    pub fn open_commit(&self, id: BlockId, hash: &str, window: &mut Window, cx: &mut App);
    pub fn row_mark(&self, id: BlockId, absolute_line: i64) -> Option<RowMark>;
}
```
  Actions: `photon_terminal::{StageAllChanges, OpenInGitPanel}`.
- **Behaviour:**
  - `stage` runs `git add -- <path>` and `stage_all` runs `git add -A`. `discard` shows `window.prompt(PromptLevel::Warning, "Discard changes to <path>?", None, &["Discard", "Cancel"])`, then runs `git restore -- <path>`. All of them run in the block's cwd.
  - On success, mark the row. On failure, show git's stderr as a workspace toast.
  - **Row buttons** are drawn on the selected block's file rows at the right edge (row = `absolute_line - top_absolute_line`). Staged rows get `diff`, Unstaged get `stage diff discard`, Untracked get `stage`. Marked rows show the mark text dimmed instead of buttons.
  - **Commit hashes:** cmd-click on a hash cell in a GitLog block opens the commit view for the repository containing the block's cwd. Hook this before SP1a's hyperlink handling.

- [ ] **Step 1: Write failing tests.**
  - `photon_terminal`, against a real temporary git repo (`git init`, skipped if git is missing):
    - `stage_runs_git_add_and_marks_row`
    - `stage_quotes_path` (file `a b.txt`)
    - `discard_requires_confirmation` (simulate cancel and leave the file unchanged; simulate Discard and see it restored)
    - `git_failure_shows_toast`
  - `terminal_view`:
    - `row_buttons_follow_wrapped_lines`
    - `cmd_click_hash_opens_commit_view` (assert the `CommitView` item opens; use the fake repository test helpers git_ui tests use)
- [ ] **Step 2: Run** `cargo nextest run -p photon_terminal stage discard git_failure` and `cargo nextest run -p terminal_view row_buttons cmd_click_hash`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo nextest run -p photon_terminal -p terminal_view`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: stage, diff and discard from git status blocks`.

### Task 8: Dev servers and notifications

**Files:**
- Modify: `crates/photon_terminal/src/smart_blocks.rs`, `crates/terminal_view/src/photon_smart_blocks_view.rs`

**Interfaces:**
- Consumes: `SmartBlocks::local_url`, `is_error_line`, `cx.show_system_notification(SystemNotification { tag, title, body, actions })`, `cx.on_system_notification_response`, `window.is_window_active()`, the `notify_after_seconds` setting.
- Produces: actions `photon_terminal::{OpenBlockUrl, StopBlock}`.
- **Behaviour:**
  - `OpenBlockUrl` calls `cx.open_url(url)`. `StopBlock` writes `\x03` to the PTY, but only when the selected block is running.
  - **Long-command notification:** when a block finishes, `notify_after_seconds > 0`, `duration ≥ notify_after_seconds`, and the window isn't active.
    - Title `"✓ {command}"` (exit 0) or `"✗ {command}"`. Commands longer than 60 chars are cut to 60 with `…`.
    - Body `"{format_duration} · exit {code}"`, using SP1a's `format_duration`.
    - Tag `"photon-block-{terminal entity id}-{block id}"`.
  - **Dev-server error notification:** only while the window is inactive. Scan new lines of the running block that has a URL with `is_error_line`. Title `"Error in {command}"`, body = the line (trimmed to 120 chars). At most one per block per 60 s, and the scan stops at the 2,000-line cap.
  - **On a notification response with our tag:** activate the window, focus the terminal and select the block.

- [ ] **Step 1: Write failing tests:**
  - `dev_server_chip_and_stop_sends_ctrl_c` (terminal_view, PTY write log has `[3]`);
  - in `photon_terminal`, using the test platform's recorded notifications (add a `// photon:` test-support accessor to the GPUI test platform if none exists):
    - `long_command_notifies_when_inactive`
    - `short_or_active_does_not_notify`
    - `notify_setting_zero_disables`
    - `server_error_notifies_once_per_minute`
- [ ] **Step 2: Run** `cargo nextest run -p photon_terminal notif server_error -p terminal_view dev_server`. Expected: FAIL.
- [ ] **Step 3: Implement.** Use the GPUI executor timer for the 60 s throttle so tests can advance the clock.
- [ ] **Step 4: Run** `cargo nextest run -p photon_terminal -p terminal_view`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: dev-server chip, stop, and finish notifications`.

### Task 9: Open as JSON, Open as table, Copy as CSV

**Files:**
- Modify: `crates/photon_terminal/src/smart_blocks.rs`
- Create: `crates/photon_terminal/src/table_item.rs` (a workspace `Item` wrapping `tabular_data_preview::TableView`)
- Modify: `crates/tabular_data_preview/src/tabular_data_preview.rs` (one `// photon:` hook: `pub fn table_contents(rows: Vec<Vec<String>>) -> types::TableLikeContent`, built the way `parser::from_buffer_with_delimiter` builds it)
- Modify: `crates/photon_terminal/Cargo.toml` (`tabular_data_preview`, `language`)

**Interfaces:**
- Consumes: `Insight::{Json, Table}`, `table_to_csv` (Task 3), `TerminalView`'s open-beside helper (Task 6), `TableView::{new, set_contents}`.
- Produces: actions `photon_terminal::{OpenBlockAsJson, OpenBlockAsTable, CopyBlockAsCsv}`.
- **Behaviour:**
  - **Open as JSON** opens a new untitled buffer holding the block's output text (`block_text(Output)`), with the JSON language from the project's language registry, in the pane beside the terminal. Use `serde_json::to_string_pretty` only when the original is a single line.
  - **Open as table** opens the `TableItem` beside the terminal. Its tab title is the block's command.
  - **Copy as CSV** writes `table_to_csv` to the clipboard.

- [ ] **Step 1: Write failing GPUI tests:**
  - `open_as_json_opens_json_buffer` (language name "JSON", text equal to the pretty JSON);
  - `open_as_table_opens_table_item` (the item exists with 2 data rows);
  - `copy_as_csv_writes_clipboard`.
- [ ] **Step 2: Run** `cargo nextest run -p terminal_view open_as copy_as_csv`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo nextest run -p photon_terminal -p terminal_view -p tabular_data_preview`. Expected: PASS.
- [ ] **Step 5: Commit** `photon: open block output as JSON or a table`.

### Task 10: Gates, CI and the off switch

**Files:**
- Modify:
  - `.github/workflows/photon.yml` (add `-p tabular_data_preview` if Task 9 changed it; the photon_blocks/photon_terminal/terminal/terminal_view packages are already there);
  - `script/photon-perf` only if a new measurement is needed;
  - `crates/terminal_view/src/terminal_view.rs` tests.

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Write failing GPUI test** `smart_blocks_off_shows_sp1a_blocks`. With `terminal.smart_blocks: false`, a failing cargo block shows no chip and no toolbar, and the block menu has only SP1a's items.
- [ ] **Step 2: Run** it. Expected: FAIL if the off switch isn't wired through every path; otherwise make the test fail first by disabling one path, then restore it.
- [ ] **Step 3: Run the gates:**
  - `script/photon-perf`: all SP1a limits pass and the baseline doesn't regress by more than 10%;
  - `script/photon-netcheck`: clean;
  - `cargo fmt --all -- --check`;
  - `./script/clippy`;
  - `cargo nextest run -p photon_blocks -p photon_terminal -p terminal -p terminal_view -p zed`.
- [ ] **Step 4: Commit** `photon: smart blocks off switch and CI`.
