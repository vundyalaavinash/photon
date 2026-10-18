use editor::{Editor, ToPoint as _};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, TextStyleRefinement, WeakEntity, Window, actions, div, prelude::*,
    px, relative, rgb,
};
use photon_blocks::BlockEvent;
use terminal::{Event, Terminal};
use util::ResultExt as _;
use workspace::Workspace;

use crate::history::{ClearHistory, HistoryEntry, HistoryPicker, HistoryStore, SearchHistory};

actions!(
    photon_terminal,
    [
        /// Runs the command typed in the terminal's input box.
        RunInput,
        /// Replaces the input with the previous command from history.
        HistoryPrevious,
        /// Replaces the input with the next command from history.
        HistoryNext,
    ]
);

const MAX_LINES: usize = 10;
const FONT_FAMILY: &str = "Hack";
const FONT_SIZE: f32 = 13.;
const LINE_HEIGHT: f32 = 1.2;
const METADATA_COLOR: u32 = 0x8E8E8E;
const BORDER_COLOR: u32 = 0x222222;
const BACKGROUND_COLOR: u32 = 0x0a0a0a;
const PADDING_TOP: f32 = 10.;
const PADDING_BOTTOM: f32 = 12.;
const BORDER_WIDTH: f32 = 1.;

/// App startup hook: creates the global command history and registers its actions.
pub fn init(cx: &mut App) {
    crate::history::HistoryStore::init_global(crate::history::default_histfile(), cx);
    cx.on_action(|_: &ClearHistory, cx| {
        let store = HistoryStore::global(cx);
        store.update(cx, |store, cx| store.clear(cx));
    });
}

/// The space below the grid while blocks are active. It always reserves one input line,
/// empty while a command runs, so the grid (and the PTY size) never changes as commands
/// start and finish; extra input lines grow upward over the grid.
pub fn input_slot(input: Option<Entity<BlockInput>>) -> impl IntoElement {
    let height = BORDER_WIDTH + PADDING_TOP + FONT_SIZE * LINE_HEIGHT + PADDING_BOTTOM;
    div()
        .relative()
        .flex_none()
        .w_full()
        .h(px(height))
        .border_t_1()
        .border_color(rgb(BORDER_COLOR))
        .bg(rgb(BACKGROUND_COLOR))
        .when_some(input, |this, input| {
            this.child(div().absolute().left_0().right_0().bottom_0().child(input))
        })
}

/// The command line of a block terminal: an editor whose text is sent to the
/// shell as one command.
pub struct BlockInput {
    editor: Entity<Editor>,
    terminal: WeakEntity<Terminal>,
    /// Index into the store's recent commands, None while editing a fresh command.
    history_cursor: Option<usize>,
    history: Entity<HistoryStore>,
    workspace: WeakEntity<Workspace>,
    _terminal_subscription: Subscription,
}

impl BlockInput {
    pub fn new(
        terminal: Entity<Terminal>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, MAX_LINES, window, cx);
            editor.set_show_gutter(false, cx);
            editor.set_text_style_refinement(TextStyleRefinement {
                font_family: Some(FONT_FAMILY.into()),
                font_size: Some(px(FONT_SIZE).into()),
                line_height: Some(relative(LINE_HEIGHT)),
                ..Default::default()
            });
            editor
        });
        let _terminal_subscription = cx.subscribe(&terminal, Self::record_finished_block);
        Self {
            editor,
            terminal: terminal.downgrade(),
            history_cursor: None,
            history: HistoryStore::global(cx),
            workspace,
            _terminal_subscription,
        }
    }

    fn record_finished_block(
        &mut self,
        terminal: Entity<Terminal>,
        event: &Event,
        cx: &mut Context<Self>,
    ) {
        let Event::BlocksChanged(Some(BlockEvent::Finished(id))) = event else {
            return;
        };
        let entry = terminal
            .read(cx)
            .blocks()
            .and_then(|blocks| blocks.block(*id))
            .and_then(HistoryEntry::from_block);
        if let Some(entry) = entry {
            self.history
                .update(cx, |history, cx| history.record(entry, cx));
        }
    }

    fn search_history(&mut self, _: &SearchHistory, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.weak_entity();
        let store = self.history.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    HistoryPicker::new(input, store, window, cx)
                });
            })
            .log_err();
    }

    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).text(cx)
    }

    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            editor.move_to_end(&editor::actions::MoveToEnd, window, cx);
        });
    }

    pub fn insert(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.insert(text, window, cx));
    }

    /// With nothing selected in the input, copy takes the grid's selection instead.
    fn copy(&mut self, _: &editor::actions::Copy, _: &mut Window, cx: &mut Context<Self>) {
        let has_selection = self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.has_non_empty_selection(&snapshot)
        });
        if has_selection {
            return;
        }
        if let Some(terminal) = self.terminal.upgrade() {
            // The copy happens on the terminal's next sync, which the view's re-render runs.
            terminal.update(cx, |terminal, cx| {
                terminal.copy(None);
                cx.notify();
            });
            cx.stop_propagation();
        }
    }

    fn run_input(&mut self, _: &RunInput, window: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal.upgrade() else {
            return;
        };
        let text = self.text(cx);
        terminal.update(cx, |terminal, _| {
            // An empty paste would be a no-op for zsh; a bare Enter gives a fresh prompt.
            // Ctrl-U first, as in `run_command`, so typeahead is not run.
            if text.is_empty() {
                terminal.input(b"\x15\r".as_slice());
            } else {
                terminal.run_command(&text);
            }
        });
        self.history_cursor = None;
        self.set_text("", window, cx);
    }

    fn history_previous(
        &mut self,
        _: &HistoryPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (cursor_row, _) = self.cursor_row_and_last_row(cx);
        let next = self.history_cursor.map_or(0, |index| index + 1);
        let history = self.history.read(cx).recent(next + 1);
        match history.get(next) {
            Some(command) if cursor_row == 0 => {
                self.history_cursor = Some(next);
                self.set_text(&command.command, window, cx);
            }
            // Lets the editor's own binding (cursor up) handle the key.
            _ => cx.propagate(),
        }
    }

    fn history_next(&mut self, _: &HistoryNext, window: &mut Window, cx: &mut Context<Self>) {
        let (cursor_row, last_row) = self.cursor_row_and_last_row(cx);
        match self.history_cursor {
            Some(index) if cursor_row == last_row => {
                let previous = index.checked_sub(1);
                let text = previous
                    .and_then(|previous| self.history.read(cx).recent(previous + 1).pop())
                    .map(|entry| entry.command)
                    .unwrap_or_default();
                self.history_cursor = previous;
                self.set_text(&text, window, cx);
            }
            _ => cx.propagate(),
        }
    }

    fn cursor_row_and_last_row(&self, cx: &App) -> (u32, u32) {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let cursor_row = editor
            .selections
            .newest_anchor()
            .head()
            .to_point(&snapshot)
            .row;
        (cursor_row, snapshot.max_point().row)
    }
}

impl Focusable for BlockInput {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for BlockInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("PhotonBlockInput")
            .capture_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::run_input))
            .on_action(cx.listener(Self::history_previous))
            .on_action(cx.listener(Self::history_next))
            .on_action(cx.listener(Self::search_history))
            .flex()
            .flex_none()
            .gap(px(6.))
            .pt(px(PADDING_TOP))
            .px(px(14.))
            .pb(px(PADDING_BOTTOM))
            .border_t_1()
            .border_color(rgb(BORDER_COLOR))
            .bg(rgb(BACKGROUND_COLOR))
            .child(
                div()
                    .font_family(FONT_FAMILY)
                    .text_size(px(FONT_SIZE))
                    .line_height(relative(LINE_HEIGHT))
                    .text_color(rgb(METADATA_COLOR))
                    .child("❯"),
            )
            .child(div().flex_1().child(self.editor.clone()))
    }
}
