use editor::Editor;
use gpui::{
    Action, Anchor, AnyElement, ClipboardItem, Div, Entity, EntityId, Hsla, Pixels, Point,
    SharedString, Stateful, actions, anchored, point, rgb,
};
use photon_blocks::{
    Block, BlockId, FileState, Insight, RunDelta, SourceLocation, rerun_failed_command,
    table_to_csv, url_label,
};
use photon_terminal::{RowMark, SmartBlocks, TableItem};
use settings::Settings as _;
use terminal::terminal_settings::TerminalSettings;
use terminal::{BlockPart, InputTarget, PathLikeTarget};
use ui::{ButtonSize, ContextMenu, prelude::*};
use util::ResultExt as _;
use workspace::{Pane, Workspace};

use crate::{
    TerminalView,
    photon_blocks_view::{active_blocks, layout_block_chrome},
    terminal_path_like_target::{focus_pane_beside_center_terminal, open_location_beside_terminal},
};

actions!(
    photon_terminal,
    [
        /// Runs only the failed tests of the selected block again.
        RerunFailedTests,
        /// Opens the source locations of the selected block's errors.
        OpenBlockErrors,
        /// Opens the next error of the selected block, or of the newest block with errors.
        NextBlockError,
        /// Opens the previous error of the selected block, or of the newest block with errors.
        PreviousBlockError,
        /// Stages every change listed by the selected `git status` block.
        StageAllChanges,
        /// Opens the Git panel for the selected `git status` block.
        OpenInGitPanel,
        /// Opens the selected block's JSON output in an editor.
        OpenBlockAsJson,
        /// Opens the selected block's table output as a table.
        OpenBlockAsTable,
        /// Copies the selected block's table output as CSV.
        CopyBlockAsCsv,
        /// Opens the local URL the selected running block printed.
        OpenBlockUrl,
        /// Stops the selected running block.
        StopBlock,
    ]
);

const FAILURE: u32 = 0xFF8272;
const SUCCESS: u32 = 0xB4FA72;
const ACCENT: u32 = 0x19AAD8;
const BUTTON_BORDER: u32 = 0x333333;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolbarButton {
    RerunFailed,
    OpenErrors,
    StageAll,
    OpenInGitPanel,
    OpenAsJson,
    OpenAsTable,
    CopyAsCsv,
    OpenUrl,
    Stop,
}

impl ToolbarButton {
    pub(crate) fn label(self, local_url: Option<&str>) -> String {
        match self {
            Self::RerunFailed => "Re-run failed".to_string(),
            Self::OpenErrors => "Open errors".to_string(),
            Self::StageAll => "Stage all".to_string(),
            Self::OpenInGitPanel => "Open in Git panel".to_string(),
            Self::OpenAsJson => "Open as JSON".to_string(),
            Self::OpenAsTable => "Open as table".to_string(),
            Self::CopyAsCsv => "Copy as CSV".to_string(),
            Self::OpenUrl => format!("Open {}", local_url.map(url_label).unwrap_or_default()),
            Self::Stop => "Stop".to_string(),
        }
    }

    fn action(self) -> Box<dyn Action> {
        match self {
            Self::RerunFailed => Box::new(RerunFailedTests),
            Self::OpenErrors => Box::new(OpenBlockErrors),
            Self::StageAll => Box::new(StageAllChanges),
            Self::OpenInGitPanel => Box::new(OpenInGitPanel),
            Self::OpenAsJson => Box::new(OpenBlockAsJson),
            Self::OpenAsTable => Box::new(OpenBlockAsTable),
            Self::CopyAsCsv => Box::new(CopyBlockAsCsv),
            Self::OpenUrl => Box::new(OpenBlockUrl),
            Self::Stop => Box::new(StopBlock),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowButton {
    Stage,
    Diff,
    Discard,
}

impl RowButton {
    fn label(self) -> &'static str {
        match self {
            Self::Stage => "stage",
            Self::Diff => "diff",
            Self::Discard => "discard",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RowContent {
    Buttons(Vec<RowButton>),
    Mark(RowMark),
}

/// A short-status row like `MM a.txt` is both staged and unstaged, so it gets both sets.
fn row_buttons(states: impl Iterator<Item = FileState>) -> Vec<RowButton> {
    let states: Vec<FileState> = states.collect();
    let unstaged = states.contains(&FileState::Unstaged);
    let stage = unstaged || states.contains(&FileState::Untracked);
    let diff = unstaged || states.contains(&FileState::Staged);
    [
        stage.then_some(RowButton::Stage),
        diff.then_some(RowButton::Diff),
        unstaged.then_some(RowButton::Discard),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// `local_url` is passed only while the block runs.
pub(crate) fn summary_chip(
    insight: Option<&Insight>,
    local_url: Option<&str>,
) -> Option<(SharedString, Hsla)> {
    let (text, color) = match insight {
        Some(Insight::Tests(run)) if run.failed > 0 => (
            format!("{} failed · {} passed", run.failed, run.passed),
            FAILURE,
        ),
        Some(Insight::Tests(run)) => (format!("{} passed", run.passed), SUCCESS),
        Some(Insight::Build { errors }) if errors.len() == 1 => ("1 error".to_string(), FAILURE),
        Some(Insight::Build { errors }) => (format!("{} errors", errors.len()), FAILURE),
        _ => (format!("● {}", url_label(local_url?)), ACCENT),
    };
    Some((text.into(), rgb(color).into()))
}

pub(crate) fn toolbar_buttons(
    insight: Option<&Insight>,
    rerun_available: bool,
    local_url: Option<&str>,
    running: bool,
) -> Vec<ToolbarButton> {
    let mut buttons = match insight {
        Some(Insight::Tests(run)) => {
            let located = run
                .failures
                .iter()
                .any(|failure| failure.location.is_some());
            [
                rerun_available.then_some(ToolbarButton::RerunFailed),
                located.then_some(ToolbarButton::OpenErrors),
            ]
            .into_iter()
            .flatten()
            .collect()
        }
        Some(Insight::Build { .. }) => vec![ToolbarButton::OpenErrors],
        Some(Insight::GitStatus { .. }) => {
            vec![ToolbarButton::StageAll, ToolbarButton::OpenInGitPanel]
        }
        Some(Insight::Json) => vec![ToolbarButton::OpenAsJson],
        Some(Insight::Table(_)) => vec![ToolbarButton::OpenAsTable, ToolbarButton::CopyAsCsv],
        Some(Insight::GitLog { .. }) | None => Vec::new(),
    };
    if running && local_url.is_some() {
        buttons.extend([ToolbarButton::OpenUrl, ToolbarButton::Stop]);
    }
    buttons
}

pub(crate) fn delta_text(delta: &RunDelta) -> Option<String> {
    let new_failures = delta.new_failures.len();
    let fixed = delta.fixed.len();
    let plural = if new_failures == 1 { "" } else { "s" };
    (new_failures + fixed > 0)
        .then(|| format!("{new_failures} new failure{plural} · {fixed} fixed"))
}

fn running_url<'a>(block: &Block, smart_blocks: &'a SmartBlocks) -> Option<&'a str> {
    smart_blocks
        .local_url(block.id)
        .filter(|_| block.end_line.is_none())
}

pub(crate) fn block_summary(
    block: &Block,
    smart_blocks: &SmartBlocks,
) -> Option<(SharedString, Hsla)> {
    summary_chip(
        smart_blocks.insight(block.id),
        running_url(block, smart_blocks),
    )
}

/// `at_prompt`: the shell waits for a command, so a re-run would not be typed into a program.
pub(crate) fn block_toolbar(
    block: &Block,
    smart_blocks: &SmartBlocks,
    at_prompt: bool,
) -> Vec<ToolbarButton> {
    let insight = smart_blocks.insight(block.id);
    let rerun_available = at_prompt
        && matches!(
            insight,
            Some(Insight::Tests(run)) if rerun_failed_command(&block.command, run).is_some()
        );
    toolbar_buttons(
        insight,
        rerun_available,
        running_url(block, smart_blocks),
        block.end_line.is_none(),
    )
}

/// The pane to the right of a center terminal, or the active pane for the dock terminal.
fn pane_beside_terminal(
    workspace: &mut Workspace,
    terminal_id: EntityId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<Pane> {
    focus_pane_beside_center_terminal(workspace, terminal_id, window, cx)
        .unwrap_or_else(|| workspace.active_pane().clone())
}

fn block_locations(insight: Option<&Insight>) -> Vec<&SourceLocation> {
    match insight {
        Some(Insight::Tests(run)) => run
            .failures
            .iter()
            .filter_map(|failure| failure.location.as_ref())
            .collect(),
        Some(Insight::Build { errors }) => errors.iter().collect(),
        _ => Vec::new(),
    }
}

#[derive(Clone, Copy)]
enum ErrorStep {
    First,
    Next,
    Previous,
}

impl ErrorStep {
    fn index(self, current: Option<usize>, count: usize) -> usize {
        match (self, current) {
            (Self::Next, Some(index)) => (index + 1) % count,
            (Self::Previous, Some(index)) => (index + count - 1) % count,
            (Self::Previous, None) => count - 1,
            (Self::First | Self::Next, _) => 0,
        }
    }
}

pub(crate) fn register_smart_block_actions(
    element: Stateful<Div>,
    cx: &mut Context<TerminalView>,
) -> Stateful<Div> {
    element
        .on_action(cx.listener(TerminalView::rerun_failed_tests))
        .on_action(cx.listener(|this, _: &OpenBlockErrors, window, cx| {
            this.open_block_error(ErrorStep::First, window, cx)
        }))
        .on_action(cx.listener(|this, _: &NextBlockError, window, cx| {
            this.open_block_error(ErrorStep::Next, window, cx)
        }))
        .on_action(cx.listener(|this, _: &PreviousBlockError, window, cx| {
            this.open_block_error(ErrorStep::Previous, window, cx)
        }))
        .on_action(cx.listener(|this, _: &StageAllChanges, _, cx| {
            if this.enabled_smart_blocks(cx).is_none() {
                return;
            }
            if let (Some((smart_blocks, _)), Some(selected)) =
                (&this.smart_blocks, this.selected_block)
            {
                smart_blocks.update(cx, |smart_blocks, cx| smart_blocks.stage_all(selected, cx));
            }
        }))
        .on_action(cx.listener(TerminalView::open_block_as_json))
        .on_action(cx.listener(TerminalView::open_block_as_table))
        .on_action(cx.listener(TerminalView::copy_block_as_csv))
        .on_action(cx.listener(|this, _: &OpenBlockUrl, _, cx| {
            if let (Some(smart_blocks), Some(selected)) =
                (this.enabled_smart_blocks(cx), this.selected_block)
                && let Some(url) = smart_blocks.local_url(selected)
            {
                cx.open_url(url);
            }
        }))
        .on_action(cx.listener(|this, _: &StopBlock, _, cx| {
            if this.enabled_smart_blocks(cx).is_none() {
                return;
            }
            let terminal = this.terminal.read(cx);
            let running = active_blocks(terminal).and_then(|blocks| blocks.running());
            if !this.read_only && running.is_some() && running == this.selected_block {
                this.terminal
                    .update(cx, |terminal, _| terminal.input(b"\x03".as_slice()));
            }
        }))
        .on_action(cx.listener(|this, _: &OpenInGitPanel, window, cx| {
            if this.enabled_smart_blocks(cx).is_some() {
                window.dispatch_action(Box::new(zed_actions::git_panel::ToggleFocus), cx)
            }
        }))
}

pub(crate) fn smart_block_menu_items(
    menu: ContextMenu,
    buttons: Vec<(ToolbarButton, String)>,
) -> ContextMenu {
    buttons.into_iter().fold(menu, |menu, (button, label)| {
        menu.action(label, button.action())
    })
}

impl TerminalView {
    /// The selected block's actions with their labels.
    pub(crate) fn selected_block_buttons(&self, cx: &App) -> Vec<(ToolbarButton, String)> {
        let (Some(smart_blocks), Some(selected)) =
            (self.enabled_smart_blocks(cx), self.selected_block)
        else {
            return Vec::new();
        };
        let terminal = self.terminal.read(cx);
        let Some(block) = active_blocks(terminal).and_then(|blocks| blocks.block(selected)) else {
            return Vec::new();
        };
        let at_prompt = terminal.input_target() == InputTarget::BlockInput;
        let local_url = running_url(block, smart_blocks);
        block_toolbar(block, smart_blocks, at_prompt)
            .into_iter()
            .map(|button| (button, button.label(local_url)))
            .collect()
    }

    /// Brings the window and this terminal forward and selects the block a notification is about.
    pub(crate) fn focus_block(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        window.activate_window();
        if let Some(workspace) = self.workspace.upgrade() {
            let item = cx.entity();
            // The workspace reads this view while activating it, so it must not be mid-update.
            window.defer(cx, move |window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.activate_item(&item, true, true, window, cx);
                });
            });
        }
        self.focus_handle.focus(window, cx);
        if active_blocks(self.terminal.read(cx)).is_some_and(|blocks| blocks.block(id).is_some()) {
            self.selected_block = Some(id);
            cx.emit(workspace::searchable::SearchEvent::MatchesInvalidated);
            cx.notify();
        }
    }

    fn rerun_failed_tests(&mut self, _: &RerunFailedTests, _: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.selected_block else {
            return;
        };
        if self.read_only {
            return;
        }
        let Some(smart_blocks) = self.enabled_smart_blocks(cx) else {
            return;
        };
        let Some(Insight::Tests(run)) = smart_blocks.insight(selected) else {
            return;
        };
        let Some(command) = self
            .terminal
            .read(cx)
            .blocks()
            .and_then(|blocks| blocks.block(selected))
            .and_then(|block| rerun_failed_command(&block.command, run))
        else {
            return;
        };
        self.terminal
            .update(cx, |terminal, cx| terminal.run_command(&command, cx));
    }

    /// Without errors to step through, the key is left to the terminal.
    fn open_block_error(&mut self, step: ErrorStep, window: &mut Window, cx: &mut Context<Self>) {
        let Some(smart_blocks) = self.enabled_smart_blocks(cx) else {
            cx.propagate();
            return;
        };
        let Some(blocks) = active_blocks(self.terminal.read(cx)) else {
            cx.propagate();
            return;
        };
        let has_locations = |id: BlockId| !block_locations(smart_blocks.insight(id)).is_empty();
        let Some(block) = (match self.selected_block {
            Some(selected) => blocks.block(selected),
            None => blocks.blocks().rev().find(|block| has_locations(block.id)),
        }) else {
            cx.propagate();
            return;
        };
        let locations = block_locations(smart_blocks.insight(block.id));
        if locations.is_empty() {
            cx.propagate();
            return;
        }
        let current = self
            .block_error_cursor
            .filter(|(id, _)| *id == block.id)
            .map(|(_, index)| index);
        let index = step.index(current, locations.len());
        let Some(location) = locations.get(index) else {
            return;
        };
        let mut maybe_path = format!("{}:{}", location.path.display(), location.line);
        if let Some(column) = location.column {
            maybe_path.push_str(&format!(":{column}"));
        }
        let target = PathLikeTarget {
            maybe_path,
            working_directory: block.cwd.clone(),
        };
        self.block_error_cursor = Some((block.id, index));
        open_location_beside_terminal(&self.workspace, &target, window, cx);
    }

    fn open_block_as_json(
        &mut self,
        _: &OpenBlockAsJson,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(smart_blocks), Some(selected)) =
            (self.enabled_smart_blocks(cx), self.selected_block)
        else {
            return;
        };
        if !matches!(smart_blocks.insight(selected), Some(Insight::Json)) {
            return;
        }
        let Some(output) = self
            .terminal
            .read(cx)
            .block_text(selected, BlockPart::Output)
        else {
            return;
        };
        let text = match output.trim() {
            single_line if !single_line.contains('\n') => {
                serde_json::from_str::<serde_json::Value>(single_line)
                    .and_then(|value| serde_json::to_string_pretty(&value))
                    .unwrap_or_else(|_| single_line.to_string())
            }
            _ => output,
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let terminal_id = cx.entity_id();
        let (project, pane) = workspace.update(cx, |workspace, cx| {
            let pane = pane_beside_terminal(workspace, terminal_id, window, cx);
            (workspace.project().clone(), pane)
        });
        let language = project.read(cx).languages().language_for_name("JSON");
        let buffer = project.update(cx, |project, cx| project.create_buffer(None, false, cx));
        window
            .spawn(cx, async move |cx| {
                let buffer = buffer.await?;
                let language = language.await.log_err();
                buffer.update(cx, |buffer, cx| {
                    buffer.set_text(text, cx);
                    if let Some(language) = language {
                        buffer.set_language(Some(language), cx);
                    }
                });
                workspace.update_in(cx, |workspace, window, cx| {
                    let editor =
                        cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
                    workspace.add_item(pane, Box::new(editor), None, true, true, window, cx);
                })
            })
            .detach_and_log_err(cx);
    }

    fn open_block_as_table(
        &mut self,
        _: &OpenBlockAsTable,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(smart_blocks), Some(selected)) =
            (self.enabled_smart_blocks(cx), self.selected_block)
        else {
            return;
        };
        let Some(Insight::Table(table)) = smart_blocks.insight(selected) else {
            return;
        };
        let Some(block) = active_blocks(self.terminal.read(cx)).and_then(|b| b.block(selected))
        else {
            return;
        };
        let title = SharedString::from(block.command.clone());
        let rows: Vec<Vec<String>> = std::iter::once(table.headers.clone())
            .chain(table.rows.iter().cloned())
            .collect();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let terminal_id = cx.entity_id();
        // The workspace reads this view while focusing the other pane, so it must not be
        // mid-update.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                let pane = pane_beside_terminal(workspace, terminal_id, window, cx);
                let item = cx.new(|cx| TableItem::new(title, rows, window, cx));
                workspace.add_item(pane, Box::new(item), None, true, true, window, cx);
            });
        });
    }

    fn copy_block_as_csv(&mut self, _: &CopyBlockAsCsv, _: &mut Window, cx: &mut Context<Self>) {
        let (Some(smart_blocks), Some(selected)) =
            (self.enabled_smart_blocks(cx), self.selected_block)
        else {
            return;
        };
        if let Some(Insight::Table(table)) = smart_blocks.insight(selected) {
            let csv = table_to_csv(table);
            cx.write_to_clipboard(ClipboardItem::new_string(csv));
        }
    }

    /// Insights recorded before `terminal.smart_blocks` was turned off stay hidden.
    pub(crate) fn enabled_smart_blocks<'a>(&'a self, cx: &'a App) -> Option<&'a SmartBlocks> {
        let (smart_blocks, _) = self.smart_blocks.as_ref()?;
        TerminalSettings::get_global(cx)
            .smart_blocks
            .then(|| smart_blocks.read(cx))
    }

    pub(crate) fn selected_block_delta(&self, cx: &App) -> Option<String> {
        self.enabled_smart_blocks(cx)?
            .delta(self.selected_block?)
            .and_then(delta_text)
    }

    /// The selected `git status` block's file rows on screen, as (screen row, absolute line,
    /// what to draw at the row's right end).
    pub(crate) fn selected_status_rows(&self, cx: &App) -> Vec<(usize, i64, RowContent)> {
        let (Some(smart_blocks), Some(selected)) =
            (self.enabled_smart_blocks(cx), self.selected_block)
        else {
            return Vec::new();
        };
        let terminal = self.terminal.read(cx);
        let Some(block) = active_blocks(terminal).and_then(|blocks| blocks.block(selected)) else {
            return Vec::new();
        };
        let Some(Insight::GitStatus { files }) =
            smart_blocks.located_insight(block, terminal.grid_columns())
        else {
            return Vec::new();
        };
        let top_line = terminal.top_absolute_line();
        let screen_rows = terminal.last_content.terminal_bounds.num_lines();
        let mut rows: Vec<(usize, i64, RowContent)> = Vec::new();
        for file in files {
            let Some(row) = usize::try_from(file.absolute_line - top_line)
                .ok()
                .filter(|row| *row < screen_rows)
            else {
                continue;
            };
            if rows
                .last()
                .is_some_and(|(_, line, _)| *line == file.absolute_line)
            {
                continue;
            }
            let content = match smart_blocks.row_mark(selected, &file.path) {
                Some(mark) => RowContent::Mark(mark),
                None => RowContent::Buttons(row_buttons(
                    files
                        .iter()
                        .filter(|other| other.absolute_line == file.absolute_line)
                        .map(|other| other.state),
                )),
            };
            rows.push((row, file.absolute_line, content));
        }
        rows
    }

    /// Stage / diff / discard buttons at the right end of the selected `git status` block's
    /// file rows.
    pub(crate) fn smart_block_row_buttons(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let (Some(smart_blocks), Some(selected)) = (
            self.smart_blocks.as_ref().map(|(entity, _)| entity.clone()),
            self.selected_block,
        ) else {
            return Vec::new();
        };
        let dimensions = self.terminal.read(cx).last_content.terminal_bounds;
        let line_height = dimensions.line_height();
        self.selected_status_rows(cx)
            .into_iter()
            .map(|(row, absolute_line, content)| {
                let position = point(
                    dimensions.bounds.origin.x + dimensions.width() - dimensions.cell_width(),
                    dimensions.bounds.origin.y + line_height * row as f32,
                );
                let row_element = h_flex()
                    .id(SharedString::from(format!(
                        "smart-block-row-{absolute_line}"
                    )))
                    // The grid underneath must not also see clicks on a button.
                    .occlude()
                    .h(line_height)
                    .gap_1();
                let row_element = match content {
                    RowContent::Mark(mark) => row_element.child(
                        Label::new(match mark {
                            RowMark::Staged => "staged",
                            RowMark::Discarded => "discarded",
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
                    RowContent::Buttons(buttons) => {
                        row_element.children(buttons.into_iter().map(|button| {
                            let smart_blocks = smart_blocks.clone();
                            Button::new(
                                SharedString::from(format!(
                                    "smart-block-row-{absolute_line}-{button:?}"
                                )),
                                button.label(),
                            )
                            .style(ButtonStyle::OutlinedCustom(rgb(BUTTON_BORDER).into()))
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::Small)
                            .on_click(move |_, window, cx| {
                                smart_blocks.update(cx, |smart_blocks, cx| match button {
                                    RowButton::Stage => {
                                        smart_blocks.stage(selected, absolute_line, cx)
                                    }
                                    RowButton::Diff => {
                                        smart_blocks.open_diff(selected, absolute_line, window, cx)
                                    }
                                    RowButton::Discard => {
                                        smart_blocks.discard(selected, absolute_line, window, cx)
                                    }
                                })
                            })
                        }))
                    }
                };
                anchored()
                    .anchor(Anchor::TopRight)
                    .position(position)
                    .child(row_element)
                    .into_any_element()
            })
            .collect()
    }

    /// The commit hash of a `git log` block under `position` (window coordinates, scroll
    /// already applied).
    pub(crate) fn commit_hash_at(
        &self,
        position: Point<Pixels>,
        cx: &App,
    ) -> Option<(BlockId, String)> {
        let smart_blocks = self.enabled_smart_blocks(cx)?;
        let terminal = self.terminal.read(cx);
        let blocks = active_blocks(terminal)?;
        let bounds = terminal.last_content.terminal_bounds;
        let offset = position - bounds.bounds.origin;
        if offset.x < Pixels::ZERO || offset.y < Pixels::ZERO {
            return None;
        }
        let row = (offset.y / bounds.line_height()).floor() as i64;
        let column = (offset.x / bounds.cell_width()).floor() as i64;
        let line = terminal.top_absolute_line() + row;
        let grid_columns = terminal.grid_columns();
        let columns = grid_columns as i64;
        blocks.blocks().find_map(|block| {
            let Some(Insight::GitLog { commits }) =
                smart_blocks.located_insight(block, grid_columns)
            else {
                return None;
            };
            commits.iter().find_map(|commit| {
                // Hash columns count characters of the logical line, which wraps every
                // `columns` cells.
                let index = (line - commit.absolute_line) * columns + column;
                let hit = line >= commit.absolute_line
                    && usize::try_from(index).is_ok_and(|index| commit.columns.contains(&index));
                hit.then(|| (block.id, commit.hash.clone()))
            })
        })
    }

    /// Returns whether a commit was opened, so the click does nothing else.
    pub(crate) fn open_commit_at(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((id, hash)) = self.commit_hash_at(position, cx) else {
            return false;
        };
        let Some(smart_blocks) = self.smart_blocks.as_ref().map(|(entity, _)| entity.clone())
        else {
            return false;
        };
        smart_blocks.update(cx, |smart_blocks, cx| {
            smart_blocks.open_commit(id, &hash, window, cx)
        });
        true
    }

    /// The selected block's buttons, right-aligned on its metadata row (Photon's prompt) or its
    /// header row, placed from the last frame's grid bounds.
    pub(crate) fn smart_block_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let selected = self.selected_block?;
        let smart_blocks = self.enabled_smart_blocks(cx)?;
        let terminal = self.terminal.read(cx);
        let block = active_blocks(terminal)?.block(selected)?;
        let at_prompt = terminal.input_target() == InputTarget::BlockInput;
        let buttons = block_toolbar(block, smart_blocks, at_prompt);
        let delta = self.selected_block_delta(cx);
        if buttons.is_empty() && delta.is_none() {
            return None;
        }
        let local_url = running_url(block, smart_blocks).map(str::to_string);
        let dimensions = terminal.last_content.terminal_bounds;
        let chrome = layout_block_chrome(terminal, Some(selected), None, dimensions.num_lines())
            .into_iter()
            .find(|chrome| chrome.selected)?;
        let row = chrome.metadata_row.unwrap_or(chrome.header_row);
        let line_height = dimensions.line_height();
        let position = point(
            dimensions.bounds.origin.x + dimensions.width() - dimensions.cell_width(),
            dimensions.bounds.origin.y + line_height * row as f32,
        );
        let focus_handle = self.focus_handle.clone();
        Some(
            anchored()
                .anchor(Anchor::TopRight)
                .position(position)
                .child(
                    h_flex()
                        .id("smart-block-toolbar")
                        // The grid underneath must not also see clicks on a button.
                        .occlude()
                        .h(line_height)
                        .gap_1()
                        .children(delta.map(|delta| {
                            Label::new(delta).size(LabelSize::Small).color(Color::Muted)
                        }))
                        .children(buttons.into_iter().map(|button| {
                            let focus_handle = focus_handle.clone();
                            Button::new(
                                SharedString::from(format!("smart-block-{button:?}")),
                                button.label(local_url.as_deref()),
                            )
                            .style(ButtonStyle::OutlinedCustom(rgb(BUTTON_BORDER).into()))
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::Small)
                            .on_click(move |_, window, cx| {
                                focus_handle.dispatch_action(button.action().as_ref(), window, cx)
                            })
                        })),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::rgb;
    use photon_blocks::{
        FileState, SourceLocation, StatusFile, Table, TestFailure, TestRun, TestTool,
    };

    fn run(passed: usize, failed: usize, located: bool) -> Insight {
        Insight::Tests(TestRun {
            tool: TestTool::Cargo,
            passed,
            failed,
            failures: (0..failed)
                .map(|index| TestFailure {
                    name: format!("test_{index}"),
                    location: located.then(|| SourceLocation {
                        path: "src/lib.rs".into(),
                        line: 3,
                        column: None,
                    }),
                })
                .collect(),
            packages: Vec::new(),
        })
    }

    fn build(errors: usize) -> Insight {
        Insight::Build {
            errors: (0..errors)
                .map(|line| SourceLocation {
                    path: "src/main.rs".into(),
                    line: line as u32,
                    column: Some(1),
                })
                .collect(),
        }
    }

    #[test]
    fn summary_chip_texts() {
        let red: Hsla = rgb(0xFF8272).into();
        let green: Hsla = rgb(0xB4FA72).into();
        let accent: Hsla = rgb(0x19AAD8).into();
        let chip = |insight: Option<&Insight>, url| {
            summary_chip(insight, url).map(|(text, color)| (text.to_string(), color))
        };
        assert_eq!(
            chip(Some(&run(41, 2, true)), None),
            Some(("2 failed · 41 passed".to_string(), red))
        );
        assert_eq!(
            chip(Some(&run(7, 0, false)), None),
            Some(("7 passed".to_string(), green))
        );
        assert_eq!(
            chip(Some(&build(1)), None),
            Some(("1 error".to_string(), red))
        );
        assert_eq!(
            chip(Some(&build(3)), None),
            Some(("3 errors".to_string(), red))
        );
        assert_eq!(
            chip(None, Some("http://localhost:5173/")),
            Some(("● localhost:5173".to_string(), accent))
        );
        assert_eq!(chip(Some(&Insight::Json), None), None);
        assert_eq!(chip(None, None), None);

        let delta = |new: usize, fixed: usize| RunDelta {
            new_failures: vec!["a".to_string(); new],
            fixed: vec!["b".to_string(); fixed],
        };
        assert_eq!(
            delta_text(&delta(1, 2)).as_deref(),
            Some("1 new failure · 2 fixed")
        );
        assert_eq!(
            delta_text(&delta(3, 0)).as_deref(),
            Some("3 new failures · 0 fixed")
        );
        assert_eq!(delta_text(&delta(0, 0)), None);
    }

    #[test]
    fn row_buttons_per_state() {
        use FileState::*;
        use RowButton::*;
        assert_eq!(row_buttons([Staged].into_iter()), [Diff]);
        assert_eq!(row_buttons([Unstaged].into_iter()), [Stage, Diff, Discard]);
        assert_eq!(row_buttons([Untracked].into_iter()), [Stage]);
        assert_eq!(
            row_buttons([Staged, Unstaged].into_iter()),
            [Stage, Diff, Discard]
        );
    }

    #[test]
    fn toolbar_buttons_per_insight() {
        use ToolbarButton::*;
        let url = Some("http://localhost:3000");
        assert_eq!(
            toolbar_buttons(Some(&run(1, 2, true)), true, None, false),
            [RerunFailed, OpenErrors]
        );
        assert_eq!(
            toolbar_buttons(Some(&run(1, 2, false)), true, None, false),
            [RerunFailed]
        );
        assert_eq!(
            toolbar_buttons(Some(&run(1, 2, true)), false, None, false),
            [OpenErrors]
        );
        assert_eq!(
            toolbar_buttons(Some(&run(3, 0, false)), false, None, false),
            []
        );
        assert_eq!(
            toolbar_buttons(Some(&build(2)), false, None, false),
            [OpenErrors]
        );
        let status = Insight::GitStatus {
            files: vec![StatusFile {
                absolute_line: 0,
                path: "a.txt".to_string(),
                state: FileState::Unstaged,
            }],
        };
        assert_eq!(
            toolbar_buttons(Some(&status), false, None, false),
            [StageAll, OpenInGitPanel]
        );
        assert_eq!(
            toolbar_buttons(Some(&Insight::Json), false, None, false),
            [OpenAsJson]
        );
        let table = Insight::Table(Table {
            headers: vec!["NAME".to_string()],
            rows: Vec::new(),
        });
        assert_eq!(
            toolbar_buttons(Some(&table), false, None, false),
            [OpenAsTable, CopyAsCsv]
        );
        assert_eq!(
            toolbar_buttons(
                Some(&Insight::GitLog {
                    commits: Vec::new()
                }),
                false,
                None,
                false
            ),
            []
        );
        assert_eq!(toolbar_buttons(None, false, url, true), [OpenUrl, Stop]);
        assert_eq!(toolbar_buttons(None, false, url, false), []);
        assert_eq!(ToolbarButton::OpenUrl.label(url), "Open localhost:3000");
        assert_eq!(ToolbarButton::RerunFailed.label(None), "Re-run failed");
    }
}
