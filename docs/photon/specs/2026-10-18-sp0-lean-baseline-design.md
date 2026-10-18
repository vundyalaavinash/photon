# Sub-project 0 — Lean baseline (design)

Status: draft for review · Date: 2026-10-18 · Upstream base: zed `56934ec056` (2026-10-17)

## 1. Goal

Turn a clean Zed checkout into **Photon**: a rebranded, offline-by-default, terminal-first build with
the Photon Black theme, automated tests, and a measured performance baseline. No new end-user
features beyond that — those come in sub-projects 1–6.

### Done means

1. `photon .` opens a `Photon.app` window, themed Photon Black, with a full-size terminal in the center.
2. Zero outbound network connections during 30 s idle after launch.
3. No sign-in, collaboration, or Zed-hosted AI UI is visible.
4. The agent picker lists Zed's external agents plus Devin.
5. All tests in §7 pass; `script/photon-perf` reports numbers within budget (§6).
6. Photon never reads or writes Zed's settings, data, or keychain credentials.

### Non-goals

Removing crates from the build; block terminal, tabs, git, PR review, Excalidraw (later sub-projects);
code signing / notarization / distribution; Linux and Windows (they keep compiling, but are untested).

## 2. Fork strategy

- This repository is the fork. `upstream` = `zed-industries/zed`. Photon's trunk is `main`.
  `origin` (the user's GitHub repo) is created later, on request.
- **Rule:** Photon code goes in `crates/photon_*`. Edits to Zed's own files are minimal *hooks*
  (a default value, an `if`, a call into a `photon_*` crate). Each hook gets a `// photon:` comment
  so `git grep "photon:"` lists every place we touched upstream code.
- Merging upstream: manually, whenever the user wants (`git fetch upstream && git merge upstream/main`).
- Zed's agent rules (`.rules`, symlinked as `CLAUDE.md`/`AGENTS.md`/`GEMINI.md`) are replaced by Photon's
  rules. We keep Zed's Rust and GPUI guidance and drop Zed's own PR-process rules (for example, the
  README review-marker rule).
- `README.md` is rewritten for Photon. The text credits Zed, and `LICENSE-GPL` / `LICENSE-APACHE` are kept.
  **Photon is GPL-3.0**, the same as the Zed editor it derives from.

## 3. Rebrand

| What | Where (upstream location) | Change |
|---|---|---|
| Product name | `crates/paths/src/paths.rs:18` `APP_NAME` (and `APP_NAME_LOWERCASE` at `:22`) | `"Photon"` / `"photon"`. This also moves every data path: `~/.config/photon`, `~/Library/Application Support/Photon`, `~/Library/Logs/Photon` |
| Channel display names | `crates/release_channel/src/lib.rs:206-211` | `Photon Dev`, `Photon Nightly`, `Photon Preview`, `Photon` |
| App IDs | `crates/release_channel/src/lib.rs:228-233` and `:45-50` | `io.github.vundyalaavinash.photon[-dev…]`. We use a reverse-domain ID based on the user's GitHub account because we don't own a domain |
| Bundle metadata | `crates/zed/Cargo.toml:286-320` (one section per channel) | `name = "Photon"`, with the same identifiers as the row above |
| Icons | `crates/zed/resources/app-icon*.png`, `Document.icns` | A placeholder Photon icon (a simple photon/dot mark). Final art comes later |
| CLI name | The binary stays `cli` inside the bundle. The "Install CLI" action creates a shell symlink that is renamed `photon` | Users type `photon .`, `photon <file>` |
| Menus and window titles | `crates/zed/src/zed/app_menus.rs:74` ("About Zed"), `crates/zed/src/zed.rs:1721` | Read the name from `paths::APP_NAME` or the channel's display name instead of hard-coding "Zed" |
| Inner executable | `Contents/MacOS/zed` | **Not renamed**, because renaming the `zed` crate would conflict with every upstream merge. Activity Monitor still shows the bundle name "Photon" |

## 4. Offline by default (switch off, not delete)

| Source of network or UI | Upstream location | Photon change |
|---|---|---|
| Telemetry events and crash reports | `default.json:1711-1716`, `client/src/telemetry.rs:566,375`, `zed/src/main.rs:386` | Defaults become `"telemetry": {"diagnostics": false, "metrics": false}`. Verify that the crash handler is not installed when `diagnostics` is false |
| Auto-update | `default.json:1726`, `auto_update.rs:289-299` | `"auto_update": false`, and hide the "Check for Updates" menu item |
| Sign-in and credentials | `zed/src/main.rs:879-882` `authenticate()`; `zed_credentials_provider` | **Hook:** never call `authenticate` at startup. **Hook:** prefix every keychain item with `photon+` (macOS keychain items are keyed only by server URL, not by app ID, so without this Photon would read Zed's API keys and zed.dev login). Also ignore `zed://channel` links, which would connect to zed.dev |
| Sign-in and user menu UI | `default.json:617-640` | `show_sign_in: false`, `show_user_menu: false`, `show_user_picture: false` |
| Collaboration panel | `default.json:1071-1077` | `collaboration_panel.button: false`. Collaborator list and call controls already render nothing when there's no room (`title_bar/src/collab.rs:336`), so no change there |
| Zed-hosted AI and edit predictions | `default.json:1205-1209`, `:1919-1921`, `agent_ui/src/agent_ui.rs:520` (`Agent::NativeAgent` is `#[default]`) | `edit_predictions.provider: "none"`. **Hook:** when there's no last-used agent (`agent_panel.rs:1261` fallback), default to `Agent::Custom { name: "claude-acp" }` instead of `NativeAgent`, so a fresh install opens Claude Code. Picking another agent is remembered as usual |
| Extension auto-install and update check | `extension_host.rs:460-468`, `:778` | **Hook:** skip the update check at startup. It runs only when the Extensions page is opened |
| ACP agent registry | `project/src/agent_registry_store.rs:20,269,344` | **Hook:** fetch only when the agent picker or panel is first opened, never at startup |
| Command palette | — | Hide the actions for collab, channels, calls, sign-in and Zed account, using Zed's existing action filter for hidden namespaces (`CommandPaletteFilter`) |

## 5. Defaults: agents, theme, and terminal-first layout

### 5.1 Agents

Keep Zed's full set of external agents (`agent_servers/src/custom.rs:17-19`: Claude, Gemini, Codex, and
the rest). Add Devin to `default.json` `agent_servers` (`:2984`):

```json
"devin": { "type": "custom", "command": "devin", "args": ["acp"] }
```

If `devin` isn't on `PATH`, the picker shows the existing "command not found" error. No extra code.

### 5.2 Photon Black theme

- New file `assets/themes/photon/photon.json`, using the same structure as `one.json`, with one theme
  named **"Photon Black"** (dark). The default theme becomes `"mode": "dark"`, `"dark": "Photon Black"`
  (`default.json:9-13`, `settings_content/src/theme.rs:344-345`). The light theme stays One Light.
- **The colors match Warp's default "Dark" theme exactly.** Values are taken from Warp's open-source
  `app/src/themes/default_themes.rs` (`dark_theme()`, `DARK_MODE_*_COLORS`). Color values are facts,
  not code, and AGPL-3 is compatible with GPL-3 in any case. We credit Warp in `photon.json`.
- The palette's source of truth lives in the new crate `photon_brand`:
  - `BHUMI = #050505` is Warp's background. Panels, the title bar and the status bar are lighter
    shades of it, mixed from white the way Warp's `Details::Darker` does.
  - `MAHA = #19AAD8` is Warp's accent: cursor, selection, focus rings, the active tab.
  - Foreground text is `#FFFFFF`.
  - Terminal ANSI colors, normal: `#616161 #FF8272 #B4FA72 #FEFDC2 #A5D5FE #FF8FFD #D0D1FE #F1F1F1`.
  - Terminal ANSI colors, bright: `#8E8E8E #FFC4BD #D6FCB9 #FEFDD5 #C1E3FE #FFB1FE #E5E6FE #FEFFFF`.
- **The editor uses the same palette, so the editor and terminal look like one app.** Code-highlighting
  colors are mapped onto Warp's ANSI colors:

  | Code element | Warp ANSI color it uses |
  |---|---|
  | Keywords | magenta `#FF8FFD` |
  | Strings | green `#B4FA72` |
  | Functions | blue `#A5D5FE` |
  | Types | cyan `#D0D1FE` |
  | Numbers, constants | yellow `#FEFDC2` |
  | Operators, punctuation | `#F1F1F1` |
  | Variables | `#FFFFFF` |
  | Errors | red `#FF8272` |

  - **Comments use bright black `#8E8E8E`**, not normal black `#616161`. On `#050505`, `#616161` is
    only about 3.3:1 contrast, which fails WCAG AA for text.
  - **Git and diff colors:** additions green, deletions red, modifications blue, all on 15%-alpha tints
    of the background.
- The theme JSON can't reference constants, so **a test ties them together**: the JSON's
  `background` must equal `BHUMI` and its `text.accent` must equal `MAHA`.

### 5.2a Typography and cursor (Warp defaults)

These are the defaults in Warp's `settings_schema.json`, and they match the user's own `~/.warp/settings.toml`:

| Setting | Warp | Photon default (`default.json`) |
|---|---|---|
| Font | **Hack** | `terminal.font_family: "Hack"` and `buffer_font_family: "Hack"` |
| Font size | **13** | `terminal.font_size: 13`, `buffer_font_size: 13` |
| Line height | **1.2** | `terminal.line_height: {"custom": 1.2}`, `buffer_line_height: {"custom": 1.2}` |
| Weight | normal | 400 |
| Cursor | **bar, blinking** | `terminal.cursor_shape: "bar"`, `terminal.blinking: "on"`, `cursor_shape: "bar"` |
| UI spacing | "normal" | Zed's default UI density. UI font size 13 |

- **Hack is bundled** in `assets/fonts/hack/` (regular, bold, italic and bold italic, about 1.2 MB, under
  the MIT and Bitstream Vera licenses), so Photon looks the same without a font install. It's
  registered the same way Zed registers its bundled fonts.
- **Out of scope here: where things sit inside the terminal** (input box pinned to the bottom, command
  blocks stacked above it, the prompt line showing the folder and git branch, block padding). Zed's
  terminal is a plain character grid, and those layouts only exist once blocks exist, so they're
  specified in sub-project 1 to match Warp's "pinned to bottom" input mode. That spec checks alignment
  with side-by-side screenshots against Warp at the same window size.
- **Tab bar** (sub-project 2): tabs on top, close button on the right, the active tab keeps its color.
  These match Warp's `tab_close_button_position = "right"` and your `preserve_active_tab_color = true`.

### 5.3 Terminal-first layout

- When a workspace opens with no items to restore (for example, `photon .` on a new folder, or the
  first launch), Photon opens a **terminal in the center pane** (`workspace::NewCenterTerminal { local: false }` so remote projects get a remote shell,
  `workspace.rs:627`). Today Zed opens an empty file (`zed/src/main.rs:1535-1575`). That's one hook.
- Zed's onboarding view (`main.rs:1556`), which is mostly sign-in and Zed AI setup, is skipped.
- The bottom terminal dock is still there but its status-bar button is hidden (`terminal.button: false`).
  The terminal now lives in the center.
- `restore_on_startup` stays `last_session`.
- **Deferred to sub-project 1:** toggling between the terminal and the last editor, opening `path:line`
  from terminal output in a split, and `photon <file>` from inside Photon's terminal targeting the
  current window. They need new actions, which belong with the terminal work.

### 5.4 Hidden dedication

The dedication lives in `photon_brand`:

```rust
pub const DEDICATION: &str = "Dedicated to Maha & Bhumi.";
```

- `photon --for` prints `DEDICATION` and exits 0. It's hidden from `--help` (`#[arg(long, hide = true)]`,
  next to `askpass` in `crates/cli/src/main.rs:154`).
- In the About window (`zed.rs:1545-1700`), Option-clicking the logo shows `DEDICATION` under the version.
- A one-line comment carrying the dedication goes at the top of `crates/zed/src/main.rs`.
- The first Photon commit message ends with the dedication.

## 6. Performance baseline

`script/photon-perf` (a shell script) builds release, then launches `Photon.app` on a fixed sample
project (`tooling/photon-perf/sample/`, a few small text files, no language servers), and reports:

- **Cold start:** time from launch until the first window is drawn. This needs a single hook that logs
  the first frame, behind the env var `PHOTON_PERF=1`.
- **Idle memory:** RSS of the app process 10 s after the first frame (`ps -o rss`).

Budgets on the user's M-series Mac: **≤ 200 MB idle physical footprint** (the number Activity Monitor shows; revised from 150 MB RSS on 2026-10-18 after measuring: ~86 MB of it is GPU window buffers that scale with window size, ~62 MB is app heap) **and ≤ 300 ms cold start**. Results are written to
`tooling/photon-perf/baseline.json`, and a regression of more than 10% against it fails.

**Where it runs.** On every PR it runs locally, as part of the pre-merge checklist. In CI it runs on
pushes to `main` and nightly, with a **20%** threshold. Shared GitHub macOS runners are too noisy for 10%,
and a release build there takes about an hour, which is too slow to run per PR.

## 7. Testing

| Layer | What | How |
|---|---|---|
| Unit | Defaults: telemetry, auto-update and sign-in are off; `edit_predictions` is `none`; Devin is in `agent_servers`; the default dark theme is Photon Black | `#[test]` that parses `assets/settings/default.json` |
| Unit | Paths resolve under `Photon`/`photon` and never `Zed`/`zed` | `#[test]` in `paths` |
| Unit | The theme matches `photon_brand`, including Warp's exact background, accent and 16 ANSI colors; every text/background pair is at least WCAG AA (4.5:1) | `#[test]` in `photon_brand` that loads `photon.json` |
| Unit | Typography defaults are Hack 13 with line height 1.2 and a blinking bar cursor, in both the terminal and the editor; the bundled Hack font loads | `#[test]` on `default.json`, plus a gpui test that the font is available |
| UI (headless) | The title bar has no sign-in or collab buttons; the command palette has no collab or account actions; a new empty workspace has a terminal as the active center item | `#[gpui::test]` with `TestAppContext`, following `title_bar/src/application_menu.rs:454` |
| CLI | `--for` prints the dedication; `--help` doesn't contain "Maha" | Inline `#[test]` in `crates/cli` (existing `mod tests`, `:310`) |
| Network | No connections other than localhost during 30 s idle | `script/photon-netcheck`: launch the app, poll `lsof -i -a -p <pid>` every second, fail on any non-loopback socket |
| Perf | §6 | `script/photon-perf` |

**CI** (`.github/workflows/photon.yml`, macOS runner):
- On every PR: `cargo fmt --check`, then `clippy`, then `cargo nextest` on `photon_*`, `paths`,
  `release_channel`, `title_bar`, `cli`, `terminal_view` and the other crates Photon touches.
- On pushes to `main` and nightly: netcheck and perf.

Zed's own CI workflows are disabled in the fork. Zed's full test suite runs locally before each
upstream merge.

## 8. Risks

- **Disk:** about 100 GB free. A debug plus release `target/` can reach 60 GB or more. The plan
  includes `cargo clean -p` hygiene and an optional `CARGO_TARGET_DIR` on another drive.
- **Missing prerequisites:** `cmake` (`brew install cmake`). The Metal toolchain may also be needed
  on macOS 26 (`xcodebuild -downloadComponent MetalToolchain`).
- **Hidden network calls:** some may exist that this design doesn't list. The netcheck test is the
  backstop, and anything it catches gets its own hook.
- **Merge drift:** the hooks are tracked by `// photon:` comments. If a hook's location moves upstream,
  `git grep` finds it.

## 9. Concepts you'll learn here

- **Cargo workspaces and crates:** why there are around 250 crates, incremental builds, and why new code goes in its own crates.
- **GPUI basics:** the app, windows, entities, actions, and settings as layered JSON (the lesson in chat).
- **macOS app bundles:** what a `.app` contains (`Info.plist`, bundle ID, `Contents/MacOS`), and why the bundle ID decides the keychain service and preferences domain.
- **Forking a large project:** hooks instead of rewrites, keeping track of every place we diverge from upstream, and merging upstream changes.
- **Measuring rather than guessing:** RSS compared with memory footprint, and how to time a cold start honestly.
