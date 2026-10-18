use editor::{Editor, ToPoint as _};
use std::path::PathBuf;

use gpui::{
    Action as _, App, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement as _,
    Render, Styled as _, Subscription, Task, TextStyleRefinement, WeakEntity, Window, actions, div,
    prelude::*, px, relative, rgb,
};
use photon_blocks::BlockEvent;
use terminal::{Event, Terminal};
use util::ResultExt as _;
use workspace::Workspace;

use crate::chips::{ChipPicker, GitInfo, branch_chip_text, folder_chip_text, git_info};
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
        /// Opens the picker for changing the terminal's directory.
        ToggleFolderPicker,
        /// Opens the picker for switching the git branch.
        ToggleBranchPicker,
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
const CHIPS_HEIGHT: f32 = 22.;
const CHIPS_GAP: f32 = 8.;
const CHIP_BORDER_COLOR: u32 = 0x333333;
const FOLDER_CHIP_COLOR: u32 = 0xA5D5FE;
const BRANCH_CHIP_COLOR: u32 = 0xB4FA72;

/// App startup hook: creates the global command history and registers its actions.
pub fn init(cx: &mut App) {
    crate::history::HistoryStore::init_global(crate::history::default_histfile(), cx);
    cx.on_action(|_: &ClearHistory, cx| {
        let store = HistoryStore::global(cx);
        store.update(cx, |store, cx| store.clear(cx));
    });
}

/// The space below the grid while blocks are active. It always reserves one input line,
/// empty while a command runs, plus the chips row above it, so the grid (and the PTY size)
/// never changes as commands start and finish or chips come and go; extra input lines grow
/// upward over the grid.
pub fn input_slot(input: Option<Entity<BlockInput>>) -> impl IntoElement {
    let height = BORDER_WIDTH
        + PADDING_TOP
        + CHIPS_HEIGHT
        + CHIPS_GAP
        + FONT_SIZE * LINE_HEIGHT
        + PADDING_BOTTOM;
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
    /// The cwd git was last queried for.
    queried_cwd: Option<PathBuf>,
    git: Option<GitInfo>,
    /// Replacing this cancels an older, now stale, query.
    git_task: Task<()>,
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
        let mut this = Self {
            editor,
            terminal: terminal.downgrade(),
            history_cursor: None,
            history: HistoryStore::global(cx),
            workspace,
            queried_cwd: None,
            git: None,
            git_task: Task::ready(()),
            _terminal_subscription,
        };
        this.refresh_git(true, cx);
        this
    }

    fn refresh_git(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal.upgrade() else {
            return;
        };
        let cwd = terminal
            .read(cx)
            .blocks()
            .and_then(|blocks| blocks.current_cwd().map(PathBuf::from));
        if !force && cwd == self.queried_cwd {
            return;
        }
        self.queried_cwd = cwd.clone();
        let Some(cwd) = cwd else {
            self.git = None;
            self.stamp_branch(cx);
            return;
        };
        let task = git_info(&cwd, cx);
        self.git_task = cx.spawn(async move |this, cx| {
            let info = task.await;
            this.update(cx, |this, cx| {
                this.git = info;
                this.stamp_branch(cx);
                cx.notify();
            })
            .log_err();
        });
    }

    fn stamp_branch(&self, cx: &mut Context<Self>) {
        let branch = self.git.as_ref().map(|info| info.branch.clone());
        self.terminal
            .update(cx, |terminal, _| terminal.set_block_branch(branch))
            .log_err();
    }

    fn toggle_folder_picker(
        &mut self,
        _: &ToggleFolderPicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.queried_cwd.clone();
        let recent = self.history.read(cx).recent_dirs(50);
        let terminal = self.terminal.clone();
        let workspace = self.workspace.clone();
        self.workspace
            .update(cx, |workspace_ref, cx| {
                workspace_ref.toggle_modal(window, cx, |window, cx| {
                    ChipPicker::folders(terminal, workspace, current, recent, window, cx)
                });
            })
            .log_err();
    }

    fn toggle_branch_picker(
        &mut self,
        _: &ToggleBranchPicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(info) = self.git.clone() else {
            return;
        };
        let terminal = self.terminal.clone();
        let workspace = self.workspace.clone();
        self.workspace
            .update(cx, |workspace_ref, cx| {
                workspace_ref.toggle_modal(window, cx, |window, cx| {
                    ChipPicker::branches(terminal, workspace, info, window, cx)
                });
            })
            .log_err();
    }

    fn record_finished_block(
        &mut self,
        terminal: Entity<Terminal>,
        event: &Event,
        cx: &mut Context<Self>,
    ) {
        let Event::BlocksChanged(change) = event else {
            return;
        };
        let finished = matches!(change, Some(BlockEvent::Finished(_)));
        self.refresh_git(finished, cx);
        let Some(BlockEvent::Finished(id)) = change else {
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
            .on_action(cx.listener(Self::toggle_folder_picker))
            .on_action(cx.listener(Self::toggle_branch_picker))
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(CHIPS_GAP))
            .pt(px(PADDING_TOP))
            .px(px(14.))
            .pb(px(PADDING_BOTTOM))
            .border_t_1()
            .border_color(rgb(BORDER_COLOR))
            .bg(rgb(BACKGROUND_COLOR))
            .child(
                div()
                    .flex()
                    .h(px(CHIPS_HEIGHT))
                    .gap(px(6.))
                    .when_some(self.queried_cwd.as_ref(), |this, cwd| {
                        this.child(chip(
                            "photon-folder-chip",
                            folder_chip_text(cwd),
                            FOLDER_CHIP_COLOR,
                            ToggleFolderPicker.boxed_clone(),
                        ))
                    })
                    .when_some(self.git.as_ref(), |this, info| {
                        this.child(chip(
                            "photon-branch-chip",
                            branch_chip_text(info),
                            BRANCH_CHIP_COLOR,
                            ToggleBranchPicker.boxed_clone(),
                        ))
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .font_family(FONT_FAMILY)
                            .text_size(px(FONT_SIZE))
                            .line_height(relative(LINE_HEIGHT))
                            .text_color(rgb(METADATA_COLOR))
                            .child("❯"),
                    )
                    .child(div().flex_1().child(self.editor.clone())),
            )
    }
}

fn chip(
    id: &'static str,
    text: String,
    color: u32,
    action: Box<dyn gpui::Action>,
) -> impl IntoElement {
    div()
        .id(id)
        .flex_none()
        .px(px(7.))
        .border_1()
        .border_color(rgb(CHIP_BORDER_COLOR))
        .rounded(px(4.))
        .text_size(px(12.))
        .text_color(rgb(color))
        .cursor_pointer()
        .hover(|style| style.border_color(rgb(0x19AAD8)))
        .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
        .child(text)
}
