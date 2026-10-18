use editor::{Editor, ToPoint as _};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement as _, Render,
    Styled as _, TextStyleRefinement, WeakEntity, Window, actions, div, prelude::*, px, relative,
    rgb,
};
use terminal::Terminal;

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

/// The command line of a block terminal: an editor whose text is sent to the
/// shell as one command.
pub struct BlockInput {
    editor: Entity<Editor>,
    terminal: WeakEntity<Terminal>,
    /// Index into `history_source`, None while editing a fresh command.
    history_cursor: Option<usize>,
}

impl BlockInput {
    pub fn new(terminal: Entity<Terminal>, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        Self {
            editor,
            terminal: terminal.downgrade(),
            history_cursor: None,
        }
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

    fn run_input(&mut self, _: &RunInput, window: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal.upgrade() else {
            return;
        };
        let text = self.text(cx);
        terminal.update(cx, |terminal, _| {
            // An empty paste would be a no-op for zsh; a bare Enter gives a fresh prompt.
            if text.is_empty() {
                terminal.input(b"\r".as_slice());
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
        let history = self.history_source(cx);
        match history.get(next) {
            Some(command) if cursor_row == 0 => {
                self.history_cursor = Some(next);
                self.set_text(command, window, cx);
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
                    .and_then(|previous| self.history_source(cx).get(previous).cloned())
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

    /// Commands of this session's blocks, newest first.
    // ponytail: session-only history; Task 7 replaces this with the global HistoryStore.
    fn history_source(&self, cx: &App) -> Vec<String> {
        let Some(terminal) = self.terminal.upgrade() else {
            return Vec::new();
        };
        terminal
            .read(cx)
            .blocks()
            .map(|blocks| {
                blocks
                    .blocks()
                    .rev()
                    .map(|block| block.command.clone())
                    .filter(|command| !command.is_empty())
                    .collect()
            })
            .unwrap_or_default()
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
            .on_action(cx.listener(Self::run_input))
            .on_action(cx.listener(Self::history_previous))
            .on_action(cx.listener(Self::history_next))
            .flex()
            .flex_none()
            .gap(px(6.))
            .pt(px(10.))
            .px(px(14.))
            .pb(px(12.))
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
