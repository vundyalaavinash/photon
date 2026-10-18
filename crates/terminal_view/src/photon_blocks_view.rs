use std::{ops::Range, time::Duration};

use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, Focusable as _, FontWeight, Hsla, KeyDownEvent,
    Pixels, Point, SharedString, Stateful, Task, TextRun, TextStyle, Window, actions, fill, point,
    px, rgb, rgba, size,
};
use photon_blocks::{Block, BlockId, BlockList, Phase, VisibleBlock, visible_blocks};
use photon_terminal::BlockInput;
use terminal::{BlockPart, InputTarget, Modes, Terminal, TerminalBounds};
use ui::prelude::*;
use util::{ResultExt as _, paths::PathExt as _};
use workspace::searchable::SearchEvent;

use crate::{TerminalMode, TerminalView};

actions!(
    photon_terminal,
    [
        /// Selects the command block above the selected one, or the newest block.
        SelectPreviousBlock,
        /// Selects the command block below the selected one.
        SelectNextBlock,
        /// Copies the selected block's command.
        CopyBlockCommand,
        /// Copies the selected block's output.
        CopyBlockOutput,
        /// Copies the selected block's command and output.
        CopyBlock,
        /// Runs the selected block's command again.
        RerunBlock,
    ]
);

const SELECTED_EDGE: u32 = 0x19AAD8;
const SELECTED_TINT: u32 = 0x19AAD814;
const FAILED_EDGE: u32 = 0xFF8272;
const SEPARATOR: u32 = 0x1a1a1a;
const METADATA: u32 = 0x8E8E8E;
const EDGE_WIDTH: Pixels = px(3.);

pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

/// Full-screen apps and terminals that fell back look exactly like classic terminals.
pub(crate) fn active_blocks(terminal: &Terminal) -> Option<&BlockList> {
    let blocks = terminal.blocks()?;
    let degraded = matches!(blocks.phase(), Phase::Degraded(_));
    let alt_screen = terminal.last_content.mode.contains(Modes::ALT_SCREEN);
    (!degraded && !alt_screen).then_some(blocks)
}

/// Screen rows a block covers, clipped to the screen. `end_line` is exclusive; a running
/// block reaches the bottom row.
fn block_rows(visible: &VisibleBlock, block: &Block, screen_rows: i32) -> Range<i32> {
    let end = if block.end_line.is_some() {
        visible.last_row
    } else {
        visible.last_row.saturating_add(1)
    };
    visible.first_row.max(0)..end.min(screen_rows)
}

pub(crate) struct BlockChrome {
    rows: Range<i32>,
    header_row: i32,
    /// Prompt rows after the first (multi-line commands), which the header replaces.
    hidden_rows: Range<i32>,
    starts_on_screen: bool,
    edge: Option<Hsla>,
    selected: bool,
    command: SharedString,
    metadata: SharedString,
}

pub(crate) fn layout_block_chrome(
    terminal: &Terminal,
    selected: Option<BlockId>,
    screen_rows: usize,
) -> Vec<BlockChrome> {
    let Some(blocks) = active_blocks(terminal) else {
        return Vec::new();
    };
    let screen_rows = i32::try_from(screen_rows).unwrap_or(i32::MAX);
    let top_absolute_line = terminal.top_absolute_line();
    visible_blocks(blocks, top_absolute_line, screen_rows)
        .iter()
        .filter_map(|visible| {
            let block = blocks.block(visible.id)?;
            let rows = block_rows(visible, block, screen_rows);
            let header_row = visible.header_row.unwrap_or(visible.first_row);
            let output_row = block.output_line.map_or(header_row, |line| {
                i32::try_from(line - top_absolute_line).unwrap_or(i32::MAX)
            });
            let is_selected = selected == Some(block.id);
            let failed = block.end_line.is_some() && block.exit_code != Some(0);
            let edge = if failed {
                Some(rgb(FAILED_EDGE).into())
            } else if is_selected {
                Some(rgb(SELECTED_EDGE).into())
            } else {
                None
            };
            let metadata = [
                block
                    .cwd
                    .as_ref()
                    .map(|cwd| cwd.compact().to_string_lossy().into_owned()),
                block.branch.clone(),
                block.duration.map(format_duration),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("  ");
            Some(BlockChrome {
                hidden_rows: (header_row + 1).max(0)..output_row.min(rows.end),
                rows,
                header_row,
                starts_on_screen: visible.first_row > 0,
                edge,
                selected: is_selected,
                command: block.command.clone().into(),
                metadata: metadata.into(),
            })
        })
        .collect()
}

pub(crate) fn paint_block_chrome(
    chrome: &[BlockChrome],
    origin: Point<Pixels>,
    dimensions: &TerminalBounds,
    background: Hsla,
    text_style: &TextStyle,
    window: &mut Window,
    cx: &mut App,
) {
    let line_height = dimensions.line_height();
    // The edge sits in the one-cell gutter left of the grid.
    let left = origin.x - dimensions.cell_width();
    let full_width = dimensions.width() + dimensions.cell_width();
    let row_bounds = |rows: &Range<i32>| {
        Bounds::new(
            point(left, origin.y + line_height * rows.start as f32),
            size(
                full_width,
                line_height * (rows.end - rows.start).max(0) as f32,
            ),
        )
    };
    let font_size = text_style.font_size.to_pixels(window.rem_size());

    for block in chrome {
        window.paint_quad(fill(
            row_bounds(&(block.header_row..block.header_row + 1)),
            background,
        ));
        if !block.hidden_rows.is_empty() {
            window.paint_quad(fill(row_bounds(&block.hidden_rows), background));
        }
        let block_bounds = row_bounds(&block.rows);
        if block.selected {
            window.paint_quad(fill(block_bounds, rgba(SELECTED_TINT)));
        }
        if let Some(edge) = block.edge {
            window.paint_quad(fill(
                Bounds::new(
                    block_bounds.origin,
                    size(EDGE_WIDTH, block_bounds.size.height),
                ),
                edge,
            ));
        }
        if block.starts_on_screen {
            window.paint_quad(fill(
                Bounds::new(block_bounds.origin, size(full_width, px(1.))),
                rgb(SEPARATOR),
            ));
        }

        let header_y = origin.y + line_height * block.header_row as f32;
        let mut command_font = text_style.font();
        command_font.weight = FontWeight::BOLD;
        let command = window.text_system().shape_line(
            block.command.clone(),
            font_size,
            &[TextRun {
                len: block.command.len(),
                font: command_font,
                color: text_style.color,
                ..Default::default()
            }],
            None,
        );
        command
            .paint(
                point(origin.x, header_y),
                line_height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            )
            .log_err();
        if !block.metadata.is_empty() {
            let metadata = window.text_system().shape_line(
                block.metadata.clone(),
                font_size,
                &[TextRun {
                    len: block.metadata.len(),
                    font: text_style.font(),
                    color: rgb(METADATA).into(),
                    ..Default::default()
                }],
                None,
            );
            let metadata_x =
                origin.x + dimensions.width() - dimensions.cell_width() - metadata.width;
            metadata
                .paint(
                    point(metadata_x, header_y),
                    line_height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                )
                .log_err();
        }
    }
}

pub(crate) fn register_block_actions(
    element: Stateful<Div>,
    cx: &mut Context<TerminalView>,
) -> Stateful<Div> {
    element
        .on_action(cx.listener(TerminalView::select_previous_block))
        .on_action(cx.listener(TerminalView::select_next_block))
        .on_action(cx.listener(|this, _: &CopyBlockCommand, _, cx| {
            this.copy_block_part(BlockPart::Command, cx)
        }))
        .on_action(cx.listener(|this, _: &CopyBlockOutput, _, cx| {
            this.copy_block_part(BlockPart::Output, cx)
        }))
        .on_action(
            cx.listener(|this, _: &CopyBlock, _, cx| this.copy_block_part(BlockPart::Both, cx)),
        )
        .on_action(cx.listener(TerminalView::rerun_block))
}

impl TerminalView {
    pub fn selected_block(&self) -> Option<BlockId> {
        self.selected_block
    }

    fn select_previous_block(
        &mut self,
        _: &SelectPreviousBlock,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(blocks) = active_blocks(self.terminal.read(cx)) else {
            return;
        };
        let previous = match self.selected_block {
            Some(selected) => blocks
                .blocks()
                .rev()
                .skip_while(|block| block.id != selected)
                .nth(1)
                .map(|block| block.id)
                .or(Some(selected)),
            None => blocks.blocks().next_back().map(|block| block.id),
        };
        self.selected_block = previous;
        cx.emit(SearchEvent::MatchesInvalidated);
        cx.notify();
    }

    /// Moving past the newest block clears the selection.
    fn select_next_block(&mut self, _: &SelectNextBlock, _: &mut Window, cx: &mut Context<Self>) {
        let Some(blocks) = active_blocks(self.terminal.read(cx)) else {
            return;
        };
        self.selected_block = self.selected_block.and_then(|selected| {
            blocks
                .blocks()
                .skip_while(|block| block.id != selected)
                .nth(1)
                .map(|block| block.id)
        });
        cx.emit(SearchEvent::MatchesInvalidated);
        cx.notify();
    }

    fn copy_block_part(&mut self, part: BlockPart, cx: &mut Context<Self>) {
        let Some(selected) = self.selected_block else {
            return;
        };
        if let Some(text) = self.terminal.read(cx).block_text(selected, part) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn rerun_block(&mut self, _: &RerunBlock, _: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.selected_block else {
            return;
        };
        if self.read_only {
            return;
        }
        let Some(command) = self
            .terminal
            .read(cx)
            .blocks()
            .and_then(|blocks| blocks.block(selected))
            .map(|block| block.command.clone())
        else {
            return;
        };
        self.terminal
            .update(cx, |terminal, cx| terminal.run_command(&command, cx));
    }

    pub(crate) fn select_block_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let terminal = self.terminal.read(cx);
        let Some(blocks) = active_blocks(terminal) else {
            return;
        };
        let bounds = terminal.last_content.terminal_bounds;
        let row = ((position.y - bounds.bounds.origin.y) / bounds.line_height()).floor() as i32;
        let screen_rows = i32::try_from(bounds.num_lines()).unwrap_or(i32::MAX);
        let clicked = visible_blocks(blocks, terminal.top_absolute_line(), screen_rows)
            .iter()
            .find(|visible| {
                blocks
                    .block(visible.id)
                    .is_some_and(|block| block_rows(visible, block, screen_rows).contains(&row))
            })
            .map(|visible| visible.id);
        if clicked != self.selected_block {
            self.selected_block = clicked;
            cx.emit(SearchEvent::MatchesInvalidated);
            cx.notify();
        }
    }

    /// Keeps only the matches inside the selected block; all matches when none is selected.
    pub(crate) fn scope_find_to_selected_block(
        &self,
        matches: Task<Vec<terminal::Range>>,
        cx: &mut Context<Self>,
    ) -> Task<Vec<terminal::Range>> {
        let terminal = self.terminal.read(cx);
        let lines = self
            .selected_block
            .and_then(|selected| terminal.block_grid_lines(selected));
        let Some(lines) = lines else {
            return matches;
        };
        cx.background_spawn(async move {
            let mut matches = matches.await;
            matches.retain(|found| {
                lines.contains(&found.start().line) && lines.contains(&found.end().line)
            });
            matches
        })
    }

    /// Drops the selection once its block is trimmed or cleared away.
    pub(crate) fn sync_selected_block(&mut self, cx: &App) {
        if let Some(selected) = self.selected_block
            && active_blocks(self.terminal.read(cx))
                .and_then(|blocks| blocks.block(selected))
                .is_none()
        {
            self.selected_block = None;
        }
    }
}

impl TerminalView {
    pub(crate) fn new_block_input(
        terminal: &Entity<Terminal>,
        workspace: gpui::WeakEntity<workspace::Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<BlockInput>> {
        terminal.read(cx).blocks()?;
        let terminal = terminal.clone();
        Some(cx.new(|cx| BlockInput::new(terminal, workspace, window, cx)))
    }

    /// The input's slot below the grid, kept while blocks are drawn so the grid size does
    /// not change between commands.
    pub(crate) fn block_input_slot(&self, cx: &App) -> Option<impl IntoElement + use<>> {
        let shown = self.block_input.is_some()
            && !self.read_only
            && !matches!(self.mode, TerminalMode::Embedded { .. })
            && active_blocks(self.terminal.read(cx)).is_some();
        shown.then(|| photon_terminal::input_slot(self.visible_block_input(cx)))
    }

    /// The input box, shown only while the shell waits at a block prompt.
    pub(crate) fn visible_block_input(&self, cx: &App) -> Option<Entity<BlockInput>> {
        let shown = !self.read_only
            && !matches!(self.mode, TerminalMode::Embedded { .. })
            && self.terminal.read(cx).input_target() == InputTarget::BlockInput;
        self.block_input.clone().filter(|_| shown)
    }

    /// While the input is shown, focus given to the grid (tab switch, pane activation, a
    /// click) moves on to the input, so keys and pastes never reach zsh's line buffer.
    pub(crate) fn redirect_grid_focus_to_block_input(
        focus_handle: &gpui::FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Subscription {
        cx.on_focus(focus_handle, window, |this, window, cx| {
            if let Some(input) = this.visible_block_input(cx) {
                window.focus(&input.focus_handle(cx), cx);
            }
        })
    }

    pub(crate) fn block_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.block_input
            .as_ref()
            .is_some_and(|input| input.focus_handle(cx).contains_focused(window, cx))
    }

    /// Moves focus between the grid and the input box as the input appears or hides,
    /// but only within this terminal so other panes keep their focus.
    pub(crate) fn sync_block_input_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.block_input.clone() else {
            return;
        };
        let shown = self.visible_block_input(cx).is_some();
        if shown == self.block_input_shown {
            return;
        }
        self.block_input_shown = shown;
        if shown && self.focus_handle.is_focused(window) {
            window.focus(&input.focus_handle(cx), cx);
        } else if !shown && self.block_input_focused(window, cx) {
            window.focus(&self.focus_handle, cx);
        }
    }

    /// Whether a key reaching the grid belongs to the input box instead of the PTY.
    /// Printable keys typed on the grid at a prompt move into the input box.
    pub(crate) fn block_input_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(input) = self.visible_block_input(cx) else {
            return false;
        };
        if self.block_input_focused(window, cx) {
            return true;
        }
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let Some(text) = keystroke.key_char.as_deref().filter(|text| {
            !modifiers.control
                && !modifiers.platform
                && !modifiers.function
                && !text.chars().any(char::is_control)
        }) else {
            return false;
        };
        window.focus(&input.focus_handle(cx), cx);
        input.update(cx, |input, cx| input.insert(text, window, cx));
        cx.stop_propagation();
        true
    }
}
