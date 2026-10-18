# Sub-project 0: Lean Baseline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the Zed checkout into Photon: rebranded, quiet on the network by default, opening to a terminal first, themed with Warp's Dark theme, with tests and a performance baseline.

**Architecture:** Photon-specific code lives in the new crate `crates/photon_brand` and a glue module, `crates/zed/src/zed/photon.rs`. Zed's own files are changed only through small hooks, each marked with `// photon:`. Most behavior changes are new defaults in `assets/settings/default.json`.

**Tech Stack:** Rust 1.98.1, GPUI, cargo-nextest, clap, serde_json_lenient, zsh scripts using `lsof` and `ps`, GitHub Actions (macOS).

**Spec:** `docs/photon/specs/2026-10-18-sp0-lean-baseline-design.md`

## Global Constraints

- Product name `Photon`, lowercase `photon`. App ID base `io.github.vundyalaavinash.photon`.
- Mark every edit to a Zed file with a `// photon: <why>` comment, or `# photon:` in TOML, or a `"photon:"` note in a JSON comment. `git grep -n "photon:"` must list all of them.
- Follow `.rules`: no `unwrap()`, no `let _ =` on fallible calls, no `mod.rs`, full-word variable names, and comments only to explain why.
- Dedication text, exactly: `Dedicated to Maha & Bhumi.` It must never appear in `--help`, the README, menus, or settings.
- Colors (Warp Dark): background `#050505`, foreground `#FFFFFF`, accent `#19AAD8`. ANSI normal `#616161 #FF8272 #B4FA72 #FEFDC2 #A5D5FE #FF8FFD #D0D1FE #F1F1F1`, ANSI bright `#8E8E8E #FFC4BD #D6FCB9 #FEFDD5 #C1E3FE #FFB1FE #E5E6FE #FEFFFF`. Comments use `#8E8E8E`.
- Typography: Hack, size 13, line height 1.2, weight 400, bar cursor that blinks, in both the terminal and the editor.
- Performance budgets: ≤ 200 MB idle physical footprint (revised by the user from 150 MB RSS during Task 8), ≤ 300 ms cold start. Locally, fail on a regression of more than 10%. In CI, fail at more than 20%.
- No outbound connection to anything other than loopback during the 30 s after launch.
- Photon never reads Zed's `~/.config/zed`, `~/Library/Application Support/Zed`, or Zed's Keychain items.
- Disk: about 100 GB free. Run `cargo clean -p <crate>` when `target/` goes over 50 GB (`du -sh target`).

## Review Focus

1. **An existing Zed user** (this user has Zed installed and signed in). Photon must start with no settings carried over and stay signed out. Task 3 tests the paths; Task 4 tests that `authenticate` is never spawned.
2. **A restored session that already has files open.** Photon must not add an extra terminal on top of the restored tabs. Task 6 tests this.
3. **A remote or SSH workspace.** The center terminal must open on the remote host, so the hook uses `local: false`. Task 6 tests this. The spec's `local: true` was wrong.
4. **User settings that override Photon defaults** (a different theme, font, or `"telemetry": {"metrics": true}`). The user's values must win. Task 5 tests this.
5. **Prompts that use Nerd Font or powerline glyphs that Hack doesn't have.** They must fall back to another font and must not turn into missing-glyph boxes or crash. Task 5 sets `font_fallbacks` and does a manual check.

---

### Task 1: Build prerequisites, first build, and fork hygiene

**Files:**
- Modify: `.rules` (Zed's agent rules; `CLAUDE.md`, `AGENTS.md` and `GEMINI.md` are symlinks to it)
- Modify: `README.md`
- Check: `.github/workflows/*.yml`

- [ ] **Step 1: Ask the user for approval, then install prerequisites.** Run `brew install cmake`, then `xcodebuild -downloadComponent MetalToolchain` (only if the build later reports a missing `metal` compiler).
- [ ] **Step 2: Do the first build.** Run `cargo build -p zed`. Expected: it finishes, and `target/debug/zed` exists. This takes a long time, so run it in the background.
- [ ] **Step 3: Measure upstream Zed to get a reference.** Run `./target/debug/zed .`, then check the debug RSS with `ps -o rss= -p <pid>` and write the number down. Debug builds use much more memory than release builds; this is only a sanity check.
- [ ] **Step 4: Rewrite `.rules` for Photon.**
  - Keep the "Rust coding guidelines", "Timers in tests" and "GPUI" sections word for word.
  - Delete the README review-marker "HARD RULE" bullet and any Zed-internal process rules.
  - Add a "Photon fork rules" section with the hook and `// photon:` rules from Global Constraints.
- [ ] **Step 5: Rewrite `README.md`.**
  - Explain what Photon is (one paragraph).
  - Give the build commands: `cargo run -p zed`, and `script/bundle-mac` for a bundle.
  - Add the line "Photon is a fork of [Zed](https://github.com/zed-industries/zed) by Zed Industries, licensed GPL-3.0."
  - Keep the license section.
- [ ] **Step 6: Make sure Zed's CI doesn't run in the fork.** Run `grep -L "github.repository_owner == 'zed-industries'\|github.repository == 'zed-industries/zed'" .github/workflows/*.yml`. Add that guard at the job level, with a `# photon:` comment, only to the workflows the grep lists.
- [ ] **Step 7: Commit.** `git commit -am "photon: fork hygiene — rules, README, upstream CI guarded"`

### Task 2: `photon_brand` crate and the hidden `photon --for`

**Files:**
- Create: `crates/photon_brand/Cargo.toml` (`[lib] path = "src/photon_brand.rs"`, no dependencies)
- Create: `crates/photon_brand/src/photon_brand.rs`
- Modify: root `Cargo.toml` (add the crate to `members` and `[workspace.dependencies]`)
- Modify: `crates/cli/Cargo.toml` (dependency `photon_brand`), `crates/cli/src/main.rs:154` (add the flag), plus the early-exit handling in `main`
- Test: inline `#[cfg(test)] mod tests` in `photon_brand.rs`; existing `mod tests` in `crates/cli/src/main.rs:310`

**Interfaces:**
- Produces:
  - `pub const DEDICATION: &str` (exact text from Global Constraints)
  - `pub const BHUMI: u32 = 0x050505`
  - `pub const MAHA: u32 = 0x19AAD8`
  - `pub const FOREGROUND: u32 = 0xFFFFFF`
  - `pub const ANSI_NORMAL: [u32; 8]` and `pub const ANSI_BRIGHT: [u32; 8]` (values in Global Constraints, in the order black, red, green, yellow, blue, magenta, cyan, white)
  - `pub const COMMENT: u32 = 0x8E8E8E`
  - `pub fn contrast_ratio(foreground: u32, background: u32) -> f32` (WCAG 2.x relative luminance)

- [ ] **Step 1: Write the failing tests.**
  ```rust
  #[test] fn contrast_matches_wcag_reference() {
      assert!((contrast_ratio(0xFFFFFF, 0x000000) - 21.0).abs() < 0.01);
      assert!((contrast_ratio(0x616161, BHUMI) - 3.3).abs() < 0.05);
      assert!((contrast_ratio(MAHA, BHUMI) - 7.6).abs() < 0.05);
  }
  #[test] fn text_colors_pass_aa_on_bhumi() {
      for color in [FOREGROUND, MAHA, COMMENT].iter().chain(&ANSI_NORMAL[1..]).chain(&ANSI_BRIGHT) {
          assert!(contrast_ratio(*color, BHUMI) >= 4.5, "{color:06X}");
      }
  }
  ```
  In `cli`: `#[test] fn dedication_flag_is_hidden_from_help()`. It renders `Args::command().render_long_help()` and asserts the output contains neither `--for` nor `Maha`. Add `#[test] fn for_flag_parses()`, which asserts `Args::try_parse_from(["cli", "--for"])` succeeds with `args.dedication == true`.
- [ ] **Step 2: Run the tests and watch them fail.** `cargo nextest run -p photon_brand -p cli`. Expected: compile errors or failures.
- [ ] **Step 3: Implement `photon_brand.rs` and the CLI flag.** The flag is `#[arg(long = "for", hide = true)] dedication: bool`. In `main`, right after parsing and before any bundle lookup: if `args.dedication` is set, `println!("{}", photon_brand::DEDICATION)` and return `Ok(())`. Leave `ANSI_NORMAL[0]` (`#616161`) out of the AA loop: it's the ANSI black color, used as a background, never as text.
- [ ] **Step 4: Run the tests and see them pass.** Same command. Expected: all pass. Also run `cargo run -p cli -- --for`, which should print the dedication.
- [ ] **Step 5: Commit.** `git commit -m "photon: photon_brand palette + hidden --for"`

### Task 3: Rebrand names, IDs, paths, bundle, and CLI symlink

**Files:**
- Modify: `crates/paths/src/paths.rs:18,22` (`APP_NAME = "Photon"`, `APP_NAME_LOWERCASE = "photon"`)
- Modify: `crates/release_channel/src/lib.rs:45-50` (`app_identifier`), `:206-211` (`display_name` becomes `Photon Dev`, `Photon Nightly`, `Photon Preview`, `Photon`), `:228-233` (`app_id` becomes `io.github.vundyalaavinash.photon-dev`, `-nightly`, `-preview`, and plain for stable)
- Modify: `crates/zed/Cargo.toml:286-320`: in all four bundle sections, `name` becomes `Photon` plus the channel suffix, and `identifier` matches `app_id`
- Modify: `crates/install_cli/src/install_cli_binary.rs:28` (`/usr/local/bin/photon`), plus the user-facing strings at `:101`, `:111` and `:132`, which say `photon`
- Modify: `crates/zed/src/zed/app_menus.rs:74` and `crates/zed/src/zed.rs:1721`: build "About …" from `ReleaseChannel::global(cx).display_name()`
- Replace: `crates/zed/resources/app-icon*.png` with placeholder art (a white dot on `#050505` with a `#19AAD8` ring). Keep the same sizes as the files being replaced (check with `sips -g pixelWidth`)
- Test: inline tests in `paths.rs` and `release_channel/src/lib.rs`

- [ ] **Step 1: Write the failing tests.**
  - `paths`: `#[test] fn paths_are_photon_not_zed()` asserts, for each of `config_dir()`, `data_dir()`, `logs_dir()` and `database_dir()`, that the path doesn't contain `zed` or `Zed`, and that it ends in (or contains) `photon` or `Photon`.
  - `release_channel`: `#[test] fn ids_are_photon()` asserts, for every channel, that `app_id()` starts with `io.github.vundyalaavinash.photon` and `display_name()` starts with `Photon`.
- [ ] **Step 2: Run them and watch them fail.** `cargo nextest run -p paths -p release_channel`. Expected: FAIL.
- [ ] **Step 3: Make the edits listed under Files.** Add a `// photon:` comment to each.
- [ ] **Step 4: Run the tests and see them pass.** Same command, then `cargo build -p zed`. Run `grep -rn '"Zed"' crates/zed/src/zed/app_menus.rs` and fix any remaining menu titles.
- [ ] **Step 5: Commit.** `git commit -am "photon: rebrand names, ids, paths, bundle, CLI link"`

### Task 4: Offline by default

**Files:**
- Modify: `assets/settings/default.json`:
  - `:1711-1716` telemetry: `diagnostics` and `metrics` both `false`
  - `:1726` `auto_update: false`
  - `:617-640`: `show_sign_in`, `show_user_menu` and `show_user_picture` all `false`
  - `:1071-1077`: `collaboration_panel.button: false`
  - `:1919-1921`: `edit_predictions.provider: "none"`
- Modify: `crates/zed/src/main.rs:879-882`: delete the `authenticate` spawn, with a `// photon:` comment. Keep `fn authenticate` with `#[allow(dead_code)]` to make upstream merges easier.
- Modify: `crates/extension_host/src/extension_host.rs:460-468`: remove the `auto_install_extensions` and `check_for_updates` calls from the startup spawn. Instead, call `check_for_updates` when the Extensions page opens (the `zed_actions::Extensions` handler in `crates/extensions_ui`).
- Modify: `crates/project/src/agent_registry_store.rs:150-154`: don't run `refresh` inside `init_global`. Refresh is then triggered by the existing `refresh_if_stale` calls (`agent_servers/src/custom.rs:226`, `project/src/agent_server_store.rs:321`) when agents are actually resolved.
- Modify: `crates/zed/src/main.rs` and the auto-update menu item in `crates/zed/src/zed/app_menus.rs`: hide "Check for Updates".
- Create: `crates/zed/src/zed/photon.rs`, which `crates/zed/src/zed.rs` uses as `mod photon;`. It contains `pub fn hide_offline_actions(cx: &mut App)`, which calls `CommandPaletteFilter::update_global(cx, |filter, _| for namespace in ["collab", "channel_modal", "call", "client", "auto_update", "zed_predict_onboarding", "notification_panel", "chat_panel"] { filter.hide_namespace(namespace) })`. Call it from `initialize_workspace`. The namespace list must be checked against `#[action(namespace = …)]` attributes in `crates/collab_ui` and `crates/client` (`grep -rhn "namespace = " crates/collab_ui crates/client crates/call crates/auto_update`).
- Test: `crates/photon_brand/tests/defaults.rs` (dev-dependencies `serde_json_lenient` and `serde_json`); a gpui test in `crates/zed/src/zed/photon.rs`.

**Interfaces:**
- Produces: `crates/zed/src/zed/photon.rs`, which Tasks 6 and 7 add to. It exports `hide_offline_actions(cx: &mut App)`.
- Produces: `crates/photon_brand/tests/defaults.rs`, with a helper `fn default_settings() -> serde_json::Value` that parses `include_str!("../../../assets/settings/default.json")`. Tasks 5 and 6 add tests to this file.

- [ ] **Step 1: Write the failing tests.**
  - `defaults.rs`: `#[test] fn offline_defaults()` asserts `telemetry.diagnostics == false`, `telemetry.metrics == false`, `auto_update == false`, `title_bar.show_sign_in == false`, `title_bar.show_user_menu == false`, `collaboration_panel.button == false`, and `edit_predictions.provider == "none"`.
  - `photon.rs`: `#[gpui::test] fn offline_actions_hidden(cx)`. It inits the command palette hooks, calls `hide_offline_actions`, and asserts that `CommandPaletteFilter::global(cx).is_hidden(&collab::ToggleScreenSharing)` (or another collab action that exists) is true and `is_hidden(&workspace::NewCenterTerminal::default())` is false.
- [ ] **Step 2: Run them and watch them fail.** `cargo nextest run -p photon_brand -p zed photon`. Expected: FAIL.
- [ ] **Step 3: Make the edits listed under Files.**
- [ ] **Step 4: Run the tests and see them pass.** Same command. Read `client::telemetry::should_install_crash_handler` (`crates/zed/src/main.rs:386`) and confirm it returns false when `telemetry.diagnostics` is false. If it doesn't, add a `// photon:` hook that makes it check that setting. Then `cargo nextest run -p extension_host -p project agent_registry` to confirm the upstream tests still pass. If a registry test relied on `init_global` refreshing, change that test to call `refresh` explicitly.
- [ ] **Step 5: Commit.** `git commit -am "photon: offline by default (telemetry, updates, sign-in, extensions, registry)"`

### Task 5: Photon Black theme, Hack font, and typography

**Files:**
- Create: `assets/themes/photon/photon.json`. Copy the structure of `assets/themes/one/one.json`. It contains one theme, `"name": "Photon Black"`, `"appearance": "dark"`, and `"author": "Photon (colors from Warp Dark, warpdotdev/warp default_themes.rs)"`.
  - Colors: `background` and `editor.background` are `#050505`. `terminal.background` is `#050505`. `text` is `#ffffff`. `text.accent`, `border.focused` and the player cursor are `#19aad8`.
  - Surfaces (white mixed over `#050505`): `surface.background` 4% (`#0f0f0f`), `elevated_surface.background` 7% (`#171717`), `border` 12% (`#232323`).
  - `terminal.ansi.*` and `terminal.ansi.bright_*`: the Global Constraints values.
  - Syntax colors (spec §5.2): keyword `#ff8ffd`, string `#b4fa72`, function `#a5d5fe`, type `#d0d1fe`, number/constant/boolean `#fefdc2`, operator/punctuation `#f1f1f1`, variable `#ffffff`, comment `#8e8e8e`.
  - Diff colors: created `#b4fa72`, deleted `#ff8272`, modified `#a5d5fe`, with `*.background` at 15% alpha (`…26`).
- Create: `assets/fonts/hack/Hack-{Regular,Bold,Italic,BoldItalic}.ttf` from https://github.com/source-foundry/Hack/releases/tag/v3.003 (`Hack-v3.003-ttf.zip`), plus `assets/fonts/hack/LICENSE.md`. They are picked up automatically by the `fonts/**/*` embed in `crates/assets/src/assets.rs:15`.
- Modify: `assets/settings/default.json`:
  - `:9-13` theme becomes `{"mode": "dark", "light": "One Light", "dark": "Photon Black"}`
  - `buffer_font_family: "Hack"`, `buffer_font_size: 13`, `buffer_line_height: {"custom": 1.2}`, `cursor_shape: "bar"`
  - `terminal`: `font_family: "Hack"`, `font_size: 13`, `line_height: {"custom": 1.2}`, `cursor_shape: "bar"`, `blinking: "on"`, `font_fallbacks: ["Symbols Nerd Font Mono", "Apple Symbols"]`
  - `ui_font_size: 13`
- Modify: `crates/settings_content/src/theme.rs:345`: `DEFAULT_DARK_THEME = "Photon Black"`
- Test: `crates/photon_brand/tests/theme.rs`, plus additions to `crates/photon_brand/tests/defaults.rs`

**Interfaces:**
- Consumes: `photon_brand::{BHUMI, MAHA, FOREGROUND, ANSI_NORMAL, ANSI_BRIGHT, COMMENT, contrast_ratio}` (Task 2), and `default_settings()` (Task 4).

- [ ] **Step 1: Write the failing tests.**
  - `theme.rs`: `#[test] fn photon_black_matches_brand()` parses `include_str!("../../../assets/themes/photon/photon.json")`, finds the theme named "Photon Black", and asserts that `background` is `BHUMI`, `text.accent` is `MAHA`, `text` is `FOREGROUND`, and the 16 `terminal.ansi.*` values equal `ANSI_NORMAL` and `ANSI_BRIGHT` in order. The helper `fn hex(value: &str) -> u32` drops a leading `#` and any alpha byte.
  - `#[test] fn syntax_and_text_pass_aa()`: every `syntax.*.color` and `text*` color has `contrast_ratio >= 4.5` against `BHUMI`.
  - `defaults.rs`: `#[test] fn warp_typography_defaults()` asserts the font family, size, line height and cursor values listed above, and that `theme.dark == "Photon Black"`.
  - `#[test] fn user_settings_override_photon_defaults()` (Review Focus 4). It uses `SettingsStore::test(cx)` with the user setting `{"theme": {"mode": "dark", "dark": "One Dark"}, "telemetry": {"metrics": true}}`, and asserts that the resolved theme is "One Dark" and `TelemetrySettings::get_global(cx).metrics` is true. Put this one in the gpui test module of `crates/zed/src/zed/photon.rs`.
- [ ] **Step 2: Run them and watch them fail.** `cargo nextest run -p photon_brand -p zed photon`. Expected: FAIL.
- [ ] **Step 3: Add the theme, the fonts and the settings changes.** The upstream test `zed::tests::test_bundled_themes`, or whatever test in `crates/zed` loads every bundled theme, must also pass with the new file. Run `cargo nextest run -p zed theme`.
- [ ] **Step 4: Run the tests and see them pass.** Then do a manual check. In `cargo run -p zed`, open a Rust file and the terminal and compare side by side with Warp at the same window size: font, line spacing and colors should look the same. Run `printf ' \n'` in the terminal: you should see glyphs, not missing-glyph boxes (Review Focus 5). If `Symbols Nerd Font Mono` isn't installed, the glyphs fall back to Apple Symbols. Write down what you saw in the commit body.
- [ ] **Step 5: Commit.** `git commit -am "photon: Photon Black (Warp Dark) theme, bundled Hack 13/1.2"`

### Task 6: Terminal-first startup and agent defaults

**Files:**
- Modify: `crates/zed/src/zed/photon.rs`: add `pub fn ensure_center_terminal(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>)`. It calls `TerminalView::deploy(workspace, &NewCenterTerminal { local: false }, window, cx)` only if `workspace.items(cx).next().is_none()`.
- Modify: `crates/zed/src/main.rs:1535-1575`. In both `open_new` closures, replace `Editor::new_file(...)` with `photon::ensure_center_terminal(...)`, keeping the `Launchpad` branch. Replace the first-open `show_onboarding_view` branch (`:1556`) with the same `open_new` and terminal path, then write the `FIRST_OPEN` key so this branch can't loop.
- Modify: `crates/zed/src/zed/open_listener.rs:485`. When `open_paths` resolves for paths that are all directories, call `ensure_center_terminal` on the returned workspace, inside `workspace.update_in`.
- Modify: `assets/settings/default.json`: set `terminal.button: false`, and add `"devin": {"type": "custom", "command": "devin", "args": ["acp"]}` to `agent_servers` at `:2984`.
- Modify: `crates/agent_ui/src/agent_panel.rs:203`. `read_global_last_used_agent` returns `Some(Agent::Custom { id: AgentId::new("claude-acp") })` (use the `CLAUDE_AGENT_ID` constant from `agent_servers`) when there's no saved value. Add a `// photon:` comment.
- Test: the gpui test module in `photon.rs`, and `defaults.rs`

**Interfaces:**
- Consumes: the `photon.rs` module (Task 4). `TerminalView::deploy(workspace, &NewCenterTerminal, window, cx)` (`terminal_view.rs:225`).
- Produces: `ensure_center_terminal`. Sub-project 1 reuses it.

- [ ] **Step 1: Write the failing tests.**
  - `#[gpui::test] async fn empty_workspace_gets_center_terminal(cx)`: open a test workspace on an empty `FakeFs` folder, call `ensure_center_terminal`, and assert that the active center item downcasts to `TerminalView` and there are no dock terminals.
  - `#[gpui::test] async fn restored_items_suppress_terminal(cx)` (Review Focus 2): open a file in the workspace first, call `ensure_center_terminal`, and assert the item count is still 1 and the active item is still the editor.
  - `#[gpui::test] async fn remote_project_terminal_not_forced_local(cx)` (Review Focus 3): assert that `ensure_center_terminal` sends an action with `local == false`. Do this by checking the `NewCenterTerminal` value it builds; pull the action out into `fn center_terminal_action() -> NewCenterTerminal` so it can be tested.
  - `defaults.rs`: `#[test] fn agent_defaults()` asserts that `agent_servers.devin.command == "devin"`, `args == ["acp"]`, and `terminal.button == false`.
  - In `agent_panel.rs`'s own test module: `#[test] fn default_agent_is_claude_when_unset()`. Pass an empty `KeyValueStore::test` (or whatever test constructor the file already uses) to `read_global_last_used_agent` and assert the result is `Some(Agent::Custom { id })` with `id == CLAUDE_AGENT_ID`.
- [ ] **Step 2: Run them and watch them fail.** `cargo nextest run -p zed photon -p agent_ui default_agent -p photon_brand`. Expected: FAIL.
- [ ] **Step 3: Make the edits listed under Files.**
- [ ] **Step 4: Run the tests and see them pass.** Same command. Then run the upstream tests `cargo nextest run -p agent_ui -p terminal_view`. Manual checks:
  - `cargo run -p zed -- /tmp` opens with a full-size terminal.
  - The agent panel lists Devin and opens Claude Code by default.
  - With `devin` temporarily removed from `PATH`, picking Devin shows an error and doesn't crash.
- [ ] **Step 5: Commit.** `git commit -am "photon: terminal-first startup, Devin + Claude default agents"`

### Task 7: Hidden dedication in the About window and `main.rs`

**Files:**
- Modify: `crates/zed/src/zed.rs:1545-1700` (`AboutWindow`). Add a field `show_dedication: bool`. Wrap the app icon in an `on_click` handler that sets `show_dedication = event.modifiers().alt` and calls `cx.notify()`. When the field is true, render `photon_brand::DEDICATION` as a muted `Label` under the version line.
- Modify: `crates/zed/Cargo.toml` (dependency `photon_brand`)
- Modify: `crates/zed/src/main.rs`, line 1: `// For Maha & Bhumi — the light this was built by.`
- Test: the `AboutWindow` test (or the gpui test module in `photon.rs` if `AboutWindow` is private: give `AboutWindow::new` and the field `pub(crate)` visibility)

- [ ] **Step 1: Write the failing test.** `#[gpui::test] fn about_dedication_only_on_alt_click(cx)`: create an `AboutWindow`, assert that `show_dedication` is false, simulate a click without modifiers and assert it's still false, then simulate a click with `alt` and assert it's true.
- [ ] **Step 2: Run it and watch it fail.** `cargo nextest run -p zed about_dedication`. Expected: FAIL.
- [ ] **Step 3: Implement it.**
- [ ] **Step 4: Run it and see it pass.** Same command. Manual check: About Photon, then Option-click the icon, and the dedication appears.
- [ ] **Step 5: Commit.** `git commit -am "photon: about window touch"`. Keep the message vague so the dedication stays hidden.

### Task 8: Network silence check and performance baseline

**Files:**
- Create: `tooling/photon-perf/sample/` (`README.md` and `notes.txt`, a few lines each, no code that would start a language server)
- Create: `script/photon-netcheck` (zsh, executable)
- Create: `script/photon-perf` (zsh, executable)
- Create: `tooling/photon-perf/baseline.json` (`{"idle_rss_mb": N, "cold_start_ms": N, "measured_on": "<machine>", "date": "YYYY-MM-DD"}`)
- Modify: `crates/zed/src/main.rs`. When the env var `PHOTON_PERF=1` is set, log `photon-perf first-frame <ms since process start>` to stderr once, on the first window's first frame. Use `window.on_next_frame` inside the first `open_new` or `open_paths` callback, timed from the `Instant` captured at the top of `main`.

**Script contracts:**
- `script/photon-netcheck [app-binary]` (default `target/release/zed`):
  1. Launch the app on `tooling/photon-perf/sample` with `--user-data-dir "$(mktemp -d)"`, so each run starts with no settings (this is the `CUSTOM_DATA_DIR` override, `paths.rs:52`). If the app binary doesn't accept that flag, grep `crates/zed/src/main.rs` for where `set_custom_data_dir` is called and use the flag it reads.
  2. Poll `lsof -nP -i -a -p <pid>` every 1 s for 30 s. Count only connections to addresses other than `127.0.0.1`, `::1` and `localhost`.
  3. Print each offending line, kill the app, and exit 1 if there were any. Otherwise exit 0.
- `script/photon-perf [--update-baseline] [--threshold 10]`:
  1. Run `cargo build --release -p zed -p cli`.
  2. Run 5 times, each with a fresh `--user-data-dir`: launch with `PHOTON_PERF=1`, read `first-frame` from stderr, wait 10 s, read `ps -o rss= -p <pid>`, and kill the app.
  3. Report the median of each. Fail with exit 1 if a median is over budget (200 MB footprint, 300 ms) or more than `threshold`% worse than `baseline.json`. `--update-baseline` writes the medians to `baseline.json` instead.

- [ ] **Step 1: Write `photon-netcheck` and run it against a known-bad build.** Temporarily set `auto_update: true` and `telemetry.metrics: true` in a user settings file inside the temporary data dir. Expected: exit 1, listing the update or telemetry hosts. This proves the check can catch problems.
- [ ] **Step 2: Run netcheck on the real build.** `cargo build --release -p zed && script/photon-netcheck`. Expected: exit 0. For each connection it reports, find what opened it with `log stream --predicate 'process == "zed"'` or by grepping the URL in `crates/`, add a `// photon:` hook in the style of Task 4, and run again until it exits 0.
- [ ] **Step 3: Add the `PHOTON_PERF` hook and `photon-perf`, then record the baseline.** Run `script/photon-perf --update-baseline`, then `script/photon-perf`. Expected: exit 0, with medians within budget. If idle memory is over budget, **stop and report the number to the user.** Don't loosen the budget without asking.
- [ ] **Step 4: Commit.** `git add script/photon-* tooling/photon-perf crates/zed/src/main.rs && git commit -m "photon: netcheck + perf baseline"`

### Task 9: CI workflow and final verification

**Files:**
- Create: `.github/workflows/photon.yml`
  - Job `check`, on `pull_request` and `push` to `main`, `runs-on: macos-15`:
    1. checkout
    2. `dtolnay/rust-toolchain` using `rust-toolchain.toml`
    3. `Swatinem/rust-cache`
    4. `brew install cmake`
    5. `cargo fmt --all -- --check`
    6. `cargo clippy -p photon_brand -p paths -p release_channel -p cli -p install_cli -p agent_ui -p extension_host -p zed -- -D warnings`
    7. `cargo install cargo-nextest --locked`
    8. `cargo nextest run -p photon_brand -p paths -p release_channel -p cli -p agent_ui -p zed photon`
  - Job `perf`, on `push` to `main` and `schedule: cron "0 6 * * *"`: build release, then `script/photon-netcheck` and `script/photon-perf --threshold 20`.

- [ ] **Step 1: Write the workflow.** Validate it locally with `actionlint .github/workflows/photon.yml` if `actionlint` is installed; otherwise check the YAML with `python3 -c "import yaml,sys; yaml.safe_load(open(sys.argv[1]))" .github/workflows/photon.yml`.
- [ ] **Step 2: Verify everything locally against the spec's "Done means".**
  1. Run `script/bundle-mac` (or `cargo run -p zed`, then Install CLI), then `photon .`. A Photon window opens in Photon Black with a full-size terminal.
  2. `script/photon-netcheck` exits 0.
  3. Visually confirm there's no sign-in or collab UI.
  4. The agent picker shows Zed's agents plus Devin.
  5. Run the nextest command from the workflow and `script/photon-perf`; both pass.
  6. `ls ~/Library/Application\ Support/Photon ~/.config/photon` exist, and the modification times of `~/.config/zed` haven't changed (`stat -f %m`).
  7. `photon --for` prints the dedication, and `photon --help | grep -c Maha` prints 0.
- [ ] **Step 3: List every hook.** `git grep -n "photon:" -- ':!docs'`. Paste the list into the commit body so upstream merges have a checklist.
- [ ] **Step 4: Commit.** `git add .github/workflows/photon.yml && git commit -m "photon: CI (tests on PR, netcheck+perf nightly)"`
