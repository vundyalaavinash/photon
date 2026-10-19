# Photon SP1b: Smart Blocks

Status: draft · 2026-10-19 · user skipped the questions; the source is the approved mockup `docs/photon/specs/sp1a-mockups/smart-blocks.html` (ideas B, C, D, E). Everything not in the mockup is Claude's call and is marked as such.

## Why

SP1a gave Photon Warp's block terminal. SP1b makes a block *understand what its command did*, so the thing you were about to hunt for in the output (which tests failed, which file changed, where the server is listening) is one click away. The AI parts of the mockup (A, F, G, "Write commit message", "Why is worker crashing?") are SP3.

## Goals

1. **Test & build blocks (B).**
   - The header shows a summary chip, for example `2 failed · 41 passed` (red when anything failed, green otherwise), or `3 errors` for a build.
   - Actions:
     - **Re-run failed only** builds a command that runs just the failed tests and runs it as a new block.
     - **Open errors** opens the first failure location in a split beside the terminal (SP1a's path opening). Next/previous error actions step through the rest.
     - **Δ vs last run** compares with the previous run of the same command in the same folder in this terminal, for example `1 new failure · 2 fixed`. It is shown in the chip tooltip and the block menu.
   - **Tools recognised:**
     - test runners: `cargo test`, `cargo nextest`, `pytest`, `jest`/`vitest` (also through `npm`/`pnpm`/`yarn`/`bun` scripts), `go test`;
     - build errors: rustc/cargo (`error[...]` + `--> file:line:col`), `tsc` (`file(line,col): error`), and gcc/clang style (`file:line:col: error:`).
   - Recognition goes by **output format**, so wrappers like `npm test` and `make test` work. The command line is used only to build the re-run command. When the re-run command can't be built safely, that action is hidden.
2. **Git blocks you can act on (C).**
   - **`git status`, long and short formats.** Each changed file's row gets buttons at its right edge, shown while the block is selected:
     - **stage**, which runs `git add -- <path>`;
     - **diff**, which opens Zed's diff view for that file;
     - **discard**, for tracked files only. It asks to confirm in a modal, then runs `git restore -- <path>`.

     The header has **Stage all** (`git add -A`) and **Open in Git panel**. Stage and discard run as background git processes, not as new blocks. On success the row is marked `staged` or `discarded`, dimmed. On failure git's message shows as a toast.
   - **`git log`** (any format that prints hashes): commit hashes are clickable and open Zed's commit view.
3. **Long-running blocks (D).**
   - **Dev servers.** When a running block prints a local URL (`localhost`, `127.0.0.1`, `0.0.0.0`, `[::1]`), the header shows the chip `Open localhost:5173`, which opens the system browser, and a **Stop** button, which sends ctrl-C to the PTY.
   - **Notifications.** A macOS notification is sent when:
     - a command that ran ≥ `terminal.notify_after_seconds` (default 10) finishes while the Photon window isn't active, saying "✓ cargo build · 42s" or "✗ cargo test · exit 101";
     - a running dev-server block prints an error line while the window isn't active (at most one per block per minute).

     Clicking the notification focuses that terminal and selects the block.
4. **Tables & JSON (E).**
   - **JSON.** When a finished block's output (trimmed) parses as JSON, the **Open as JSON** action opens it in a new untitled editor buffer with the JSON language, so Zed's folding and syntax highlighting apply.
   - **Tables.** When the output is column-aligned (`kubectl get`, `docker ps`, `ps aux`), the **Open as table** action opens it in Zed's existing sortable, filterable table viewer (`crates/tabular_data_preview`) beside the terminal. **Copy as CSV** copies it as CSV.
     - "Column-aligned" means a header line plus ≥2 rows whose columns are separated by character positions that are blank on every line, giving ≥2 columns. The last column may contain spaces.
5. **Everything is reachable without the mouse.** Every block action is also in the block's right-click menu and the command palette while a block is selected. Re-run failed, Open errors and Next/previous error get keybindings (Claude's call; the builder picks the keys, avoiding SP1a's).

## Non-goals (SP1b)

- **Anything that needs an agent:** fix with agent, agent commands as blocks, `#` plain English, AI commit messages, "why is this crashing". All of these are SP3.
- **Collapse noise and Pin to top** for dev servers. Both need to hide or move rows of the single terminal grid that every block shares, which SP1a's design doesn't allow. Revisit if the grid model changes.
- **Drawing tables or JSON trees inline in the terminal.** They open beside it instead, because the grid can't grow rows for them.
- **git commands beyond `status` and `log`**, git log graphs, and blocks inside `ssh`.
- **Shells other than zsh** (unchanged from SP1a).

## Anti-goals (a failure even if it works)

- **It hides or rewrites output.** The raw output always stays exactly as printed. Smart blocks only add a chip, buttons and actions on top. A wrong guess may show a wrong chip, but must never lose text.
- **It runs anything without a click.** Recognisers only read text. Every git change, re-run or stop is an explicit click or keypress. `discard` always confirms.
- **It slows the terminal.** SP1a's limits all still hold:
  - idle CPU < 1%;
  - 100k lines ≤ 10% slower than `terminal.blocks: false`;
  - < 1 ms per command on the UI thread;
  - footprint ≤ 200 MB;
  - cold start ≤ 300 ms.
- **It makes network calls** (`photon-netcheck` stays clean). Opening a URL in the browser is the user's click.

## How it works (Claude's calls; the builder may refine)

- **Recognisers are pure functions** in a new module of `crates/photon_blocks`: `fn recognise(command: &str, output: &str, exit_code: Option<i32>) -> Option<Insight>`. `Insight` is an enum with these variants:
  - `Tests {passed, failed, failures: Vec<Failure{name, location}>, rerun_failed: Option<String>}`
  - `Build {errors: Vec<Location>}`
  - `GitStatus {files}`, each with its line, path and staged/unstaged/untracked state
  - `GitLog {hashes}`, each with its line and column range, opened by cmd-click (SP1a's convention for links)
  - `Json`
  - `Table {columns, rows}`

  They are tested against fixture outputs captured from the real tools.
- **When they run:** once, when a block ends (D mark), on a background thread, with the block's output text (`block_text(BlockPart::Output)`). If the output is over 20,000 lines or 5 MB, there is no insight.
  - The dev-server URL scan is the only work done while a block runs. It checks new output lines of the running block on terminal wakeups and stops after the first URL or 2,000 lines.
  - The error-line scan for notifications runs only while the window is inactive.
- **Where results live:** an `insight` field next to each block in `TerminalBlocks` (SP1a's `crates/terminal/src/photon.rs`), dropped with the block.
- **Rendering:** the existing block chrome (`crates/terminal_view/src/photon_blocks_view.rs`) draws:
  - the summary chip in the header row;
  - a Warp-style action toolbar at the header's right edge while the block is selected (a plain click selects, as in SP1a), replacing the metadata while visible. Claude's call: selection rather than hover, so overlay hitboxes never sit over the grid while you drag-select text;
  - per-row buttons for git status rows.

  No grid rows are added or hidden.
- **Git** reuses SP1a's async git process helper (`chips.rs`). The diff view, commit view and Git panel are Zed's (`crates/git_ui`).
- **Notifications** use GPUI's `show_system_notification`.
- **Settings:**
  - `terminal.smart_blocks` (default `true`) turns all of SP1b off.
  - `terminal.notify_after_seconds` (default `10`; `0` turns notifications off).

  Both are set in `assets/settings/default.json`.
- **Fork rules:** new code goes in `crates/photon_blocks` and `crates/photon_terminal`, with minimal `// photon:` hooks elsewhere (SP0 rules).

## Testing

- **Unit tests per recogniser,** on fixture outputs:
  - passing and failing runs for each test tool;
  - build errors for each format;
  - long and short `git status`;
  - `git log` (default and `--oneline`);
  - JSON (valid, invalid, huge);
  - aligned tables (kubectl, docker ps, ps aux);
  - near-misses that must give no insight.
- **Re-run command builder:** one test per tool, plus the "can't build safely" cases (shell operators, unknown wrappers).
- **GPUI tests:**
  - the chip and toolbar appear on a finished test block;
  - Re-run failed writes the right command to the PTY;
  - stage runs git and marks the row;
  - discard needs confirmation;
  - Open as JSON and Open as table open the right items;
  - the dev-server chip appears while running, and Stop sends ctrl-C;
  - `terminal.smart_blocks: false` shows SP1a blocks only.
- **Gates:** `photon-perf` (all SP1a limits), `photon-netcheck`, fmt and clippy.
- **Visual check (the user's eyes):** the chip and toolbar against the mockup and Warp.

## Left to the builder

- Exact chip and toolbar styling within the Photon Black theme.
- Keybindings for the new actions.
- Recogniser internals and the fixture set beyond the cases above.
- How the table is handed to `tabular_data_preview` (a temporary TSV buffer, or feeding its `TableView` directly).
