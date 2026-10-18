use editor::{
    CompletionProvider, Editor, EditorEvent, Inlay, MultiBufferOffset, SelectionEffects,
    ToOffset as _, ToPoint as _,
};
use language::{Buffer, CodeLabel};
use project::{
    CompletionDisplayOptions, CompletionResponse, CompletionSource, InlayId,
    lsp_store::CompletionDocumentation,
};
use std::{cell::RefCell, path::PathBuf, rc::Rc};

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
use crate::completion::{Completion, ZshCompleter, word_start};
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
        /// Completes the word before the cursor the way zsh would.
        Complete,
        /// Accepts the grey history suggestion, or moves right when there is none.
        AcceptSuggestion,
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
// Far above the ids the editor hands out for its own predictions.
const SUGGESTION_INLAY_ID: usize = usize::MAX;
const SUGGESTION_INLAY: InlayId = InlayId::EditPrediction(SUGGESTION_INLAY_ID);

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
    completer: ZshCompleter,
    /// Set while a Tab is waiting on the helper; further Tabs are ignored until it answers.
    completion_task: Option<Task<()>>,
    /// What the editor's completion menu lists: the word's start offset and its candidates.
    menu_items: Rc<RefCell<Option<(usize, Vec<Completion>)>>>,
    /// The rest of a history command, shown in grey after the cursor.
    suggestion: Option<String>,
    _terminal_subscription: Subscription,
    _editor_subscription: Subscription,
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
        let menu_items = Rc::default();
        editor.update(cx, |editor, _| {
            editor.set_completion_provider(Some(Rc::new(ShellCompletionProvider {
                items: Rc::clone(&menu_items),
            })));
            editor.set_show_completions_on_input(Some(false));
        });
        let _terminal_subscription = cx.subscribe(&terminal, Self::record_finished_block);
        let _editor_subscription = cx.subscribe(&editor, |this, editor, event, cx| {
            if matches!(
                event,
                EditorEvent::BufferEdited | EditorEvent::SelectionsChanged { .. }
            ) {
                // Candidates are only valid for the word they were fetched for; once their
                // menu is closed, an edit or a cursor move makes them stale (ctrl-space
                // would otherwise reopen them over the wrong range).
                if !editor.read(cx).has_visible_completions_menu() {
                    this.menu_items.borrow_mut().take();
                }
                this.refresh_suggestion(cx);
            }
        });
        let mut this = Self {
            editor,
            terminal: terminal.downgrade(),
            history_cursor: None,
            history: HistoryStore::global(cx),
            workspace,
            queried_cwd: None,
            git: None,
            git_task: Task::ready(()),
            completer: ZshCompleter::new(&photon_blocks::zsh_integration_dir(paths::data_dir())),
            completion_task: None,
            menu_items,
            suggestion: None,
            _terminal_subscription,
            _editor_subscription,
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

    pub fn suggestion(&self) -> Option<&str> {
        self.suggestion.as_deref()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_completer_for_test(&mut self, completer: ZshCompleter) {
        self.completer = completer;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn completion_menu_visible(&self, cx: &App) -> bool {
        self.editor.read(cx).has_visible_completions_menu()
    }

    fn text_and_cursor(&self, cx: &App) -> (String, usize) {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let cursor = editor
            .selections
            .newest_anchor()
            .head()
            .to_offset(&snapshot);
        (snapshot.text(), cursor.0)
    }

    fn refresh_suggestion(&mut self, cx: &mut Context<Self>) {
        let (text, cursor) = self.text_and_cursor(cx);
        let suggestion = if cursor == text.len() {
            self.history.read(cx).suggestion(&text)
        } else {
            None
        };
        if suggestion == self.suggestion {
            return;
        }
        self.suggestion = suggestion.clone();
        self.editor.update(cx, |editor, cx| {
            let end = editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .anchor_after(MultiBufferOffset(text.len()));
            let inlays = suggestion
                .map(|suggestion| Inlay::edit_prediction(SUGGESTION_INLAY_ID, end, suggestion))
                .into_iter()
                .collect();
            editor.splice_inlays(&[SUGGESTION_INLAY], inlays, cx);
        });
    }

    fn accept_suggestion(
        &mut self,
        _: &AcceptSuggestion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.suggestion.clone() {
            Some(suggestion) => self.insert(&suggestion, window, cx),
            // Lets the editor's own binding (cursor right) handle the key.
            None => cx.propagate(),
        }
    }

    fn complete(&mut self, _: &Complete, window: &mut Window, cx: &mut Context<Self>) {
        if self.completion_task.is_some() {
            return;
        }
        let Some(terminal) = self.terminal.upgrade() else {
            return;
        };
        let cwd = terminal
            .read(cx)
            .blocks()
            .and_then(|blocks| blocks.current_cwd().map(PathBuf::from))
            .unwrap_or_else(|| util::paths::home_dir().clone());
        let (text, cursor) = self.text_and_cursor(cx);
        let Some(before) = text.get(..cursor).map(str::to_string) else {
            return;
        };
        let task = self.completer.complete(&before, &cwd, cx);
        self.completion_task = Some(cx.spawn_in(window, async move |this, cx| {
            let completions = task.await;
            this.update_in(cx, |this, window, cx| {
                this.completion_task = None;
                match completions {
                    Ok(completions) => this.apply_completions(&before, completions, window, cx),
                    Err(error) => log::error!("tab completion failed: {error:#}"),
                }
            })
            .log_err();
        }));
    }

    fn apply_completions(
        &mut self,
        before: &str,
        completions: Vec<Completion>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (text, cursor) = self.text_and_cursor(cx);
        // Typed or moved while the helper was thinking: the answer is for another word.
        if text.get(..cursor) != Some(before) {
            return;
        }
        let start = word_start(before);
        *self.menu_items.borrow_mut() = None;
        match completions.as_slice() {
            [] => {}
            [only] => {
                let mut insert = only.insert.clone();
                if !insert.ends_with('/') {
                    insert.push(' ');
                }
                self.editor.update(cx, |editor, cx| {
                    editor.change_selections(
                        SelectionEffects::no_scroll(),
                        window,
                        cx,
                        |selections| {
                            selections.select_ranges([
                                MultiBufferOffset(start)..MultiBufferOffset(cursor)
                            ])
                        },
                    );
                    editor.insert(&insert, window, cx);
                });
            }
            _ => {
                *self.menu_items.borrow_mut() = Some((start, completions));
                self.editor.update(cx, |editor, cx| {
                    editor.show_completions(&editor::actions::ShowCompletions, window, cx)
                });
            }
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
            .on_action(cx.listener(Self::complete))
            .on_action(cx.listener(Self::accept_suggestion))
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

/// Feeds the editor's completion menu with the candidates the last Tab produced.
struct ShellCompletionProvider {
    items: Rc<RefCell<Option<(usize, Vec<Completion>)>>>,
}

impl CompletionProvider for ShellCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        _: editor::CompletionContext,
        _: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let Some((start, items)) = self.items.borrow().clone() else {
            return Task::ready(Ok(Vec::new()));
        };
        let buffer = buffer.read(cx);
        let start = buffer.anchor_before(start.min(buffer.len()));
        let completions = items
            .into_iter()
            .map(|item| project::Completion {
                replace_range: start..buffer_position,
                label: CodeLabel::plain(item.insert.clone(), None),
                new_text: item.insert,
                documentation: item
                    .description
                    .map(|description| CompletionDocumentation::SingleLine(description.into())),
                source: CompletionSource::Custom,
                icon_path: None,
                icon_color: None,
                match_start: None,
                snippet_deduplication_key: None,
                insert_text_mode: None,
                confirm: None,
                group: None,
            })
            .collect();
        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions::default(),
            is_incomplete: false,
        }]))
    }

    // Typing while the menu is open re-filters the same candidates; with the menu closed
    // the editor never asks, since completions-on-input is off.
    fn is_completion_trigger(
        &self,
        _: &Entity<Buffer>,
        _: language::Anchor,
        _: &str,
        _: bool,
        _: &mut Context<Editor>,
    ) -> bool {
        self.items.borrow().is_some()
    }

    fn sort_completions(&self) -> bool {
        false
    }
}
