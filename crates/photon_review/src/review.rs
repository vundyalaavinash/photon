use std::collections::{HashMap, HashSet};
use std::ops::{Range, RangeInclusive};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use buffer_diff::BufferDiff;
use editor::{
    Anchor, Editor, EditorEvent, ExcerptRange, MultiBuffer, MultiBufferSnapshot, PathKey,
    RowHighlightOptions,
    display_map::{
        BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, RenderBlock,
    },
};
use futures::future::{join_all, try_join_all};
use gpui::{
    Action as _, AnyWindowHandle, App, AsyncApp, AsyncWindowContext, BackgroundExecutor,
    ClickEvent, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Global,
    HighlightStyle, PromptLevel, Render, SharedString, Stateful, StyledText, Subscription, Task,
    WeakEntity, Window, actions, rgb,
};
use language::{Buffer, Capability, Point};
use multi_buffer::{ToPoint as _, build_excerpt_ranges};
use parking_lot::Mutex;
use project::{
    Project,
    lsp_store::OpenLspBufferHandle,
    trusted_worktrees::{TrustedWorktrees, TrustedWorktreesEvent},
};
use ui::{Checkbox, ToggleState, Tooltip, prelude::*};
use util::{ResultExt as _, rel_path::RelPath};
use workspace::{
    AppState, Item, OpenMode, OpenOptions, Toast, ToggleWorktreeSecurity, Workspace, WorkspaceDb,
    notifications::NotificationId, open_paths,
};

use crate::{
    CallerMark, ChangedFunction, Checkout, ChecksState, ContextCache, ContextError, ContextExcerpt,
    Draft, FetchedCheckout, FileAnalysis, FileDiff, FileKind, FileStatus, FunctionContext, GhError,
    GhRunner, LoadedFile, MapFile, MapFunction, PostState, PrDetail, PrKey, PrSummary, Reanchor,
    RepoId, ReviewDb, ReviewEvent, ReviewThread, SUMMARY_SLOT, Side, SinceReviewPlan, Step,
    ToggleChangesSinceReview, Tracked, analyse_loaded_file, auth_status_request, base_text,
    build_loaded_file, build_review_map, call_edges, caller_touched, candidate_repos,
    classify_caller, classify_path, commentable_lines, context::buffer_path, context_excerpts,
    deleted_lines, detail_request, fetch_checkout, fetch_url, find_clone, finish_checkout,
    functions, gh_runner, git_diff, is_sendable, landing_note, parse_auth_status, parse_detail,
    parse_mutation, parse_reply, place, plan_since_review, reanchor,
    refuse_head_change_under_open_window, reply_request, reply_slot, resolve_request,
    review_request, review_root, since_review_files, viewed_request,
};

actions!(
    photon_review,
    [
        /// Shows the next step of the pull request review map.
        NextStep,
        /// Shows the previous step of the pull request review map.
        PreviousStep,
        /// Starts a draft review comment on the selected lines.
        AddComment
    ]
);

const LEFT_COMMENT_NOTE: &str = "Switch to the full diff to comment on removed lines";
const VIEWED_DISABLED_TOOLTIP: &str = "Switch to the full diff to mark things viewed";
const HUNK_CONTEXT_LINES: u32 = 3;
const COMPOSER_MAX_LINES: u32 = 4;
const TRUST_REASON: &str = "Trust this worktree to find callers";
const UNCHANGED_BACKGROUND: u32 = 0x07090b;
const INTERRUPTED_NOTE: &str =
    "May already be posted: a submit was interrupted. Include it again or delete it.";
const EARLIER_VERSION_NOTE: &str = "An earlier version of this comment was posted. Include this \
     version too, or delete it.";
const IMPACT_LEGEND: &str = "Every caller in the repo, not just the diff. Red: the signature \
     changed and this caller wasn't updated. Amber: the body changed and this caller wasn't \
     touched, so check it.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextState {
    Finding,
    Ready,
    Unavailable(SharedString),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImpactRow {
    pub location: String,
    pub mark: CallerMark,
}

struct UnchangedContext;

/// The database side of one draft. Writes run one at a time; a newer body replaces a pending
/// one, so the newest text always lands last without a timer.
#[derive(Default)]
struct DraftSync {
    pending: Option<PendingWrite>,
    id: Option<i64>,
    in_flight: bool,
    error: Option<SharedString>,
    /// Woken when the queue runs dry, so Submit can wait for its marks to land.
    idle_waiters: Vec<futures::channel::oneshot::Sender<()>>,
    /// The draft as last queued, or `None` once discarded, for settling a submit that finishes
    /// after its review closed.
    latest: Option<Draft>,
}

/// An empty body is never stored, so emptying a draft removes its row until text comes back.
enum PendingWrite {
    Save(Draft),
    DeleteRow,
}

enum SyncJob {
    Save(Draft),
    Delete(i64),
}

impl DraftSync {
    fn next_job(&mut self) -> Option<SyncJob> {
        match self.pending.take() {
            Some(PendingWrite::Save(mut draft)) => {
                draft.id = self.id;
                return Some(SyncJob::Save(draft));
            }
            Some(PendingWrite::DeleteRow) => {
                if let Some(id) = self.id.take() {
                    return Some(SyncJob::Delete(id));
                }
            }
            None => {}
        }
        self.in_flight = false;
        for waiter in self.idle_waiters.drain(..) {
            waiter.send(()).ok();
        }
        None
    }
}

/// The database side of one composer's unsent text: the summary or a thread's reply. As with
/// `DraftSync`, one writer drains it, so the newest text always lands last.
#[derive(Default)]
struct TextSync {
    pending: Option<String>,
    in_flight: bool,
    latest: String,
}

fn queue_text_write(
    sync: &Arc<Mutex<TextSync>>,
    database: &ReviewDb,
    key: &PrKey,
    slot: &str,
    body: String,
    executor: &BackgroundExecutor,
) {
    {
        let mut state = sync.lock();
        state.latest = body.clone();
        state.pending = Some(body);
        if std::mem::replace(&mut state.in_flight, true) {
            return;
        }
    }
    let sync = sync.clone();
    let database = database.clone();
    let key = key.clone();
    let slot = slot.to_string();
    // Detached so closing the review never cancels the write of what the user typed.
    executor
        .spawn(async move {
            loop {
                let body = {
                    let mut state = sync.lock();
                    let Some(body) = state.pending.take() else {
                        state.in_flight = false;
                        return;
                    };
                    body
                };
                database
                    .save_composer_text(key.clone(), slot.clone(), body)
                    .await
                    .log_err();
            }
        })
        .detach();
}

/// Clears a sent composer's stored text unless it was edited after sending.
fn clear_sent_text(
    sync: &Arc<Mutex<TextSync>>,
    sent: &str,
    database: &ReviewDb,
    key: &PrKey,
    slot: &str,
    executor: &BackgroundExecutor,
) {
    let unchanged = sync.lock().latest == sent;
    if unchanged {
        queue_text_write(sync, database, key, slot, String::new(), executor);
    }
}

struct DraftView {
    local_id: usize,
    composer: Option<Entity<Editor>>,
    sync: Arc<Mutex<DraftSync>>,
    _composer_subscription: Option<Subscription>,
}

struct SelectedLines {
    path: String,
    side: Side,
    start_line: u32,
    end_line: u32,
    anchor_text: String,
}

pub struct ReviewItem {
    project: Entity<Project>,
    detail: PrDetail,
    checkout: Checkout,
    files: Vec<LoadedFile>,
    analyses: Vec<Option<FileAnalysis>>,
    functions: Vec<ChangedFunction>,
    contexts: Vec<Option<FunctionContext>>,
    changed_rows: HashMap<String, Vec<Range<u32>>>,
    steps: Vec<Step>,
    selected: usize,
    viewed_steps: HashSet<String>,
    context_state: ContextState,
    editor: Entity<Editor>,
    multibuffer: Entity<MultiBuffer>,
    context_cache: ContextCache,
    shown_paths: Vec<PathKey>,
    shown_context: Vec<ContextExcerpt>,
    expanded_context: HashSet<(EntityId, Point)>,
    label_blocks: HashSet<CustomBlockId>,
    impact_function: Option<String>,
    impact: Vec<ImpactRow>,
    database: ReviewDb,
    commentable: HashMap<String, Vec<RangeInclusive<u32>>>,
    drafts: Vec<Draft>,
    draft_views: Vec<DraftView>,
    next_draft_id: usize,
    reply_editors: HashMap<String, Entity<Editor>>,
    text_syncs: HashMap<String, Arc<Mutex<TextSync>>>,
    comment_blocks: HashSet<CustomBlockId>,
    login: String,
    summary_editor: Entity<Editor>,
    submitting: bool,
    submit_error: Option<SharedString>,
    thread_errors: HashMap<String, SharedString>,
    file_errors: HashMap<String, SharedString>,
    submit_note: Option<SharedString>,
    resolving: HashSet<String>,
    marking_viewed: HashSet<String>,
    last_review: Option<String>,
    full_files: Option<Vec<LoadedFile>>,
    since_review_note: Option<SharedString>,
    loading_since_review: bool,
    _since_review_lsp_handles: Vec<OpenLspBufferHandle>,
    _since_review_task: Task<()>,
    _reanchor_task: Task<()>,
    _lsp_handles: Vec<OpenLspBufferHandle>,
    _subscriptions: Vec<Subscription>,
    _context_task: Task<()>,
}

pub(crate) struct PreparedReview {
    pub login: String,
    pub checkout: Checkout,
    pub detail: PrDetail,
    pub files: Vec<(FileDiff, Option<String>)>,
}

#[derive(Default)]
struct OpeningReviews(Arc<Mutex<HashSet<PrKey>>>);

impl Global for OpeningReviews {}

/// Frees the pull request for another `open_review` when its open finishes or is dropped.
struct OpeningGuard {
    opening: Arc<Mutex<HashSet<PrKey>>>,
    key: PrKey,
}

impl Drop for OpeningGuard {
    fn drop(&mut self) {
        self.opening.lock().remove(&self.key);
    }
}

/// Errors are shown as a toast in the window the user clicked from. A second call for a pull
/// request that is still opening does nothing.
pub fn open_review(
    summary: PrSummary,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let opening = cx.default_global::<OpeningReviews>().0.clone();
    if !opening.lock().insert(summary.key.clone()) {
        return Task::ready(Ok(()));
    }
    let guard = OpeningGuard {
        opening,
        key: summary.key.clone(),
    };
    let app_state = workspace.app_state().clone();
    let runner = gh_runner(cx);
    let database = ReviewDb::global(cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let result = open_in_new_window(summary, app_state, runner, database, cx).await;
        drop(guard);
        if let Err(error) = &result {
            let message = format!("{error:#}");
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(NotificationId::unique::<ReviewItem>(), message),
                        cx,
                    )
                })
                .log_err();
        }
        result
    })
}

/// The worktree path is reused for every head of a pull request, so trust given to an earlier
/// head must not carry over to code the user hasn't seen. Runs before the worktree is opened.
pub(crate) fn forget_stale_trust(checkout: &Checkout, cx: &mut App) {
    if !checkout.head_changed {
        return;
    }
    let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
        return;
    };
    trusted_worktrees.update(cx, |trusted_worktrees, cx| {
        trusted_worktrees.forget_trust(&checkout.worktree, cx);
        // Saved now, as Zed only saves trust when something is trusted.
        trusted_worktrees.schedule_serialization(cx, |trusted_paths, cx| {
            let database = WorkspaceDb::global(cx);
            cx.background_spawn(async move {
                database
                    .save_trusted_worktrees(trusted_paths)
                    .await
                    .log_err();
            })
        });
    });
}

/// Trusting a folder that contains Photon's review worktrees must not trust a pull request's code.
pub(crate) fn refuse_parent_trust(root: PathBuf, cx: &mut App) {
    if let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) {
        trusted_worktrees.update(cx, |trusted_worktrees, _| {
            trusted_worktrees.refuse_parent_trust_under(root)
        });
    }
}

fn find_open_review(
    app_state: &AppState,
    key: &PrKey,
    cx: &App,
) -> Option<(AnyWindowHandle, Entity<Workspace>, Entity<ReviewItem>)> {
    app_state
        .workspace_store
        .read(cx)
        .workspaces_with_windows()
        .find_map(|(window, workspace)| {
            let workspace = workspace.upgrade()?;
            let item = workspace
                .read(cx)
                .items_of_type::<ReviewItem>(cx)
                .find(|item| item.read(cx).detail.summary.key == *key)?;
            Some((window, workspace, item))
        })
}

async fn open_in_new_window(
    summary: PrSummary,
    app_state: Arc<AppState>,
    runner: Arc<dyn GhRunner>,
    database: ReviewDb,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let open = cx.update(|_, cx| find_open_review(&app_state, &summary.key, cx))?;
    if let Some((window, workspace, item)) = open
        && item.read_with(cx, |item, _| item.checkout.head_sha == summary.head_sha)
    {
        window.update(cx, |_, window, cx| {
            window.activate_window();
            workspace.update(cx, |workspace, cx| {
                workspace.activate_item(&item, true, true, window, cx)
            })
        })?;
        return Ok(());
    }

    // Not read up front: the caller is usually mid-update on a workspace this would read.
    let candidates = cx.update(|_, cx| candidate_repos(cx))?.await;
    let url = fetch_url(&repo_id(&summary));
    let (login, fetched) = cx
        .background_spawn({
            let summary = summary.clone();
            let runner = runner.clone();
            async move { fetch_review(&summary, url, review_root(), candidates, runner).await }
        })
        .await?;
    if refuse_head_change_under_open_window(&summary.key, &fetched.head_sha, &app_state, cx).await?
    {
        return Ok(());
    }
    let prepared = cx
        .background_spawn({
            let database = database.clone();
            async move { finish_review(&summary, login, fetched, runner, database).await }
        })
        .await?;
    cx.update(|_, cx| forget_stale_trust(&prepared.checkout, cx))?;
    let opened = cx
        .update(|_, cx| {
            open_paths(
                std::slice::from_ref(&prepared.checkout.worktree),
                app_state,
                OpenOptions {
                    open_mode: OpenMode::NewWindow,
                    ..OpenOptions::default()
                },
                cx,
            )
        })?
        .await?;
    place_review(
        opened.workspace,
        opened.window.into(),
        prepared,
        database,
        cx,
    )
    .await
}

/// Adds the review to the workspace's active pane, or replaces an open review of the same pull
/// request in place, since that one describes an older head.
async fn place_review(
    workspace: Entity<Workspace>,
    window: AnyWindowHandle,
    prepared: PreparedReview,
    database: ReviewDb,
    cx: &mut AsyncApp,
) -> Result<()> {
    let PreparedReview {
        login,
        checkout,
        detail,
        files,
    } = prepared;
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
    let loads = cx.update(|cx| {
        files
            .into_iter()
            .map(|(diff, base)| {
                let abs_path = checkout.worktree.join(&diff.path);
                build_loaded_file(&project, abs_path, base, diff, cx)
            })
            .collect::<Vec<_>>()
    });
    let files = try_join_all(loads).await?;
    window.update(cx, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let key = detail.summary.key.clone();
            let existing = workspace
                .items_of_type::<ReviewItem>(cx)
                .find(|item| item.read(cx).detail.summary.key == key);
            let item = cx.new(|cx| {
                ReviewItem::new(
                    project, detail, login, checkout, files, database, window, cx,
                )
            });
            let pane = existing.as_ref().and_then(|existing| {
                workspace
                    .panes()
                    .iter()
                    .find(|pane| pane.read(cx).index_for_item(existing).is_some())
                    .cloned()
            });
            match (existing, pane) {
                (Some(existing), Some(pane)) => pane.update(cx, |pane, cx| {
                    let index = pane.index_for_item(&existing);
                    pane.remove_item(existing.entity_id(), false, false, window, cx);
                    pane.add_item(Box::new(item), true, true, index, window, cx);
                }),
                _ => workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx),
            }
        })
    })?;
    Ok(())
}

pub(crate) fn repo_id(summary: &PrSummary) -> RepoId {
    RepoId {
        host: summary.key.host.to_lowercase(),
        owner: summary.key.owner.to_lowercase(),
        repo: summary.key.repo.to_lowercase(),
    }
}

/// Everything `open_review` does before it opens a window: the login check, the checkout, the
/// detail and the diff. The user's clone only gains Photon's refs and worktree entry (see `prepare_checkout`).
#[cfg(test)]
pub(crate) async fn prepare_review(
    summary: &PrSummary,
    fetch_url: String,
    root: PathBuf,
    candidates: Vec<PathBuf>,
    runner: Arc<dyn GhRunner>,
    database: ReviewDb,
) -> Result<PreparedReview> {
    let (login, fetched) =
        fetch_review(summary, fetch_url, root, candidates, runner.clone()).await?;
    finish_review(summary, login, fetched, runner, database).await
}

/// The login check and the fetch: everything before the worktree moves. Returns the login.
pub(crate) async fn fetch_review(
    summary: &PrSummary,
    fetch_url: String,
    root: PathBuf,
    candidates: Vec<PathBuf>,
    runner: Arc<dyn GhRunner>,
) -> Result<(String, FetchedCheckout)> {
    let key = &summary.key;
    let hosts = parse_auth_status(&runner.run(auth_status_request()).await?)?;
    let login = hosts
        .into_iter()
        .find(|auth| auth.host.eq_ignore_ascii_case(&key.host))
        .and_then(|auth| auth.login)
        .ok_or_else(|| GhError::LoggedOut {
            host: Some(key.host.clone()),
        })?;

    let clone = find_clone(candidates, &repo_id(summary)).await;
    let fetched = fetch_checkout(key, &summary.base_ref, &fetch_url, clone, &root).await?;
    Ok((login, fetched))
}

pub(crate) async fn finish_review(
    summary: &PrSummary,
    login: String,
    fetched: FetchedCheckout,
    runner: Arc<dyn GhRunner>,
    database: ReviewDb,
) -> Result<PreparedReview> {
    let key = &summary.key;
    let checkout = finish_checkout(fetched).await?;
    database
        .track(Tracked {
            key: key.clone(),
            node_id: summary.node_id.clone(),
            repo_dir: checkout.repo_dir.clone(),
            worktree: checkout.worktree.clone(),
        })
        .await?;
    let detail = parse_detail(key, &runner.run(detail_request(key, &login)).await?)?;

    let diffs = git_diff(&checkout.worktree, &checkout.merge_base, &checkout.head_sha).await?;
    let mut files = Vec::with_capacity(diffs.len());
    for diff in diffs {
        let base = base_text(&checkout.worktree, &checkout.merge_base, &diff).await?;
        files.push((diff, base));
    }
    Ok(PreparedReview {
        login,
        checkout,
        detail,
        files,
    })
}

fn status_label(diff: &FileDiff) -> &'static str {
    if diff.submodule {
        return "submodule";
    }
    if diff.symlink {
        return "symlink";
    }
    if diff.binary {
        return "binary file";
    }
    match diff.status {
        FileStatus::Added => "new file",
        FileStatus::Modified => "changed",
        FileStatus::Deleted => "deleted",
        FileStatus::Renamed { .. } => "renamed",
    }
}

fn unavailable_reason(error: ContextError) -> &'static str {
    match error {
        ContextError::NoCallHierarchy => "No language server offers call hierarchy for this code.",
        ContextError::TimedOut => "The language server didn't answer within 30 seconds.",
    }
}

fn mark_tag(mark: CallerMark) -> (&'static str, Color) {
    match mark {
        CallerMark::InThisPr => ("in this PR", Color::Accent),
        CallerMark::Unchanged => ("unchanged", Color::Muted),
        CallerMark::NotUpdated => ("not updated", Color::Error),
        CallerMark::Check => ("check", Color::Warning),
    }
}

fn buffer_for<'a>(files: &'a [LoadedFile], path: &str) -> Option<&'a Entity<Buffer>> {
    files
        .iter()
        .find(|file| file.diff.path == path)
        .and_then(|file| file.contents.as_ref())
        .map(|(buffer, _)| buffer)
}

fn path_key(sort_prefix: usize, buffer: &Entity<Buffer>, path: &str, cx: &App) -> Option<PathKey> {
    let path = match buffer.read(cx).file() {
        Some(file) => file.path().clone(),
        None => RelPath::from_unix_str(path).log_err()?.into_arc(),
    };
    Some(PathKey::with_sort_prefix(sort_prefix as u64, path))
}

fn label_renderer(
    label: SharedString,
    collapsed: bool,
    excerpt_index: usize,
    item: WeakEntity<ReviewItem>,
) -> RenderBlock {
    Arc::new(move |block: &mut BlockContext| {
        let item = item.clone();
        h_flex()
            .id(("photon-review-context", excerpt_index))
            .w_full()
            .pl(block.margins.gutter.full_width())
            .child(
                Label::new(label.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when(collapsed, |this| {
                this.cursor_pointer().on_click(move |_, _, cx| {
                    item.update(cx, |item, cx| item.expand_context(excerpt_index, cx))
                        .log_err();
                })
            })
            .into_any_element()
    })
}

fn comment_card(id: ElementId, block: &BlockContext) -> Stateful<Div> {
    let colors = block.app.theme().colors();
    v_flex()
        .id(id)
        .ml(block.margins.gutter.full_width())
        .mr_4()
        .my_1()
        .p_2()
        .gap_1()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.panel_background)
}

fn draft_renderer(
    item: WeakEntity<ReviewItem>,
    local_id: usize,
    line: u32,
    body: SharedString,
    note: Option<SharedString>,
    posted_note: Option<&'static str>,
    composer: Option<Entity<Editor>>,
    sync: Arc<Mutex<DraftSync>>,
) -> RenderBlock {
    Arc::new(move |block: &mut BlockContext| {
        let error = sync.lock().error.clone();
        let include_item = item.clone();
        let add_item = item.clone();
        let cancel_item = item.clone();
        let retry_item = item.clone();
        let edit_item = item.clone();
        let delete_item = item.clone();
        comment_card(("photon-review-draft", local_id).into(), block)
            .child(
                h_flex()
                    .gap_1()
                    .child(Label::new("you").size(LabelSize::Small))
                    .child(
                        Label::new(format!("· on line {line} · draft"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .map(|this| match &composer {
                Some(editor) => this.child(editor.clone()),
                None => this.child(Label::new(body.clone()).size(LabelSize::Small)),
            })
            .children(
                note.clone()
                    .map(|note| Label::new(note).size(LabelSize::XSmall).color(Color::Muted)),
            )
            .when_some(posted_note, |this, posted_note| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Label::new(posted_note)
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                        .child(
                            Button::new("include", "Include again")
                                .label_size(LabelSize::XSmall)
                                .on_click(move |_, _, cx| {
                                    include_item
                                        .update(cx, |item, cx| item.include_again(local_id, cx))
                                        .log_err();
                                }),
                        ),
                )
            })
            .children(error.map(|error| {
                h_flex()
                    .gap_1()
                    .child(
                        Label::new(error)
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    )
                    .child(
                        Button::new("retry", "Retry")
                            .label_size(LabelSize::XSmall)
                            .on_click(move |_, _, cx| {
                                retry_item
                                    .update(cx, |item, cx| item.queue_save(local_id, cx))
                                    .log_err();
                            }),
                    )
            }))
            .when(composer.is_some(), |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("add", "Add to review")
                                .style(ButtonStyle::Filled)
                                .on_click(move |_, _, cx| {
                                    add_item
                                        .update(cx, |item, cx| item.collapse_draft(local_id, cx))
                                        .log_err();
                                }),
                        )
                        .child(
                            Button::new("cancel", "Cancel").on_click(move |_, window, cx| {
                                cancel_item
                                    .update(cx, |item, cx| item.cancel_draft(local_id, window, cx))
                                    .log_err();
                            }),
                        ),
                )
            })
            .when(composer.is_none(), |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(Button::new("edit", "Edit").on_click(move |_, window, cx| {
                            edit_item
                                .update(cx, |item, cx| item.edit_draft(local_id, window, cx))
                                .log_err();
                        }))
                        .child(
                            Button::new("delete", "Delete").on_click(move |_, window, cx| {
                                delete_item
                                    .update(cx, |item, cx| item.cancel_draft(local_id, window, cx))
                                    .log_err();
                            }),
                        ),
                )
            })
            .into_any_element()
    })
}

fn thread_renderer(
    item: WeakEntity<ReviewItem>,
    thread: ReviewThread,
    reply: Option<Entity<Editor>>,
    error: Option<SharedString>,
    resolve_pending: bool,
) -> RenderBlock {
    Arc::new(move |block: &mut BlockContext| {
        let id = SharedString::from(format!("photon-review-thread-{}", thread.id));
        let reply_item = item.clone();
        let resolve_item = item.clone();
        let reply_thread = thread.id.clone();
        let resolve_thread = thread.id.clone();
        let resolved = thread.is_resolved;
        comment_card(id.into(), block)
            .children(thread.comments.iter().map(|comment| {
                v_flex()
                    .child(Label::new(comment.author.clone()).size(LabelSize::Small))
                    .child(Label::new(comment.body.clone()).size(LabelSize::Small))
            }))
            .children(error.clone().map(|error| {
                Label::new(error)
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
            }))
            .child(
                h_flex()
                    .gap_1()
                    .children(reply.clone().map(|editor| div().flex_1().child(editor)))
                    .child(Button::new("reply", "Reply").on_click(move |_, _, cx| {
                        reply_item
                            .update(cx, |item, cx| {
                                item.click_reply(&reply_thread, cx).detach_and_log_err(cx)
                            })
                            .log_err();
                    }))
                    .child(
                        Button::new("resolve", if resolved { "Unresolve" } else { "Resolve" })
                            .disabled(resolve_pending)
                            .on_click(move |_, _, cx| {
                                resolve_item
                                    .update(cx, |item, cx| {
                                        item.set_thread_resolved(&resolve_thread, !resolved, cx)
                                            .detach_and_log_err(cx)
                                    })
                                    .log_err();
                            }),
                    ),
            )
            .into_any_element()
    })
}

/// Returns false when a writer is already draining this draft's queue; it will pick up the
/// pending write.
fn claim_sync(sync: &Mutex<DraftSync>) -> bool {
    !std::mem::replace(&mut sync.lock().in_flight, true)
}

/// Runs the draft's queued writes now, unless a writer is already running them.
async fn flush_sync(
    this: &WeakEntity<ReviewItem>,
    local_id: usize,
    sync: &Arc<Mutex<DraftSync>>,
    database: &ReviewDb,
    cx: &mut AsyncApp,
) {
    if claim_sync(sync) {
        drain_sync(this.clone(), local_id, sync.clone(), database.clone(), cx).await;
    }
}

/// Queues the draft's last known text with a new post state, for when the review has closed.
fn requeue_latest(sync: &Mutex<DraftSync>, post_state: PostState) {
    let mut state = sync.lock();
    let latest = state
        .latest
        .clone()
        .filter(|draft| !draft.body.trim().is_empty());
    if let Some(mut latest) = latest {
        latest.post_state = post_state;
        state.pending = Some(PendingWrite::Save(latest));
    }
}

/// Runs a draft's queued writes in order. It works whether or not the review is still open, so a
/// delete queued after the review closes still follows any save in flight.
async fn drain_sync(
    this: WeakEntity<ReviewItem>,
    local_id: usize,
    sync: Arc<Mutex<DraftSync>>,
    database: ReviewDb,
    cx: &mut AsyncApp,
) {
    loop {
        let job = sync.lock().next_job();
        match job {
            None => return,
            Some(SyncJob::Delete(id)) => match database.delete_draft(id).await {
                Ok(()) => {
                    this.update(cx, |this, _| {
                        if let Some(index) = this.draft_index(local_id) {
                            this.drafts[index].id = None;
                        }
                    })
                    .ok();
                }
                Err(error) => {
                    log::error!("deleting a review draft failed: {error:#}");
                    let mut state = sync.lock();
                    if state.id.is_none() {
                        state.id = Some(id);
                    }
                    state.error = Some(format!("Couldn't save this draft: {error:#}").into());
                }
            },
            Some(SyncJob::Save(draft)) => {
                let result = database.save_draft(draft).await;
                let saved_id = {
                    let mut state = sync.lock();
                    match result {
                        Ok(id) => {
                            state.id = Some(id);
                            state.error = None;
                            Some(id)
                        }
                        Err(error) => {
                            log::error!("saving a review draft failed: {error:#}");
                            state.error =
                                Some(format!("Couldn't save this draft: {error:#}").into());
                            None
                        }
                    }
                };
                // The review may be closed by now; the write itself already happened.
                this.update(cx, |this, cx| {
                    if let (Some(id), Some(index)) = (saved_id, this.draft_index(local_id)) {
                        this.drafts[index].id = Some(id);
                    }
                    // Draft cards read the save error when the editor redraws them.
                    this.editor.update(cx, |_, cx| cx.notify());
                })
                .ok();
            }
        }
    }
}

/// Call hierarchy needs every head buffer open in its language server.
fn index_files(
    project: &Entity<Project>,
    files: &[LoadedFile],
    cx: &mut App,
) -> (Vec<OpenLspBufferHandle>, HashMap<String, Vec<Range<u32>>>) {
    let mut lsp_handles = Vec::new();
    let mut changed_rows = HashMap::new();
    for file in files {
        let Some((buffer, diff)) = &file.contents else {
            continue;
        };
        if buffer.read(cx).file().is_some() {
            lsp_handles.push(project.update(cx, |project, cx| {
                project.register_buffer_with_language_servers(buffer, cx)
            }));
        }
        let rows =
            functions::changed_rows(&diff.read(cx).snapshot(cx), &buffer.read(cx).snapshot());
        changed_rows.insert(file.diff.path.clone(), rows);
    }
    (lsp_handles, changed_rows)
}

impl ReviewItem {
    pub fn new(
        project: Entity<Project>,
        detail: PrDetail,
        login: String,
        checkout: Checkout,
        files: Vec<LoadedFile>,
        database: ReviewDb,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            Editor::for_multibuffer(multibuffer.clone(), Some(project.clone()), window, cx)
        });
        let (lsp_handles, changed_rows) = index_files(&project, &files, cx);
        let mut subscriptions = vec![cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::SelectionsChanged { .. }) {
                this.update_impact(cx);
            }
        })];
        if let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) {
            subscriptions.push(cx.subscribe(
                &trusted_worktrees,
                |this, _, event: &TrustedWorktreesEvent, cx| {
                    let TrustedWorktreesEvent::Trusted(worktree_store, _) = event else {
                        return;
                    };
                    let waiting =
                        this.context_state == ContextState::Unavailable(TRUST_REASON.into());
                    if waiting
                        && *worktree_store == this.project.read(cx).worktree_store().downgrade()
                    {
                        this._context_task = this.load_context(cx);
                    }
                },
            ));
        }
        let commentable = files
            .iter()
            .map(|file| (file.diff.path.clone(), commentable_lines(&file.diff)))
            .collect();
        let composer_texts = database
            .composer_texts(&detail.summary.key)
            .log_err()
            .unwrap_or_default();
        let summary_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, COMPOSER_MAX_LINES as usize, window, cx);
            editor.set_placeholder_text("Leave a summary (optional)", window, cx);
            if let Some(text) = composer_texts.get(SUMMARY_SLOT) {
                editor.set_text(text.as_str(), window, cx);
            }
            editor
        });
        // Comment is enabled by summary text, so the actions area redraws as it is typed.
        subscriptions.push(cx.subscribe(
            &summary_editor,
            |this, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    let text = editor.read(cx).text(cx);
                    this.queue_text(SUMMARY_SLOT, text, cx);
                    cx.notify();
                }
            },
        ));
        let text_syncs = composer_texts
            .iter()
            .map(|(slot, text)| {
                let sync = TextSync {
                    latest: text.clone(),
                    ..TextSync::default()
                };
                (slot.clone(), Arc::new(Mutex::new(sync)))
            })
            .collect();
        let stored_drafts = database
            .drafts(&detail.summary.key)
            .log_err()
            .unwrap_or_default();
        let viewed_steps = database
            .viewed_steps(&detail.summary.key)
            .log_err()
            .unwrap_or_default();
        // GitHub's record wins: it includes reviews submitted on github.com.
        let last_review = detail.summary.last_review_commit.clone().or_else(|| {
            database
                .last_review(&detail.summary.key)
                .log_err()
                .flatten()
        });
        let mut this = Self {
            project,
            detail,
            checkout,
            files,
            analyses: Vec::new(),
            functions: Vec::new(),
            contexts: Vec::new(),
            changed_rows,
            steps: Vec::new(),
            selected: 0,
            viewed_steps,
            context_state: ContextState::Finding,
            editor,
            multibuffer,
            context_cache: ContextCache::default(),
            shown_paths: Vec::new(),
            shown_context: Vec::new(),
            expanded_context: HashSet::new(),
            label_blocks: HashSet::new(),
            impact_function: None,
            impact: Vec::new(),
            database,
            commentable,
            drafts: Vec::new(),
            draft_views: Vec::new(),
            next_draft_id: 0,
            reply_editors: HashMap::new(),
            text_syncs,
            comment_blocks: HashSet::new(),
            login,
            summary_editor,
            submitting: false,
            submit_error: None,
            thread_errors: HashMap::new(),
            file_errors: HashMap::new(),
            submit_note: None,
            resolving: HashSet::new(),
            marking_viewed: HashSet::new(),
            last_review,
            full_files: None,
            since_review_note: None,
            loading_since_review: false,
            _since_review_lsp_handles: Vec::new(),
            _since_review_task: Task::ready(()),
            _reanchor_task: Task::ready(()),
            _lsp_handles: lsp_handles,
            _subscriptions: subscriptions,
            _context_task: Task::ready(()),
        };
        for draft in stored_drafts {
            this.push_draft(draft);
        }
        this.add_reply_editors(&composer_texts, window, cx);
        this.rebuild_map(&[], cx);
        this._context_task = this.load_context(cx);
        this._reanchor_task = this.reanchor_drafts(cx);
        this
    }

    pub fn changes_since_review_offered(&self) -> bool {
        self.last_review
            .as_ref()
            .is_some_and(|reviewed| *reviewed != self.checkout.head_sha)
    }

    pub fn showing_changes_since_review(&self) -> bool {
        self.full_files.is_some()
    }

    pub fn toggle_changes_since_review(
        &mut self,
        _: &ToggleChangesSinceReview,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.since_review_note = None;
        if self.loading_since_review {
            self.loading_since_review = false;
            self._since_review_task = Task::ready(());
            cx.notify();
            return;
        }
        if let Some(full_files) = self.full_files.take() {
            self._since_review_task = Task::ready(());
            self.set_files(full_files, cx);
            return;
        }
        let Some(reviewed) = self
            .last_review
            .clone()
            .filter(|_| self.changes_since_review_offered())
        else {
            return;
        };
        let checkout = self.checkout.clone();
        let key = self.detail.summary.key.clone();
        let url = fetch_url(&repo_id(&self.detail.summary));
        let project = self.project.clone();
        self.loading_since_review = true;
        self._since_review_task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn({
                    let checkout = checkout.clone();
                    async move {
                        let plan = plan_since_review(&checkout, &key, &url, &reviewed).await;
                        let files = match &plan {
                            SinceReviewPlan::Unavailable { .. } => Vec::new(),
                            _ => since_review_files(&checkout, &plan).await?,
                        };
                        anyhow::Ok((plan, files))
                    }
                })
                .await;
            let loaded = match outcome {
                Ok((plan, files)) => match plan.message() {
                    Some(message) => Err(message.to_string()),
                    None => {
                        let loads = cx.update(|cx| {
                            files
                                .into_iter()
                                .map(|(diff, base)| {
                                    let abs_path = checkout.worktree.join(&diff.path);
                                    build_loaded_file(&project, abs_path, base, diff, cx)
                                })
                                .collect::<Vec<_>>()
                        });
                        try_join_all(loads)
                            .await
                            .map_err(|error| format!("{error:#}"))
                    }
                },
                Err(error) => Err(format!("{error:#}")),
            };
            this.update(cx, |this, cx| {
                this.loading_since_review = false;
                match loaded {
                    Ok(files) => {
                        let full = std::mem::take(&mut this.files);
                        this.full_files = Some(full);
                        this.set_files(files, cx);
                    }
                    Err(message) => {
                        this.since_review_note = Some(message.into());
                        cx.notify();
                    }
                }
            })
            .log_err();
        });
        cx.notify();
    }

    fn set_files(&mut self, files: Vec<LoadedFile>, cx: &mut Context<Self>) {
        let (lsp_handles, changed_rows) = index_files(&self.project, &files, cx);
        // The full set's handles live in `_lsp_handles`; only the subset's are replaced here.
        self._since_review_lsp_handles = if self.full_files.is_some() {
            lsp_handles
        } else {
            Vec::new()
        };
        self.changed_rows = changed_rows;
        self.files = files;
        self.analyses = Vec::new();
        self.functions = Vec::new();
        self.contexts = Vec::new();
        self.steps = Vec::new();
        self.selected = 0;
        self.expanded_context.clear();
        self.rebuild_map(&[], cx);
        self._context_task = self.load_context(cx);
    }

    pub fn drafts(&self) -> &[Draft] {
        &self.drafts
    }

    /// Gives each thread without one a reply box, keeping what was typed in existing ones.
    /// `stored` is unsent text from the database, by slot.
    fn add_reply_editors(
        &mut self,
        stored: &HashMap<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread_ids: Vec<String> = self
            .detail
            .threads
            .iter()
            .map(|thread| thread.id.clone())
            .filter(|thread_id| !self.reply_editors.contains_key(thread_id))
            .collect();
        for thread_id in thread_ids {
            let slot = reply_slot(&thread_id);
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Reply…", window, cx);
                if let Some(text) = stored.get(&slot) {
                    editor.set_text(text.as_str(), window, cx);
                }
                editor
            });
            self._subscriptions.push(cx.subscribe(
                &editor,
                move |this, editor, event: &EditorEvent, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        let text = editor.read(cx).text(cx);
                        this.queue_text(&slot, text, cx);
                    }
                },
            ));
            self.reply_editors.insert(thread_id, editor);
        }
    }

    fn text_sync(&mut self, slot: &str) -> Arc<Mutex<TextSync>> {
        self.text_syncs.entry(slot.to_string()).or_default().clone()
    }

    fn queue_text(&mut self, slot: &str, body: String, cx: &mut Context<Self>) {
        let sync = self.text_sync(slot);
        queue_text_write(
            &sync,
            &self.database,
            &self.detail.summary.key,
            slot,
            body,
            cx.background_executor(),
        );
    }

    fn sendable_drafts(&self) -> usize {
        self.drafts
            .iter()
            .filter(|draft| is_sendable(draft, &self.checkout.head_sha))
            .count()
    }

    /// Drafts with text that Submit leaves out because they are outdated or not yet re-anchored.
    pub fn left_out_drafts(&self) -> usize {
        self.drafts
            .iter()
            .filter(|draft| {
                !draft.body.trim().is_empty()
                    && draft.post_state == PostState::Unposted
                    && !is_sendable(draft, &self.checkout.head_sha)
            })
            .count()
    }

    /// Why a draft may already be on GitHub. While a submit runs, its own drafts are marked, so
    /// nothing is said until it ends.
    pub fn posted_note(&self, draft: &Draft) -> Option<&'static str> {
        if self.submitting {
            return None;
        }
        match draft.post_state {
            PostState::Unposted => None,
            PostState::Sending => Some(INTERRUPTED_NOTE),
            PostState::EarlierVersionPosted => Some(EARLIER_VERSION_NOTE),
        }
    }

    pub fn left_out_note(&self) -> Option<String> {
        let mut parts = Vec::new();
        let left_out = self.left_out_drafts();
        if left_out > 0 {
            let plural = if left_out == 1 { "" } else { "s" };
            parts.push(format!(
                "{left_out} draft{plural} not included (outdated or not yet re-placed)"
            ));
        }
        let maybe_posted = self
            .drafts
            .iter()
            .filter(|draft| self.posted_note(draft).is_some())
            .count();
        if maybe_posted > 0 {
            parts.push(format!("{maybe_posted} may already be posted"));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// Makes a draft that may already be posted sendable again, at the user's word.
    pub fn include_again(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        self.drafts[index].post_state = PostState::Unposted;
        self.queue_save(local_id, cx);
        self.show_comments(cx);
        cx.notify();
    }

    pub fn can_comment(&self, cx: &App) -> bool {
        self.sendable_drafts() > 0 || !self.summary_editor.read(cx).text(cx).trim().is_empty()
    }

    fn buffer_for_path(&self, path: &str, cx: &App) -> Option<Entity<Buffer>> {
        self.multibuffer
            .read(cx)
            .all_buffers_iter()
            .find(|buffer| buffer_path(buffer, cx).as_deref() == Some(path))
            .or_else(|| buffer_for(&self.files, path).cloned())
    }

    fn draft_languages(&self, cx: &App) -> HashMap<String, String> {
        self.drafts
            .iter()
            .filter_map(|draft| {
                let buffer = self.buffer_for_path(&draft.path, cx)?;
                let language = buffer.read(cx).language()?.code_fence_block_name();
                Some((draft.path.clone(), language.to_string()))
            })
            .collect()
    }

    /// Posts every sendable draft as one GitHub review. The sent drafts are marked `sending` in
    /// the database first, so after a crash they come back as "may already be posted" rather than
    /// sendable. A failure keeps every draft and shows GitHub's message until the next submit.
    pub fn submit(
        &mut self,
        event: ReviewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.submitting {
            return Task::ready(Err(anyhow!("This review is already being submitted.")));
        }
        let key = self.detail.summary.key.clone();
        let head_sha = self.checkout.head_sha.clone();
        let summary = self.summary_editor.read(cx).text(cx);
        let summary_sync = self.text_sync(SUMMARY_SLOT);
        let commentable = self.commentable.clone();
        let languages = self.draft_languages(cx);
        let marked: Vec<(usize, Arc<Mutex<DraftSync>>)> = self
            .drafts
            .iter()
            .zip(&self.draft_views)
            .filter(|(draft, _)| is_sendable(draft, &head_sha))
            .map(|(_, view)| (view.local_id, view.sync.clone()))
            .collect();
        // Marked through each draft's own queue, so the mark lands after any save in flight,
        // a first insert included, and the request waits until every queue has run dry.
        let mut marks = Vec::new();
        for (local_id, sync) in &marked {
            if let Some(index) = self.draft_index(*local_id) {
                self.drafts[index].post_state = PostState::Sending;
                self.queue_save(*local_id, cx);
            }
            let mut state = sync.lock();
            if state.in_flight {
                let (waiter, idle) = futures::channel::oneshot::channel();
                state.idle_waiters.push(waiter);
                marks.push(idle);
            }
        }
        self.submitting = true;
        self.submit_error = None;
        self.submit_note = None;
        cx.notify();
        let runner = gh_runner(cx);
        let database = self.database.clone();
        cx.spawn_in(window, async move |this, cx| {
            for idle in marks {
                idle.await.ok();
            }
            // Built after the wait from each draft's latest version, which works even if the
            // review closed: a draft removed or emptied meanwhile is left out, and one edited
            // meanwhile goes with its newer text.
            let mut sent = Vec::new();
            let mut drafts = Vec::new();
            let mut unmarked = false;
            for (local_id, sync) in &marked {
                let state = sync.lock();
                let Some(latest) = state
                    .latest
                    .as_ref()
                    .filter(|draft| !draft.body.trim().is_empty())
                else {
                    continue;
                };
                unmarked |= state.error.is_some() || state.id.is_none();
                drafts.push(Draft {
                    post_state: PostState::Unposted,
                    ..latest.clone()
                });
                sent.push((*local_id, sync.clone(), latest.body.clone()));
            }
            let request = review_request(
                &key,
                &head_sha,
                event,
                summary.trim(),
                &drafts,
                &commentable,
                &|path| languages.get(path).cloned(),
            );
            let posted = if unmarked {
                Err(anyhow!(
                    "Couldn't save the drafts before posting, so nothing was sent."
                ))
            } else {
                runner.run(request).await.map_err(anyhow::Error::from)
            };

            if let Err(error) = posted {
                let review_open = this
                    .update(cx, |this, cx| {
                        this.submitting = false;
                        this.submit_error = Some(format!("{error:#}").into());
                        for (local_id, _) in &marked {
                            if let Some(index) = this.draft_index(*local_id) {
                                this.drafts[index].post_state = PostState::Unposted;
                                this.queue_save(*local_id, cx);
                            }
                        }
                        cx.notify();
                    })
                    .is_ok();
                // Nothing reached GitHub, so the marks are cleared even if the review closed.
                if !review_open {
                    for (local_id, sync) in &marked {
                        requeue_latest(sync, PostState::Unposted);
                        flush_sync(&this, *local_id, sync, &database, cx).await;
                    }
                }
                return Err(error);
            }

            let finished = this.update_in(cx, |this, window, cx| {
                this.submitting = false;
                // Emptied while waiting, so left out of the review.
                for (local_id, _) in &marked {
                    let left_out = sent.iter().all(|(sent_id, _, _)| sent_id != local_id);
                    if let Some(index) = this.draft_index(*local_id)
                        && left_out
                    {
                        this.drafts[index].post_state = PostState::Unposted;
                    }
                }
                let mut posted = Vec::new();
                for (local_id, sync, body) in &sent {
                    let Some(index) = this.draft_index(*local_id) else {
                        // Discarded while posting; its own delete is already queued.
                        continue;
                    };
                    if this.drafts[index].body == *body {
                        this.drafts.remove(index);
                        this.draft_views.remove(index);
                        posted.push((*local_id, sync.clone()));
                    } else {
                        // Edited while posting: the earlier text is on GitHub, the newer stays.
                        this.drafts[index].post_state = PostState::EarlierVersionPosted;
                        this.queue_save(*local_id, cx);
                    }
                }
                if this.summary_editor.read(cx).text(cx) == summary {
                    this.summary_editor
                        .update(cx, |editor, cx| editor.set_text("", window, cx));
                }
                this.show_comments(cx);
                cx.notify();
                posted
            });
            let review_open = finished.is_ok();
            if !review_open {
                clear_sent_text(
                    &summary_sync,
                    &summary,
                    &database,
                    &key,
                    SUMMARY_SLOT,
                    cx.background_executor(),
                );
            }
            let posted = match finished {
                Ok(posted) => posted,
                // The review closed while posting: each draft's latest queued version says
                // whether it was edited after it was sent.
                Err(_) => sent
                    .iter()
                    .filter(|(_, sync, body)| {
                        let latest_body = sync.lock().latest.as_ref().map(|d| d.body.clone());
                        let edited = latest_body
                            .is_some_and(|latest| latest != *body && !latest.trim().is_empty());
                        if edited {
                            requeue_latest(sync, PostState::EarlierVersionPosted);
                        }
                        !edited
                    })
                    .map(|(local_id, sync, _)| (*local_id, sync.clone()))
                    .collect(),
            };
            for (_, sync) in &posted {
                sync.lock().pending = Some(PendingWrite::DeleteRow);
            }
            for (local_id, sync, _) in &sent {
                flush_sync(&this, *local_id, sync, &database, cx).await;
            }

            let mut problem = match database.set_last_review(key, head_sha).await {
                Ok(()) => None,
                Err(error) => {
                    log::error!("recording the last review failed: {error:#}");
                    Some(format!("{error:#}"))
                }
            };
            if !review_open {
                return Ok(());
            }
            match this.update_in(cx, |this, window, cx| this.refresh_detail(window, cx)) {
                Ok(refresh) => {
                    if let Err(error) = refresh.await {
                        problem.get_or_insert(format!("{error:#}"));
                    }
                }
                Err(_) => return Ok(()),
            }
            if let Some(problem) = problem {
                this.update(cx, |this, cx| {
                    this.submit_note =
                        Some(format!("Review posted. Couldn't refresh: {problem}").into());
                    cx.notify();
                })
                .ok();
            }
            Ok(())
        })
    }

    fn refresh_detail(&self, window: &mut Window, cx: &mut Context<Self>) -> Task<Result<()>> {
        let key = self.detail.summary.key.clone();
        let request = detail_request(&key, &self.login);
        let runner = gh_runner(cx);
        cx.spawn_in(window, async move |this, cx| {
            let detail = parse_detail(&key, &runner.run(request).await?)?;
            this.update_in(cx, |this, window, cx| {
                if let Some(commit) = &detail.summary.last_review_commit {
                    this.last_review = Some(commit.clone());
                }
                this.detail = detail;
                this.add_reply_editors(&HashMap::new(), window, cx);
                this.show_comments(cx);
                cx.notify();
            })
        })
    }

    /// The Reply button: posts what is typed in the thread's reply box.
    pub fn click_reply(&mut self, thread_id: &str, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(editor) = self.reply_editors.get(thread_id) else {
            return Task::ready(Err(anyhow!("No reply box for this thread.")));
        };
        let body = editor.read(cx).text(cx);
        if body.trim().is_empty() {
            return Task::ready(Ok(()));
        }
        self.reply(thread_id, body, cx)
    }

    pub fn reply(
        &mut self,
        thread_id: &str,
        body: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(first_comment) = self
            .detail
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .and_then(|thread| thread.comments.first())
        else {
            return Task::ready(Err(anyhow!("This thread has no comment to reply to.")));
        };
        let key = self.detail.summary.key.clone();
        let request = reply_request(&key, first_comment.database_id, &body);
        let runner = gh_runner(cx);
        let thread_id = thread_id.to_string();
        let slot = reply_slot(&thread_id);
        let reply_sync = self.text_sync(&slot);
        let database = self.database.clone();
        cx.spawn(async move |this, cx| {
            let result = match runner.run(request).await {
                Ok(output) => parse_reply(&output),
                Err(error) => Err(error.into()),
            };
            // The review may have closed meanwhile; the stored reply still goes once it's posted.
            if result.is_ok() {
                clear_sent_text(
                    &reply_sync,
                    &body,
                    &database,
                    &key,
                    &slot,
                    cx.background_executor(),
                );
            }
            this.update(cx, |this, cx| {
                match &result {
                    Ok(comment) => {
                        this.thread_errors.remove(&thread_id);
                        if let Some(thread) = this
                            .detail
                            .threads
                            .iter_mut()
                            .find(|thread| thread.id == thread_id)
                        {
                            thread.comments.push(comment.clone());
                        }
                        let buffer = this
                            .reply_editors
                            .get(&thread_id)
                            .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton());
                        if let Some(buffer) = buffer
                            && buffer.read(cx).text() == body
                        {
                            buffer.update(cx, |buffer, cx| buffer.set_text("", cx));
                        }
                    }
                    Err(error) => {
                        this.thread_errors
                            .insert(thread_id.clone(), format!("{error:#}").into());
                    }
                }
                this.show_comments(cx);
                cx.notify();
            })?;
            result.map(|_| ())
        })
    }

    /// Flips the thread at once, as GitHub does, and flips it back if `gh` fails. A click while
    /// the thread's last request is pending sends nothing.
    pub fn set_thread_resolved(
        &mut self,
        thread_id: &str,
        resolved: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.resolving.contains(thread_id) {
            return Task::ready(Ok(()));
        }
        let Some(thread) = self
            .detail
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
        else {
            return Task::ready(Err(anyhow!("Unknown review thread {thread_id}.")));
        };
        thread.is_resolved = resolved;
        self.resolving.insert(thread_id.to_string());
        self.thread_errors.remove(thread_id);
        self.show_comments(cx);
        cx.notify();
        let request = resolve_request(&self.detail.summary.key.host, thread_id, resolved);
        let runner = gh_runner(cx);
        let thread_id = thread_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = match runner.run(request).await {
                Ok(output) => parse_mutation(&output),
                Err(error) => Err(error.into()),
            };
            this.update(cx, |this, cx| {
                this.resolving.remove(&thread_id);
                if let Err(error) = &result {
                    // A refresh may have brought GitHub's own state in the meantime.
                    if let Some(thread) = this
                        .detail
                        .threads
                        .iter_mut()
                        .find(|thread| thread.id == thread_id && thread.is_resolved == resolved)
                    {
                        thread.is_resolved = !resolved;
                    }
                    this.thread_errors
                        .insert(thread_id.clone(), format!("{error:#}").into());
                }
                this.show_comments(cx);
                cx.notify();
            })?;
            result
        })
    }

    /// Syncs GitHub's Viewed checkbox for a file: flips at once and flips back if `gh` fails. A
    /// click while the file's last request is pending sends nothing.
    pub fn set_file_viewed(
        &mut self,
        path: &str,
        viewed: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.full_files.is_some() || self.marking_viewed.contains(path) {
            return Task::ready(Ok(()));
        }
        self.set_local_file_viewed(path, viewed);
        self.marking_viewed.insert(path.to_string());
        self.file_errors.remove(path);
        cx.notify();
        let request = viewed_request(
            &self.detail.summary.key.host,
            &self.detail.summary.node_id,
            path,
            viewed,
        );
        let runner = gh_runner(cx);
        let path = path.to_string();
        cx.spawn(async move |this, cx| {
            let result = match runner.run(request).await {
                Ok(output) => parse_mutation(&output),
                Err(error) => Err(error.into()),
            };
            this.update(cx, |this, cx| {
                this.marking_viewed.remove(&path);
                if let Err(error) = &result {
                    if this.file_viewed(&path) == viewed {
                        this.set_local_file_viewed(&path, !viewed);
                    }
                    this.file_errors
                        .insert(path.clone(), format!("{error:#}").into());
                }
                cx.notify();
            })?;
            result
        })
    }

    fn set_local_file_viewed(&mut self, path: &str, viewed: bool) {
        match self.detail.files.iter_mut().find(|file| file.path == path) {
            Some(file) => file.viewed = viewed,
            None => self.detail.files.push(crate::PrFile {
                path: path.to_string(),
                viewed,
            }),
        }
    }

    fn file_viewed(&self, path: &str) -> bool {
        self.detail
            .files
            .iter()
            .any(|file| file.path == path && file.viewed)
    }

    /// Steps exist only in Photon, so their viewed state stays local.
    pub fn set_step_viewed(&mut self, step_key: &str, viewed: bool, cx: &mut Context<Self>) {
        if self.full_files.is_some() {
            return;
        }
        if viewed {
            self.viewed_steps.insert(step_key.to_string());
        } else {
            self.viewed_steps.remove(step_key);
        }
        cx.notify();
        let database = self.database.clone();
        let key = self.detail.summary.key.clone();
        let step_key = step_key.to_string();
        cx.background_spawn(async move { database.set_step_viewed(key, step_key, viewed).await })
            .detach_and_log_err(cx);
    }

    fn draft_index(&self, local_id: usize) -> Option<usize> {
        self.draft_views
            .iter()
            .position(|view| view.local_id == local_id)
    }

    fn draft_note(&self, draft: &Draft) -> Option<String> {
        landing_note(&place(draft, &self.commentable))
    }

    fn push_draft(&mut self, draft: Draft) -> usize {
        let local_id = self.next_draft_id;
        self.next_draft_id += 1;
        let sync = Arc::new(Mutex::new(DraftSync {
            id: draft.id,
            latest: Some(draft.clone()),
            ..DraftSync::default()
        }));
        self.drafts.push(draft);
        self.draft_views.push(DraftView {
            local_id,
            composer: None,
            sync,
            _composer_subscription: None,
        });
        local_id
    }

    /// Shows the draft as an editable composer holding its current body.
    fn open_composer(&mut self, local_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        if self.draft_views[index].composer.is_some() {
            return;
        }
        let body = self.drafts[index].body.clone();
        let composer = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, COMPOSER_MAX_LINES as usize, window, cx);
            editor.set_placeholder_text("Leave a comment", window, cx);
            editor.set_text(body, window, cx);
            editor
        });
        let subscription = cx.subscribe(&composer, move |this, editor, event: &EditorEvent, cx| {
            if !matches!(event, EditorEvent::BufferEdited) {
                return;
            }
            let body = editor.read(cx).text(cx);
            if let Some(index) = this.draft_index(local_id) {
                this.drafts[index].body = body;
                this.queue_save(local_id, cx);
                // The draft count and Comment's enabled state depend on the body.
                cx.notify();
            }
        });
        let view = &mut self.draft_views[index];
        view.composer = Some(composer.clone());
        view._composer_subscription = Some(subscription);
        self.show_comments(cx);
        window.focus(&composer.focus_handle(cx), cx);
        cx.notify();
    }

    pub fn edit_draft(&mut self, local_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.open_composer(local_id, window, cx);
    }

    fn queue_save(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        let draft = &self.drafts[index];
        let write = if draft.body.trim().is_empty() {
            PendingWrite::DeleteRow
        } else {
            PendingWrite::Save(draft.clone())
        };
        let sync = self.draft_views[index].sync.clone();
        {
            let mut state = sync.lock();
            state.pending = Some(write);
            state.latest = Some(draft.clone());
        }
        self.start_sync(local_id, sync, cx);
    }

    fn start_sync(&self, local_id: usize, sync: Arc<Mutex<DraftSync>>, cx: &mut Context<Self>) {
        if !claim_sync(&sync) {
            return;
        }
        let database = self.database.clone();
        // Detached so closing the review never cancels the write of what the user typed.
        cx.spawn(async move |this, cx| drain_sync(this, local_id, sync, database, cx).await)
            .detach();
    }

    fn discard_draft(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        self.drafts.remove(index);
        let view = self.draft_views.remove(index);
        {
            let mut state = view.sync.lock();
            state.pending = Some(PendingWrite::DeleteRow);
            state.latest = None;
        }
        self.start_sync(local_id, view.sync, cx);
        self.show_comments(cx);
        cx.notify();
    }

    pub fn cancel_draft(&mut self, local_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        if self.drafts[index].body.trim().is_empty() {
            self.discard_draft(local_id, cx);
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Discard this draft?",
            None,
            &["Discard", "Keep"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await.ok() == Some(0) {
                this.update(cx, |this, cx| this.discard_draft(local_id, cx))
                    .log_err();
            }
        })
        .detach();
    }

    fn collapse_draft(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        if self.drafts[index].body.trim().is_empty() {
            return;
        }
        // Saving again here retries a write that failed while typing.
        self.queue_save(local_id, cx);
        let view = &mut self.draft_views[index];
        view.composer = None;
        view._composer_subscription = None;
        self.show_comments(cx);
        cx.notify();
    }

    /// The newest selection's lines in the buffer under it: the head buffer of any excerpt
    /// (`Side::Right`), or the base text of deleted rows (`Side::Left`).
    fn selected_lines(&self, cx: &App) -> Option<SelectedLines> {
        let selection = *self.editor.read(cx).selections.newest_anchor();
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let start = selection.start.to_point(&snapshot);
        let mut end = selection.end.to_point(&snapshot);
        // A selection ending at the start of a line doesn't include that line.
        if end.row > start.row && end.column == 0 {
            end = Point::new(end.row - 1, 0);
        }
        let (buffer, start_point) = snapshot.point_to_buffer_point(Point::new(start.row, 0))?;
        let end_row = snapshot
            .point_to_buffer_point(Point::new(end.row, 0))
            .filter(|(end_buffer, _)| end_buffer.remote_id() == buffer.remote_id())
            .map_or(start_point.row, |(_, point)| point.row.max(start_point.row));
        let anchor_text = (start_point.row..=end_row)
            .map(|row| {
                buffer
                    .text_for_range(Point::new(row, 0)..Point::new(row, buffer.line_len(row)))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let buffer_id = buffer.remote_id();
        let base_file = self.files.iter().find(|file| {
            file.contents.as_ref().is_some_and(|(_, diff)| {
                diff.read(cx).base_text_buffer().read(cx).remote_id() == buffer_id
            })
        });
        let (path, side) = match base_file {
            Some(file) => (file.diff.path.clone(), Side::Left),
            None => {
                let buffer = self.multibuffer.read(cx).buffer(buffer_id)?;
                let path = buffer_path(&buffer, cx).or_else(|| {
                    self.files
                        .iter()
                        .find(|file| file.contents.as_ref().is_some_and(|(b, _)| *b == buffer))
                        .map(|file| file.diff.path.clone())
                })?;
                (path, Side::Right)
            }
        };
        Some(SelectedLines {
            path,
            side,
            start_line: start_point.row + 1,
            end_line: end_row + 1,
            anchor_text,
        })
    }

    fn add_comment(&mut self, _: &AddComment, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lines) = self.selected_lines(cx) else {
            return;
        };
        // The base text here is the reviewed commit's, so its line numbers are not GitHub's.
        if self.full_files.is_some() && lines.side == Side::Left {
            self.since_review_note = Some(LEFT_COMMENT_NOTE.into());
            cx.notify();
            return;
        }
        let draft = Draft {
            id: None,
            key: self.detail.summary.key.clone(),
            path: lines.path,
            side: lines.side,
            start_line: lines.start_line,
            end_line: lines.end_line,
            commit: self.checkout.head_sha.clone(),
            anchor_text: lines.anchor_text,
            body: String::new(),
            outdated: false,
            general: false,
            post_state: crate::PostState::Unposted,
        };
        let local_id = self.push_draft(draft);
        self.open_composer(local_id, window, cx);
    }

    /// Moves a draft to the current selection, which also makes an outdated draft current again.
    pub fn replace_draft(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let (Some(lines), Some(index)) = (self.selected_lines(cx), self.draft_index(local_id))
        else {
            return;
        };
        let draft = &mut self.drafts[index];
        draft.path = lines.path;
        draft.side = lines.side;
        draft.start_line = lines.start_line;
        draft.end_line = lines.end_line;
        draft.anchor_text = lines.anchor_text;
        draft.commit = self.checkout.head_sha.clone();
        draft.outdated = false;
        draft.general = false;
        self.queue_save(local_id, cx);
        self.show_comments(cx);
        cx.notify();
    }

    fn post_as_general(&mut self, local_id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        self.drafts[index].general = true;
        self.queue_save(local_id, cx);
        cx.notify();
    }

    /// Drafts written on an older head move to the same text at this head, or become outdated.
    fn reanchor_drafts(&self, cx: &mut Context<Self>) -> Task<()> {
        let head = &self.checkout.head_sha;
        let fs = self.project.read(cx).fs().clone();
        let mut jobs = Vec::new();
        for (draft, view) in self.drafts.iter().zip(&self.draft_views) {
            if draft.outdated || draft.commit == *head {
                continue;
            }
            let file = self.files.iter().find(|file| file.diff.path == draft.path);
            let contents = file.and_then(|file| file.contents.as_ref());
            // `Ok(None)` means the file is gone; an error leaves the draft for the next open.
            let text: Task<Result<Option<String>>> = match (draft.side, contents) {
                (Side::Left, contents) => Task::ready(Ok(
                    contents.and_then(|(_, diff)| diff.read(cx).base_text_string(cx))
                )),
                (Side::Right, Some((buffer, _))) => Task::ready(Ok(Some(buffer.read(cx).text()))),
                (Side::Right, None) if file.is_some() => Task::ready(Ok(None)),
                (Side::Right, None) => {
                    let path = self.checkout.worktree.join(&draft.path);
                    let fs = fs.clone();
                    cx.background_spawn(async move {
                        match fs.metadata(&path).await? {
                            None => Ok(None),
                            Some(_) => fs.load(&path).await.map(Some),
                        }
                    })
                }
            };
            jobs.push((
                view.local_id,
                draft.path.clone(),
                draft.anchor_text.clone(),
                draft.start_line,
                text,
            ));
        }
        cx.spawn(async move |this, cx| {
            for (local_id, path, anchor_text, start_line, text) in jobs {
                let text = match text.await {
                    Ok(text) => text,
                    Err(error) => {
                        log::warn!("can't read {path} to re-anchor a review draft: {error:#}");
                        continue;
                    }
                };
                let result = reanchor(&anchor_text, start_line, text.as_deref());
                this.update(cx, |this, cx| this.apply_reanchor(local_id, result, cx))
                    .log_err();
            }
        })
    }

    fn apply_reanchor(&mut self, local_id: usize, result: Reanchor, cx: &mut Context<Self>) {
        let Some(index) = self.draft_index(local_id) else {
            return;
        };
        let result = match result {
            Reanchor::Placed {
                start_line,
                end_line,
            } if self.drafts[index].side == Side::Left
                && !self.on_deleted_rows(&self.drafts[index].path, start_line, end_line) =>
            {
                Reanchor::Outdated
            }
            result => result,
        };
        let draft = &mut self.drafts[index];
        match result {
            Reanchor::Placed {
                start_line,
                end_line,
            } => {
                draft.start_line = start_line;
                draft.end_line = end_line;
                draft.commit = self.checkout.head_sha.clone();
            }
            Reanchor::Outdated => draft.outdated = true,
        }
        self.queue_save(local_id, cx);
        self.show_comments(cx);
        cx.notify();
    }

    fn on_deleted_rows(&self, path: &str, start_line: u32, end_line: u32) -> bool {
        self.files
            .iter()
            .find(|file| file.diff.path == path)
            .is_some_and(|file| {
                deleted_lines(&file.diff)
                    .iter()
                    .any(|rows| rows.contains(&start_line) && rows.contains(&end_line))
            })
    }

    /// The multibuffer position at the end of a 1-based line, when that line is shown.
    fn line_anchor(
        &self,
        path: &str,
        side: Side,
        line: u32,
        snapshot: &MultiBufferSnapshot,
        cx: &App,
    ) -> Option<Anchor> {
        let row = line.checked_sub(1)?;
        match side {
            Side::Right => {
                let buffer = self.buffer_for_path(path, cx)?.read(cx).snapshot();
                if row > buffer.max_point().row {
                    return None;
                }
                snapshot
                    .anchor_in_excerpt(buffer.anchor_before(Point::new(row, buffer.line_len(row))))
            }
            Side::Left => {
                let file = self.files.iter().find(|file| file.diff.path == path)?;
                let (buffer, diff) = file.contents.as_ref()?;
                let buffer = buffer.read(cx).snapshot();
                let diff = diff.read(cx).snapshot(cx);
                let base = diff.base_text();
                if row > base.max_point().row {
                    return None;
                }
                let base_point = Point::new(row, base.line_len(row));
                let point = diff.base_text_point_to_buffer_point(base_point, &buffer);
                let anchor = snapshot.anchor_in_excerpt(buffer.anchor_before(point))?;
                Some(anchor.with_diff_base_anchor(base.anchor_before(base_point)))
            }
        }
    }

    fn show_comments(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let item = cx.weak_entity();
        let mut blocks = Vec::new();
        for (draft, view) in self.drafts.iter().zip(&self.draft_views) {
            if draft.outdated || draft.general {
                continue;
            }
            let Some(anchor) =
                self.line_anchor(&draft.path, draft.side, draft.end_line, &snapshot, cx)
            else {
                continue;
            };
            let note = self.draft_note(draft);
            let posted_note = self.posted_note(draft);
            let body_rows = draft.body.lines().count().max(1) as u32;
            let height = match view.composer {
                Some(_) => COMPOSER_MAX_LINES + 4,
                None => 3 + body_rows,
            } + if note.is_some() { 2 } else { 0 }
                + if posted_note.is_some() { 2 } else { 0 };
            blocks.push(BlockProperties {
                placement: BlockPlacement::Below(anchor),
                height: Some(height),
                style: BlockStyle::Sticky,
                render: draft_renderer(
                    item.clone(),
                    view.local_id,
                    draft.end_line,
                    draft.body.clone().into(),
                    note.map(SharedString::from),
                    posted_note,
                    view.composer.clone(),
                    view.sync.clone(),
                ),
                priority: 0,
            });
        }
        for thread in &self.detail.threads {
            let Some(line) = thread.line else {
                continue;
            };
            let Some(anchor) = self.line_anchor(&thread.path, thread.side, line, &snapshot, cx)
            else {
                continue;
            };
            let body_rows: usize = thread
                .comments
                .iter()
                .map(|comment| 1 + comment.body.lines().count().max(1))
                .sum();
            let error = self.thread_errors.get(&thread.id).cloned();
            let pending = self.resolving.contains(&thread.id);
            blocks.push(BlockProperties {
                placement: BlockPlacement::Below(anchor),
                height: Some(body_rows as u32 + 3 + u32::from(error.is_some())),
                style: BlockStyle::Sticky,
                render: thread_renderer(
                    item.clone(),
                    thread.clone(),
                    self.reply_editors.get(&thread.id).cloned(),
                    error,
                    pending,
                ),
                priority: 0,
            });
        }
        let stale = std::mem::take(&mut self.comment_blocks);
        self.comment_blocks = self.editor.update(cx, |editor, cx| {
            editor.remove_blocks(stale.into_iter().collect(), None, cx);
            editor.insert_blocks(blocks, None, cx).into_iter().collect()
        });
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn selected_step(&self) -> usize {
        self.selected
    }

    pub fn select_step(&mut self, index: usize, _window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.steps.len() || index == self.selected {
            return;
        }
        self.selected = index;
        self.show_step(cx);
        cx.notify();
    }

    pub fn context_state(&self) -> &ContextState {
        &self.context_state
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn impact(&self) -> &[ImpactRow] {
        &self.impact
    }

    pub fn expand_context(&mut self, excerpt_index: usize, cx: &mut Context<Self>) {
        let Some(excerpt) = self.shown_context.get(excerpt_index) else {
            return;
        };
        self.expanded_context
            .insert((excerpt.buffer.entity_id(), excerpt.range.start));
        self.show_step(cx);
        cx.notify();
    }

    fn load_context(&mut self, cx: &mut Context<Self>) -> Task<()> {
        self.context_state = ContextState::Finding;
        cx.notify();
        let analyses: Vec<_> = self
            .files
            .iter()
            .map(|file| analyse_loaded_file(file, cx))
            .collect();
        cx.spawn(async move |this, cx| {
            let analyses = join_all(analyses).await;
            let Some(fetches) = this
                .update(cx, |this, cx| {
                    this.set_analyses(analyses, cx);
                    // Language servers don't start in a restricted worktree, and a PR's code is
                    // never trusted automatically, so the fetches (and their 30 s cap) wait for
                    // the user to trust it.
                    if this.worktree_restricted(cx) {
                        this.context_state = ContextState::Unavailable(TRUST_REASON.into());
                        cx.notify();
                        return None;
                    }
                    Some(this.fetch_contexts(cx))
                })
                .log_err()
                .flatten()
            else {
                return;
            };
            let results = join_all(fetches).await;
            this.update(cx, |this, cx| this.set_contexts(results, cx))
                .log_err();
        })
    }

    fn worktree_restricted(&self, cx: &mut Context<Self>) -> bool {
        let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
            return false;
        };
        let worktree_store = self.project.read(cx).worktree_store();
        let worktree_ids: Vec<_> = self
            .project
            .read(cx)
            .worktrees(cx)
            .map(|worktree| worktree.read(cx).id())
            .collect();
        trusted_worktrees.update(cx, |trusted_worktrees, cx| {
            worktree_ids
                .into_iter()
                .any(|id| !trusted_worktrees.can_trust(&worktree_store, id, cx))
        })
    }

    fn set_analyses(&mut self, analyses: Vec<Option<FileAnalysis>>, cx: &mut Context<Self>) {
        self.functions = analyses
            .iter()
            .flatten()
            .flat_map(|analysis| match analysis {
                FileAnalysis::Functions { functions, .. } => functions.clone(),
                FileAnalysis::Unanalysable => Vec::new(),
            })
            .collect();
        self.contexts = vec![None; self.functions.len()];
        self.analyses = analyses;
        self.rebuild_map(&[], cx);
    }

    fn fetch_contexts(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Vec<Task<Result<FunctionContext, ContextError>>> {
        let mut fetches = Vec::with_capacity(self.functions.len());
        for function in &self.functions {
            let fetch = match buffer_for(&self.files, &function.path) {
                Some(buffer) => self.context_cache.fetch(
                    &self.checkout.head_sha,
                    self.project.clone(),
                    buffer.clone(),
                    function,
                    cx,
                ),
                None => Task::ready(Err(ContextError::NoCallHierarchy)),
            };
            fetches.push(fetch);
        }
        fetches
    }

    fn set_contexts(
        &mut self,
        results: Vec<Result<FunctionContext, ContextError>>,
        cx: &mut Context<Self>,
    ) {
        let failure = if results.iter().any(Result::is_ok) {
            None
        } else {
            results
                .iter()
                .find_map(|result| result.as_ref().err().copied())
        };
        self.context_state = match failure {
            Some(error) => ContextState::Unavailable(unavailable_reason(error).into()),
            None => ContextState::Ready,
        };
        self.contexts = results.into_iter().map(Result::ok).collect();
        let edges = call_edges(&self.functions, &self.contexts, cx);
        self.rebuild_map(&edges, cx);
        self.update_impact(cx);
    }

    fn map_inputs(&self) -> (Vec<MapFile>, Vec<MapFunction>) {
        let files = self
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let mut kind = classify_path(&file.diff.path);
                let unanalysable = file.contents.is_none()
                    || matches!(
                        self.analyses.get(index),
                        Some(None | Some(FileAnalysis::Unanalysable))
                    );
                if kind == FileKind::Code && unanalysable {
                    kind = FileKind::Unanalysable;
                }
                MapFile {
                    path: file.diff.path.clone(),
                    kind,
                    status_label: status_label(&file.diff),
                }
            })
            .collect();
        let functions = self
            .functions
            .iter()
            .map(|function| MapFunction {
                path: function.path.clone(),
                name: function.name.clone(),
                first_row: function.range.start.row,
            })
            .collect();
        (files, functions)
    }

    fn rebuild_map(&mut self, edges: &[(usize, usize)], cx: &mut Context<Self>) {
        let (files, functions) = self.map_inputs();
        let selected_key = self.steps.get(self.selected).map(|step| step.key.clone());
        self.steps = build_review_map(&files, &functions, edges);
        self.selected = selected_key
            .and_then(|key| self.steps.iter().position(|step| step.key == key))
            .unwrap_or_else(|| self.selected.min(self.steps.len().saturating_sub(1)));
        self.show_step(cx);
        cx.notify();
    }

    /// The file's changed rows, minus rows that belong only to functions of other steps.
    fn step_rows(&self, step: &Step, path: &str) -> Vec<Range<u32>> {
        let Some(rows) = self.changed_rows.get(path) else {
            return Vec::new();
        };
        rows.iter()
            .filter(|rows| {
                let mut owners = self
                    .functions
                    .iter()
                    .enumerate()
                    .filter(|(_, function)| {
                        function.path == path
                            && function.range.start.row < rows.end
                            && rows.start <= function.range.end.row
                    })
                    .map(|(index, _)| index)
                    .peekable();
                owners.peek().is_none() || owners.any(|index| step.functions.contains(&index))
            })
            .cloned()
            .collect()
    }

    fn show_step(&mut self, cx: &mut Context<Self>) {
        let step = self.steps.get(self.selected).cloned();
        let mut entries: Vec<(PathKey, Entity<Buffer>, Vec<ExcerptRange<Point>>)> = Vec::new();
        let mut shown_context: Vec<ContextExcerpt> = Vec::new();
        if let Some(step) = &step {
            for path in &step.files {
                let Some(buffer) = buffer_for(&self.files, path) else {
                    continue;
                };
                let Some(key) = path_key(entries.len(), buffer, path, cx) else {
                    continue;
                };
                let snapshot = buffer.read(cx).snapshot();
                let max_row = snapshot.max_point().row;
                let ranges = self.step_rows(step, path).into_iter().map(|rows| {
                    let start = rows.start.min(max_row);
                    let end = rows.end.saturating_sub(1).clamp(start, max_row);
                    Point::new(start, 0)..Point::new(end, snapshot.line_len(end))
                });
                let ranges = build_excerpt_ranges(ranges, HUNK_CONTEXT_LINES, &snapshot);
                entries.push((key, buffer.clone(), ranges));
            }

            for &index in &step.functions {
                let (Some(function), Some(Some(context))) =
                    (self.functions.get(index), self.contexts.get(index))
                else {
                    continue;
                };
                for mut excerpt in context_excerpts(function, context, &self.changed_rows, cx) {
                    let duplicate = shown_context.iter().any(|shown| {
                        shown.buffer == excerpt.buffer && shown.range.start == excerpt.range.start
                    });
                    if duplicate {
                        continue;
                    }
                    let expanded = self
                        .expanded_context
                        .contains(&(excerpt.buffer.entity_id(), excerpt.range.start));
                    if expanded {
                        let snapshot = excerpt.buffer.read(cx).snapshot();
                        let last_row = (excerpt.range.start.row + excerpt.full_rows - 1)
                            .min(snapshot.max_point().row);
                        excerpt.range.end = Point::new(last_row, snapshot.line_len(last_row));
                    }
                    shown_context.push(excerpt);
                }
            }
        }

        // A buffer can sit under only one path key, so context inside an already shown file
        // joins that file's ranges.
        for excerpt in &shown_context {
            let range = ExcerptRange {
                context: excerpt.range.clone(),
                primary: excerpt.range.clone(),
            };
            if let Some(entry) = entries.iter_mut().find(|entry| entry.1 == excerpt.buffer) {
                entry.2.push(range);
                continue;
            }
            let Some(path) = buffer_path(&excerpt.buffer, cx) else {
                continue;
            };
            let Some(key) = path_key(entries.len(), &excerpt.buffer, &path, cx) else {
                continue;
            };
            entries.push((key, excerpt.buffer.clone(), vec![range]));
        }

        let keys: Vec<PathKey> = entries.iter().map(|entry| entry.0.clone()).collect();
        let stale: Vec<PathKey> = self
            .shown_paths
            .iter()
            .filter(|key| !keys.contains(key))
            .cloned()
            .collect();
        let diffs: Vec<Entity<BufferDiff>> = self
            .files
            .iter()
            .filter_map(|file| file.contents.as_ref())
            .filter(|(buffer, _)| entries.iter().any(|entry| &entry.1 == buffer))
            .map(|(_, diff)| diff.clone())
            .collect();
        self.multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.remove_excerpts_for_paths(stale, cx);
            for (key, buffer, ranges) in entries {
                let snapshot = buffer.read(cx).snapshot();
                multibuffer.set_excerpt_ranges_for_path(key, buffer, &snapshot, ranges, cx);
            }
            for diff in diffs {
                multibuffer.add_diff(diff, cx);
            }
        });
        self.shown_paths = keys;

        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let item = cx.weak_entity();
        let mut blocks = Vec::new();
        let mut highlights = Vec::new();
        for (index, excerpt) in shown_context.iter().enumerate() {
            let buffer = excerpt.buffer.read(cx).snapshot();
            let Some(start) = snapshot.anchor_in_excerpt(buffer.anchor_before(excerpt.range.start))
            else {
                continue;
            };
            let end = snapshot
                .anchor_in_excerpt(buffer.anchor_after(excerpt.range.end))
                .unwrap_or(start);
            let collapsed = excerpt.range.end.row - excerpt.range.start.row + 1 < excerpt.full_rows;
            let label = if collapsed {
                format!(
                    "{} · collapsed · {} lines",
                    excerpt.label, excerpt.full_rows
                )
            } else {
                excerpt.label.clone()
            };
            blocks.push(BlockProperties {
                placement: BlockPlacement::Above(start),
                height: Some(1),
                style: BlockStyle::Flex,
                render: label_renderer(label.into(), collapsed, index, item.clone()),
                priority: 0,
            });
            highlights.push(start..end);
        }
        let stale_blocks = std::mem::take(&mut self.label_blocks);
        self.label_blocks = self.editor.update(cx, |editor, cx| {
            editor.remove_blocks(stale_blocks.into_iter().collect(), None, cx);
            editor.clear_row_highlights::<UnchangedContext>();
            for range in highlights {
                editor.highlight_rows::<UnchangedContext>(
                    range,
                    |_| rgb(UNCHANGED_BACKGROUND).into(),
                    RowHighlightOptions::default(),
                    cx,
                );
            }
            editor.insert_blocks(blocks, None, cx).into_iter().collect()
        });
        self.shown_context = shown_context;
        self.show_comments(cx);
    }

    fn update_impact(&mut self, cx: &mut Context<Self>) {
        let head = self.editor.read(cx).selections.newest_anchor().head();
        let target = self.multibuffer.read(cx).point_to_buffer_point(head, cx);
        let function_index = target.and_then(|(buffer, point)| {
            self.functions
                .iter()
                .enumerate()
                .filter(|(_, function)| {
                    buffer_for(&self.files, &function.path) == Some(&buffer)
                        && function.range.start <= point
                        && point <= function.range.end
                })
                .max_by_key(|(_, function)| function.range.start)
                .map(|(index, _)| index)
        });
        let function = function_index.and_then(|index| self.functions.get(index));
        self.impact_function = function.map(|function| function.name.clone());
        let context = function_index
            .and_then(|index| self.contexts.get(index))
            .and_then(Option::as_ref);
        self.impact = match (function, context) {
            (Some(function), Some(context)) => context
                .callers
                .iter()
                .map(|call| {
                    let path = buffer_path(&call.from.buffer, cx).unwrap_or_default();
                    let snapshot = call.from.buffer.read(cx).snapshot();
                    let row = call
                        .from_ranges
                        .iter()
                        .map(|location| {
                            language::ToPoint::to_point(
                                &location.range.start,
                                &location.buffer.read(cx).snapshot(),
                            )
                            .row
                        })
                        .min()
                        .unwrap_or_else(|| {
                            language::ToPoint::to_point(&call.from.range.start, &snapshot).row
                        });
                    ImpactRow {
                        location: format!("{path}:{}", row + 1),
                        mark: classify_caller(
                            Some(function.change),
                            caller_touched(call, &self.changed_rows, cx),
                        ),
                    }
                })
                .collect(),
            _ => Vec::new(),
        };
        cx.notify();
    }

    fn progress_text(&self) -> String {
        let viewed = self
            .steps
            .iter()
            .filter(|step| self.viewed_steps.contains(&step.key))
            .count();
        format!("{viewed} of {} steps viewed", self.steps.len())
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let summary = &self.detail.summary;
        let checks = match summary.checks {
            ChecksState::Passing => Some(("checks passing", Color::Success)),
            ChecksState::Failing => Some(("checks failing", Color::Error)),
            ChecksState::Pending => Some(("checks pending", Color::Warning)),
            ChecksState::Unknown => None,
        };
        h_flex()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(format!(
                "#{} {}",
                summary.key.number, summary.title
            )))
            .child(
                Label::new(format!("{} → {}", summary.author, summary.base_ref))
                    .size(LabelSize::Small)
                    .color(Color::Accent),
            )
            .children(
                checks.map(|(text, color)| Label::new(text).size(LabelSize::Small).color(color)),
            )
            .child(
                Label::new(format!(
                    "{} files · +{} −{}",
                    self.files.len(),
                    self.detail.additions,
                    self.detail.deletions
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .when(self.changes_since_review_offered(), |header| {
                let showing = self.showing_changes_since_review();
                header.child(
                    Button::new(
                        "since-review",
                        if self.loading_since_review {
                            "Loading changes… (click to cancel)"
                        } else {
                            "Changes since your review"
                        },
                    )
                    .toggle_state(showing)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_changes_since_review(&ToggleChangesSinceReview, window, cx)
                    })),
                )
            })
            .children(self.since_review_note.clone().map(|note| {
                Label::new(note)
                    .size(LabelSize::Small)
                    .color(Color::Warning)
            }))
            .child(
                div().ml_auto().child(
                    Label::new(self.progress_text())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
    }

    fn render_map(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let function_row = |name: &str, color: Color| {
            h_flex()
                .pl_6()
                .gap_1p5()
                .child(div().size_1p5().rounded_full().bg(color.color(cx)))
                .child(
                    Label::new(format!("{name}()"))
                        .size(LabelSize::Small)
                        .color(color),
                )
        };
        let mut map = v_flex()
            .id("photon-review-map")
            .w(px(250.))
            .flex_none()
            .h_full()
            .p_2()
            .gap_1()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(colors.border)
            .child(
                Label::new("Review map")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        map = map.children(self.render_outdated_drafts(cx));
        for (index, step) in self.steps.iter().enumerate() {
            let selected = index == self.selected;
            let title = if self.viewed_steps.contains(&step.key) {
                format!("✓ {}", step.title)
            } else {
                step.title.clone()
            };
            map = map.child(
                h_flex()
                    .id(("photon-review-step", index))
                    .gap_2()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(colors.element_selected))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.select_step(index, window, cx)),
                    )
                    .child(
                        Label::new((index + 1).to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(Label::new(title).truncate())
                            .child(
                                Label::new(step.subtitle.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    ),
            );
            if selected {
                let step_key = step.key.clone();
                map = map.child(
                    div().pl_6().child(
                        Checkbox::new(
                            ("photon-review-step-viewed", index),
                            self.viewed_steps.contains(&step.key).into(),
                        )
                        .label("Step viewed")
                        .label_size(LabelSize::Small)
                        .disabled(self.full_files.is_some())
                        .when(self.full_files.is_some(), |checkbox| {
                            checkbox.tooltip(Tooltip::text(VIEWED_DISABLED_TOOLTIP))
                        })
                        .on_click(cx.listener(
                            move |this, state: &ToggleState, _, cx| {
                                this.set_step_viewed(&step_key, *state == ToggleState::Selected, cx)
                            },
                        )),
                    ),
                );
                for path in &step.files {
                    let file_path = path.clone();
                    map = map.child(
                        v_flex()
                            .pl_6()
                            .child(
                                Checkbox::new(
                                    SharedString::from(format!("photon-review-viewed-{path}")),
                                    self.file_viewed(path).into(),
                                )
                                .label(format!("{path} viewed"))
                                .label_size(LabelSize::Small)
                                .disabled(
                                    self.marking_viewed.contains(path) || self.full_files.is_some(),
                                )
                                .when(self.full_files.is_some(), |checkbox| {
                                    checkbox.tooltip(Tooltip::text(VIEWED_DISABLED_TOOLTIP))
                                })
                                .on_click(cx.listener(
                                    move |this, state: &ToggleState, _, cx| {
                                        this.set_file_viewed(
                                            &file_path,
                                            *state == ToggleState::Selected,
                                            cx,
                                        )
                                        .detach_and_log_err(cx)
                                    },
                                )),
                            )
                            .children(self.file_errors.get(path).map(|error| {
                                Label::new(error.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Error)
                            })),
                    );
                }
                for function in step
                    .functions
                    .iter()
                    .filter_map(|&index| self.functions.get(index))
                {
                    map = map.child(function_row(&function.name, Color::Accent));
                }
                let mut neighbours: Vec<&str> = Vec::new();
                for excerpt in &self.shown_context {
                    if !neighbours.contains(&excerpt.name.as_str()) {
                        neighbours.push(&excerpt.name);
                    }
                }
                for name in neighbours {
                    map = map.child(function_row(name, Color::Disabled));
                }
            }
        }
        let legend_row = |text: &'static str, color: Color| {
            h_flex()
                .gap_1p5()
                .child(div().size_1p5().rounded_full().bg(color.color(cx)))
                .child(Label::new(text).size(LabelSize::Small).color(color))
        };
        map.child(
            v_flex()
                .pt_4()
                .gap_1()
                .child(
                    Label::new("Legend")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(legend_row("changed in this PR", Color::Accent))
                .child(legend_row(
                    "unchanged, pulled in by the call graph",
                    Color::Disabled,
                )),
        )
    }

    fn render_outdated_drafts(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let outdated: Vec<_> = self
            .drafts
            .iter()
            .zip(&self.draft_views)
            .filter(|(draft, _)| draft.outdated)
            .collect();
        if outdated.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        Some(
            v_flex()
                .gap_1()
                .pb_2()
                .child(
                    Label::new("Outdated drafts")
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
                .children(outdated.into_iter().map(|(draft, view)| {
                    let local_id = view.local_id;
                    let posted_note = self.posted_note(draft);
                    v_flex()
                        .id(("photon-review-outdated", local_id))
                        .gap_1()
                        .p_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(colors.border_variant)
                        .child(
                            Label::new(format!("{}:{}", draft.path, draft.start_line))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(draft.anchor_text.clone())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx),
                        )
                        .child(Label::new(draft.body.clone()).size(LabelSize::Small))
                        .child(
                            h_flex()
                                .gap_1()
                                .flex_wrap()
                                .child(Button::new("replace", "Re-place here").on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.replace_draft(local_id, cx)
                                    }),
                                ))
                                // A draft that may already be posted is included again
                                // before anything else can send it.
                                .map(|this| match posted_note {
                                    Some(_) => this.child(
                                        Button::new("include", "Include again").on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.include_again(local_id, cx)
                                            }),
                                        ),
                                    ),
                                    None if draft.general => this.child(
                                        Label::new("posts as a general comment")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                                    None => this.child(
                                        Button::new("general", "Post as general comment").on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.post_as_general(local_id, cx)
                                            }),
                                        ),
                                    ),
                                })
                                .child(Button::new("delete", "Delete").on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.cancel_draft(local_id, window, cx)
                                    },
                                ))),
                        )
                        .children(posted_note.map(|note| {
                            Label::new(note)
                                .size(LabelSize::XSmall)
                                .color(Color::Warning)
                        }))
                }))
                .into_any_element(),
        )
    }

    fn render_code(&self) -> impl IntoElement {
        let status = match &self.context_state {
            ContextState::Finding => Some(
                h_flex().px_2().py_1().child(
                    Label::new("finding callers…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            ),
            ContextState::Ready => None,
            ContextState::Unavailable(reason) => Some(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .child(
                        Label::new("no call information")
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    )
                    .child(
                        Label::new(reason.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when(reason.as_ref() == TRUST_REASON, |this| {
                        this.child(
                            Button::new("photon-review-trust", "Trust worktree…")
                                .style(ButtonStyle::Filled)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(ToggleWorktreeSecurity.boxed_clone(), cx)
                                }),
                        )
                    }),
            ),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .children(status)
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }

    fn render_impact(&self, cx: &Context<Self>) -> impl IntoElement {
        let title = match &self.impact_function {
            Some(name) => format!("Impact of {name}()"),
            None => "Impact".to_string(),
        };
        let highlight = |word: &str, color: Color| {
            IMPACT_LEGEND.find(word).map(|start| {
                (
                    start..start + word.len(),
                    HighlightStyle {
                        color: Some(color.color(cx)),
                        ..HighlightStyle::default()
                    },
                )
            })
        };
        let submit = |event: ReviewEvent| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                this.submit(event, window, cx).detach_and_log_err(cx)
            })
        };
        let left_out = self.left_out_note();
        let legend_highlights: Vec<_> = [
            highlight("Red", Color::Error),
            highlight("Amber", Color::Warning),
        ]
        .into_iter()
        .flatten()
        .collect();
        v_flex()
            .w(px(260.))
            .flex_none()
            .h_full()
            .p_2()
            .gap_1()
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
            .children(self.impact.iter().map(|row| {
                let (tag, color) = mark_tag(row.mark);
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(Label::new(row.location.clone()).size(LabelSize::Small))
                    .child(Label::new(tag).size(LabelSize::XSmall).color(color))
            }))
            .child(
                div()
                    .text_xs()
                    .text_color(Color::Muted.color(cx))
                    .child(StyledText::new(IMPACT_LEGEND).with_highlights(legend_highlights)),
            )
            .child(
                v_flex()
                    .pt_4()
                    .gap_1()
                    .child(
                        Label::new("Your review")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!(
                            "{} draft comment{} · {}",
                            self.drafts.len(),
                            if self.drafts.len() == 1 { "" } else { "s" },
                            self.progress_text()
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(self.summary_editor.clone())
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .child(
                                Button::new("photon-review-comment", "Comment")
                                    .disabled(self.submitting || !self.can_comment(cx))
                                    .on_click(submit(ReviewEvent::Comment)),
                            )
                            .child(
                                Button::new("photon-review-approve", "Approve")
                                    .style(ButtonStyle::Filled)
                                    .disabled(self.submitting)
                                    .on_click(submit(ReviewEvent::Approve)),
                            )
                            .child(
                                Button::new("photon-review-request-changes", "Request changes")
                                    .disabled(self.submitting)
                                    .on_click(submit(ReviewEvent::RequestChanges)),
                            ),
                    )
                    .children(left_out.map(|note| {
                        Label::new(note)
                            .size(LabelSize::Small)
                            .color(Color::Warning)
                    }))
                    .children(
                        self.submit_note.clone().map(|note| {
                            Label::new(note).size(LabelSize::Small).color(Color::Muted)
                        }),
                    )
                    .children(
                        self.submit_error.clone().map(|error| {
                            Label::new(error).size(LabelSize::Small).color(Color::Error)
                        }),
                    ),
            )
    }
}

impl Render for ReviewItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PhotonReview")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(|this, _: &NextStep, window, cx| {
                this.select_step(this.selected + 1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousStep, window, cx| {
                this.select_step(this.selected.saturating_sub(1), window, cx)
            }))
            .on_action(cx.listener(Self::add_comment))
            .on_action(cx.listener(Self::toggle_changes_since_review))
            .child(self.render_header(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(self.render_map(cx))
                    .child(self.render_code())
                    .child(self.render_impact(cx)),
            )
    }
}

impl EventEmitter<()> for ReviewItem {}

impl Focusable for ReviewItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl Item for ReviewItem {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        let summary = &self.detail.summary;
        format!("#{} {}", summary.key.number, summary.title).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PullRequest))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "synchronous fixture setup")]
mod tests {
    use super::*;
    use crate::{FakeGh, FileStatus, PrKey, ReviewDb, build_loaded_file, parse_detail};
    use editor::MultiBufferOffset;
    use fs::FakeFs;
    use futures::FutureExt as _;
    use gpui::{TestAppContext, VisualTestContext};
    use language::FakeLspAdapter;
    use project::Project;
    use project::trusted_worktrees::PathTrust;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use util::path;
    use workspace::MultiWorkspace;

    const DETAIL: &str = include_str!("../test_fixtures/detail.json");
    const AUTH: &str =
        r#"{"hosts":{"github.com":[{"state":"success","active":true,"login":"alice"}]}}"#;

    struct GraphFunction {
        name: &'static str,
        path: &'static str,
        start: u32,
        end: u32,
    }

    struct GraphCall {
        caller: &'static str,
        callee: &'static str,
        row: u32,
    }

    struct Graph {
        functions: Vec<GraphFunction>,
        calls: Vec<GraphCall>,
    }

    struct PrFile {
        path: &'static str,
        base: Option<&'static str>,
        status: FileStatus,
        hunks: Vec<crate::Hunk>,
    }

    fn function(name: &'static str, path: &'static str, start: u32, end: u32) -> GraphFunction {
        GraphFunction {
            name,
            path,
            start,
            end,
        }
    }

    fn call(caller: &'static str, callee: &'static str, row: u32) -> GraphCall {
        GraphCall {
            caller,
            callee,
            row,
        }
    }

    fn modified(path: &'static str, base: &'static str) -> PrFile {
        PrFile {
            path,
            base: Some(base),
            status: FileStatus::Modified,
            hunks: Vec::new(),
        }
    }

    fn uri(path: &str) -> lsp::Uri {
        lsp::Uri::from_file_path(format!("{}/{path}", path!("/repo"))).expect("uri")
    }

    fn lsp_item(function: &GraphFunction) -> lsp::CallHierarchyItem {
        lsp::CallHierarchyItem {
            name: function.name.to_string(),
            kind: lsp::SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: uri(function.path),
            range: lsp::Range::new(
                lsp::Position::new(function.start, 0),
                lsp::Position::new(function.end, 1),
            ),
            selection_range: lsp::Range::new(
                lsp::Position::new(function.start, 3),
                lsp::Position::new(function.start, 3 + function.name.len() as u32),
            ),
            data: None,
        }
    }

    fn call_range(row: u32) -> lsp::Range {
        lsp::Range::new(lsp::Position::new(row, 4), lsp::Position::new(row, 10))
    }

    type Gate = futures::future::Shared<futures::channel::oneshot::Receiver<()>>;

    fn answer_call_hierarchy(
        server: &mut lsp::FakeLanguageServer,
        graph: &Arc<Graph>,
        gate: Gate,
        prepares: &Arc<AtomicUsize>,
    ) {
        server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
            let graph = graph.clone();
            let prepares = prepares.clone();
            move |params, _| {
                prepares.fetch_add(1, Ordering::SeqCst);
                let position = params.text_document_position_params;
                let item = graph
                    .functions
                    .iter()
                    .find(|function| {
                        uri(function.path) == position.text_document.uri
                            && function.start <= position.position.line
                            && position.position.line <= function.end
                    })
                    .map(lsp_item);
                let gate = gate.clone();
                async move {
                    gate.await.ok();
                    Ok(item.map(|item| vec![item]))
                }
            }
        });
        server.set_request_handler::<lsp::request::CallHierarchyOutgoingCalls, _, _>({
            let graph = graph.clone();
            move |params, _| {
                let calls = graph
                    .calls
                    .iter()
                    .filter(|call| call.caller == params.item.name)
                    .filter_map(|call| {
                        let callee = graph.functions.iter().find(|f| f.name == call.callee)?;
                        Some(lsp::CallHierarchyOutgoingCall {
                            to: lsp_item(callee),
                            from_ranges: vec![call_range(call.row)],
                        })
                    })
                    .collect();
                async move { Ok(Some(calls)) }
            }
        });
        server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
            let graph = graph.clone();
            move |params, _| {
                let calls = graph
                    .calls
                    .iter()
                    .filter(|call| call.callee == params.item.name)
                    .filter_map(|call| {
                        let caller = graph.functions.iter().find(|f| f.name == call.caller)?;
                        Some(lsp::CallHierarchyIncomingCall {
                            from: lsp_item(caller),
                            from_ranges: vec![call_range(call.row)],
                        })
                    })
                    .collect();
                async move { Ok(Some(calls)) }
            }
        });
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    fn key() -> PrKey {
        PrKey {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "web".into(),
            number: 482,
        }
    }

    fn checkout() -> Checkout {
        Checkout {
            repo_dir: PathBuf::from(path!("/repo")),
            worktree: PathBuf::from(path!("/repo")),
            head_sha: "head".into(),
            merge_base: "base".into(),
            head_changed: false,
        }
    }

    /// Builds a review over `tree`, where `files` are the PR's changed files. `call_hierarchy`
    /// false gives a server without the capability; `gate` holds every prepare request until it
    /// resolves.
    async fn open_item(
        cx: &mut TestAppContext,
        tree: serde_json::Value,
        files: Vec<PrFile>,
        graph: Graph,
        call_hierarchy: bool,
        gate: Option<Gate>,
    ) -> (Entity<ReviewItem>, VisualTestContext) {
        let (item, cx, _) =
            open_item_with(cx, tree, files, graph, call_hierarchy, gate, false, None).await;
        (item, cx)
    }

    static DATABASE_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn test_database(cx: &mut TestAppContext) -> ReviewDb {
        let name = format!(
            "review_item_{}",
            DATABASE_COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        cx.foreground_executor()
            .block_test(ReviewDb::open_test_db(name.leak()))
    }

    /// `restricted` opens the project with worktree trust enforced and the worktree untrusted.
    /// Also returns the number of call-hierarchy prepare requests the server received.
    async fn open_item_with(
        cx: &mut TestAppContext,
        tree: serde_json::Value,
        files: Vec<PrFile>,
        graph: Graph,
        call_hierarchy: bool,
        gate: Option<Gate>,
        restricted: bool,
        database: Option<ReviewDb>,
    ) -> (Entity<ReviewItem>, VisualTestContext, Arc<AtomicUsize>) {
        let detail = parse_detail(&key(), DETAIL).expect("detail");
        open_item_with_detail(
            cx,
            tree,
            files,
            graph,
            call_hierarchy,
            gate,
            restricted,
            database,
            detail,
        )
        .await
    }

    async fn open_item_with_detail(
        cx: &mut TestAppContext,
        tree: serde_json::Value,
        files: Vec<PrFile>,
        graph: Graph,
        call_hierarchy: bool,
        gate: Option<Gate>,
        restricted: bool,
        database: Option<ReviewDb>,
        detail: PrDetail,
    ) -> (Entity<ReviewItem>, VisualTestContext, Arc<AtomicUsize>) {
        init_test(cx);
        let database = database.unwrap_or_else(|| test_database(cx));
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/repo"), tree).await;
        let project = if restricted {
            cx.update(|cx| project::trusted_worktrees::init(Default::default(), cx));
            Project::test_with_worktree_trust(fs, [path!("/repo").as_ref()], cx).await
        } else {
            Project::test(fs, [path!("/repo").as_ref()], cx).await
        };
        let prepares = Arc::new(AtomicUsize::new(0));
        let registry = project.read_with(cx, |project, _| project.languages().clone());
        registry.add(language::rust_lang());
        let graph = Arc::new(graph);
        let gate = gate.unwrap_or_else(|| {
            let (release, gate) = futures::channel::oneshot::channel();
            release.send(()).expect("open gate");
            gate.shared()
        });
        registry.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    call_hierarchy_provider: call_hierarchy
                        .then_some(lsp::CallHierarchyServerCapability::Simple(true)),
                    ..lsp::ServerCapabilities::default()
                },
                initializer: Some(Box::new({
                    let prepares = prepares.clone();
                    move |server| answer_call_hierarchy(server, &graph, gate.clone(), &prepares)
                })),
                ..FakeLspAdapter::default()
            },
        );
        let mut loaded = Vec::new();
        for file in files {
            let diff = FileDiff {
                path: file.path.to_string(),
                status: file.status,
                binary: false,
                submodule: false,
                symlink: false,
                hunks: file.hunks,
            };
            let task = cx.update(|cx| {
                build_loaded_file(
                    &project,
                    PathBuf::from(path!("/repo")).join(file.path),
                    file.base.map(str::to_string),
                    diff,
                    cx,
                )
            });
            loaded.push(task.await.expect("loaded file"));
        }
        let window = cx.add_window(|window, cx| {
            ReviewItem::new(
                project.clone(),
                detail,
                "alice".into(),
                checkout(),
                loaded,
                database,
                window,
                cx,
            )
        });
        let item = window.root(cx).expect("item");
        let cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        (item, cx, prepares)
    }

    /// Lets the language server start and every call-hierarchy retry run.
    fn settle(cx: &mut VisualTestContext) {
        for _ in 0..5 {
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.run_until_parked();
        }
    }

    fn text(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> String {
        item.read_with(cx, |item, cx| item.editor().read(cx).text(cx))
    }

    fn step_keys(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> Vec<String> {
        item.read_with(cx, |item, _| {
            item.steps().iter().map(|step| step.key.clone()).collect()
        })
    }

    const LOGIN_BASE: &str = "\
fn login(user: &str, password: &str) -> bool {
    verify_password(user, password)
}
";
    const LOGIN_HEAD: &str = "\
fn login(user: &str, password: &str) -> bool {
    let user = user.trim();
    audit(user);
    verify_password(user, password)
}
";
    const PASSWORD: &str = "\
fn verify_password(user: &str, password: &str) -> bool {
    user == password
}
";

    fn login_graph() -> Graph {
        Graph {
            functions: vec![
                function("login", "src/login.rs", 0, 4),
                function("verify_password", "src/password.rs", 0, 2),
            ],
            calls: vec![call("login", "verify_password", 3)],
        }
    }

    fn login_tree() -> serde_json::Value {
        json!({ "src": { "login.rs": LOGIN_HEAD, "password.rs": PASSWORD } })
    }

    #[gpui::test]
    async fn workspace_shows_changed_hunk_then_unchanged_callee(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
        )
        .await;
        settle(&mut cx);
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(
                gpui::AvailableSpace::Definite(px(1200.)),
                gpui::AvailableSpace::Definite(px(800.)),
            ),
            |_, _| item.clone().into_any_element(),
        );
        let text = text(&item, &mut cx);
        let hunk = text.find("let user = user.trim();").expect("hunk shown");
        let callee = text.find("user == password").expect("callee shown");
        assert!(hunk < callee, "{text}");
        item.read_with(&cx, |item, _| {
            assert_eq!(item.context_state(), &ContextState::Ready);
            let labels: Vec<_> = item
                .shown_context
                .iter()
                .map(|excerpt| excerpt.label.as_str())
                .collect();
            assert_eq!(
                labels,
                vec!["unchanged · src/password.rs · verify_password() · called on line 4"]
            );
            assert_eq!(item.label_blocks.len(), 1);
            let titles: Vec<_> = item.steps().iter().map(|step| step.title.clone()).collect();
            assert_eq!(titles, vec!["login.rs · login()"]);
        });
    }

    #[gpui::test]
    async fn diff_shows_before_callers_arrive(cx: &mut TestAppContext) {
        let (release, gate) = futures::channel::oneshot::channel::<()>();
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            Some(gate.shared()),
        )
        .await;
        settle(&mut cx);
        assert!(text(&item, &mut cx).contains("let user = user.trim();"));
        assert!(!text(&item, &mut cx).contains("user == password"));
        item.read_with(&cx, |item, _| {
            assert_eq!(item.context_state(), &ContextState::Finding)
        });

        release.send(()).expect("release");
        settle(&mut cx);
        item.read_with(&cx, |item, _| {
            assert_eq!(item.context_state(), &ContextState::Ready)
        });
        assert!(text(&item, &mut cx).contains("user == password"));
    }

    #[gpui::test]
    async fn no_call_hierarchy_is_unavailable(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            false,
            None,
        )
        .await;
        settle(&mut cx);
        item.read_with(&cx, |item, _| {
            assert!(matches!(item.context_state(), ContextState::Unavailable(_)));
        });
        assert!(text(&item, &mut cx).contains("let user = user.trim();"));
    }

    const A_BASE: &str = "fn caller() {\n    callee();\n}\n";
    const A_HEAD: &str = "fn caller() {\n    callee();\n    callee();\n}\n";
    const B_BASE: &str = "fn callee() {\n    let a = 1;\n}\n";
    const B_HEAD: &str = "fn callee() {\n    let b = 2;\n}\n";

    fn two_file_graph() -> Graph {
        Graph {
            functions: vec![
                function("caller", "src/a.rs", 0, 3),
                function("callee", "src/b.rs", 0, 2),
            ],
            calls: vec![call("caller", "callee", 1), call("caller", "callee", 2)],
        }
    }

    #[gpui::test]
    async fn selecting_step_replaces_excerpts(cx: &mut TestAppContext) {
        let graph = Graph {
            functions: two_file_graph().functions,
            calls: Vec::new(),
        };
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": { "a.rs": A_HEAD, "b.rs": B_HEAD } }),
            vec![modified("src/a.rs", A_BASE), modified("src/b.rs", B_BASE)],
            graph,
            true,
            None,
        )
        .await;
        settle(&mut cx);
        assert_eq!(
            step_keys(&item, &mut cx),
            vec!["src/a.rs::caller", "src/b.rs::callee"]
        );
        assert!(text(&item, &mut cx).contains("fn caller"));
        assert!(!text(&item, &mut cx).contains("let b = 2"));

        item.update_in(&mut cx, |item, window, cx| item.select_step(1, window, cx));
        cx.run_until_parked();
        assert!(text(&item, &mut cx).contains("let b = 2"));
        assert!(!text(&item, &mut cx).contains("fn caller"));

        item.update_in(&mut cx, |item, window, cx| {
            window.focus(&item.editor().focus_handle(cx), cx)
        });
        cx.dispatch_action(PreviousStep);
        cx.run_until_parked();
        item.read_with(&cx, |item, _| assert_eq!(item.selected_step(), 0));
        assert!(text(&item, &mut cx).contains("fn caller"));
        cx.dispatch_action(NextStep);
        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        item.read_with(&cx, |item, _| assert_eq!(item.selected_step(), 1));
    }

    #[gpui::test]
    async fn map_reorders_when_edges_arrive(cx: &mut TestAppContext) {
        let (release, gate) = futures::channel::oneshot::channel::<()>();
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": { "a.rs": A_HEAD, "b.rs": B_HEAD } }),
            vec![modified("src/a.rs", A_BASE), modified("src/b.rs", B_BASE)],
            two_file_graph(),
            true,
            Some(gate.shared()),
        )
        .await;
        settle(&mut cx);
        assert_eq!(
            step_keys(&item, &mut cx),
            vec!["src/a.rs::caller", "src/b.rs::callee"]
        );
        item.update_in(&mut cx, |item, window, cx| item.select_step(1, window, cx));

        release.send(()).expect("release");
        settle(&mut cx);
        assert_eq!(
            step_keys(&item, &mut cx),
            vec!["src/b.rs::callee", "src/a.rs::caller"]
        );
        item.read_with(&cx, |item, _| {
            assert_eq!(item.selected_step(), 0);
            assert_eq!(item.context_state(), &ContextState::Ready);
        });
        assert!(text(&item, &mut cx).contains("let b = 2"));
    }

    #[gpui::test]
    async fn impact_marks_callers(cx: &mut TestAppContext) {
        let graph = Graph {
            functions: vec![
                function("target", "src/target.rs", 0, 2),
                function("uses_new", "src/a.rs", 0, 2),
                function("old_user", "src/c.rs", 0, 2),
            ],
            calls: vec![call("uses_new", "target", 1), call("old_user", "target", 1)],
        };
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": {
                "target.rs": "fn target(a: u32, b: u32) -> u32 {\n    a + b\n}\n",
                "a.rs": "fn uses_new() -> u32 {\n    target(1, 2)\n}\n",
                "c.rs": "fn old_user() -> u32 {\n    target(1)\n}\n",
            } }),
            vec![
                modified("src/target.rs", "fn target(a: u32) -> u32 {\n    a\n}\n"),
                modified("src/a.rs", "fn uses_new() -> u32 {\n    target(1)\n}\n"),
            ],
            graph,
            true,
            None,
        )
        .await;
        settle(&mut cx);
        assert_eq!(
            step_keys(&item, &mut cx),
            vec!["src/target.rs::target", "src/a.rs::uses_new"]
        );
        // The selected step keeps its key when edges arrive, so it moved to the second step.
        item.update_in(&mut cx, |item, window, cx| item.select_step(0, window, cx));
        let offset = text(&item, &mut cx)
            .find("fn target(a: u32, b")
            .expect("target shown");
        item.update_in(&mut cx, |item, window, cx| {
            item.editor().update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([MultiBufferOffset(offset)..MultiBufferOffset(offset)])
                })
            })
        });
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            assert_eq!(item.impact_function.as_deref(), Some("target"));
            assert_eq!(
                item.impact(),
                &[
                    ImpactRow {
                        location: "src/a.rs:2".into(),
                        mark: CallerMark::InThisPr,
                    },
                    ImpactRow {
                        location: "src/c.rs:2".into(),
                        mark: CallerMark::NotUpdated,
                    },
                ]
            );
        });
    }

    #[gpui::test]
    async fn long_unchanged_excerpt_collapses_and_expands(cx: &mut TestAppContext) {
        let long: String = std::iter::once("fn long() {\n".to_string())
            .chain((1..=10).map(|row| format!("    let row{row} = {row};\n")))
            .chain(std::iter::once("}\n".to_string()))
            .collect();
        let graph = Graph {
            functions: vec![
                function("main", "src/main.rs", 0, 3),
                function("long", "src/long.rs", 0, 11),
            ],
            calls: vec![call("main", "long", 2)],
        };
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": {
                "main.rs": "fn main() {\n    let a = 2;\n    long();\n}\n",
                "long.rs": long,
            } }),
            vec![modified(
                "src/main.rs",
                "fn main() {\n    let a = 1;\n    long();\n}\n",
            )],
            graph,
            true,
            None,
        )
        .await;
        settle(&mut cx);
        let rows = |item: &ReviewItem| {
            item.shown_context
                .iter()
                .map(|excerpt| excerpt.range.end.row - excerpt.range.start.row + 1)
                .collect::<Vec<_>>()
        };
        item.read_with(&cx, |item, _| {
            assert_eq!(rows(item), vec![8]);
            assert!(
                item.shown_context[0]
                    .label
                    .starts_with("unchanged · src/long.rs · long() · called on line 3"),
            );
        });
        assert!(text(&item, &mut cx).contains("let row7 = 7;"));
        assert!(!text(&item, &mut cx).contains("let row8 = 8;"));

        item.update(&mut cx, |item, cx| item.expand_context(0, cx));
        cx.run_until_parked();
        item.read_with(&cx, |item, _| assert_eq!(rows(item), vec![12]));
        assert!(text(&item, &mut cx).contains("let row10 = 10;"));
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    struct GitFixture {
        _temp: tempfile::TempDir,
        base: PathBuf,
        upstream: PathBuf,
        seed: PathBuf,
        user: PathBuf,
        upstream_url: String,
        pr_sha: String,
    }

    impl GitFixture {
        /// An upstream with `main` and PR #482, and a user clone whose remote names
        /// github.com/acme/web and whose work tree has an uncommitted edit.
        fn new() -> Option<Self> {
            if Command::new("git").arg("--version").output().is_err() {
                return None;
            }
            let temp = tempfile::tempdir().expect("tempdir");
            let base = temp.path().canonicalize().expect("canonical");
            let upstream = base.join("upstream.git");
            let seed = base.join("seed");
            let user = base.join("user");
            std::fs::create_dir_all(&seed).expect("seed dir");
            run(
                &base,
                &["init", "-q", "--bare", "-b", "main", "upstream.git"],
            );
            run(&seed, &["init", "-q", "-b", "main"]);
            std::fs::write(seed.join("a.rs"), "fn a() {}\n").expect("write");
            run(&seed, &["add", "."]);
            run(&seed, &["commit", "-q", "-m", "base"]);
            run(&seed, &["checkout", "-q", "-b", "pr"]);
            let upstream_url = format!("file://{}", upstream.display());
            run(&base, &["clone", "-q", &upstream_url, "user"]);
            let mut fixture = Self {
                _temp: temp,
                base,
                upstream,
                seed,
                user,
                upstream_url,
                pr_sha: String::new(),
            };
            fixture.push_pr_head("fn a() {\n    1;\n}\n");
            run(&fixture.user, &["fetch", "-q", "origin"]);
            run(
                &fixture.user,
                &["remote", "set-url", "origin", "git@github.com:acme/web.git"],
            );
            std::fs::write(fixture.user.join("a.rs"), "dirty\n").expect("write");
            Some(fixture)
        }

        fn push_pr_head(&mut self, contents: &str) {
            std::fs::write(self.seed.join("a.rs"), contents).expect("write");
            run(&self.seed, &["commit", "-q", "-am", "pr"]);
            self.pr_sha = run(&self.seed, &["rev-parse", "HEAD"]);
            run(
                &self.seed,
                &["push", "-q", "-f", &self.upstream_url, "main", "pr"],
            );
            run(
                &self.upstream,
                &["update-ref", "refs/pull/482/head", &self.pr_sha],
            );
        }

        async fn prepare(&self, fake: &Arc<FakeGh>, database: &ReviewDb) -> Result<PreparedReview> {
            fake.respond(|request| request.args[0] == "auth", Ok(AUTH.into()));
            fake.respond(
                |request| {
                    request
                        .stdin
                        .as_deref()
                        .is_some_and(|stdin| stdin.contains("pullRequest"))
                },
                Ok(DETAIL.into()),
            );
            let summary = parse_detail(&key(), DETAIL).expect("detail").summary;
            prepare_review(
                &summary,
                self.upstream_url.clone(),
                self.base.join("root"),
                vec![self.user.clone()],
                fake.clone(),
                database.clone(),
            )
            .await
        }
    }

    #[gpui::test]
    async fn open_review_leaves_user_clone_untouched(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = GitFixture::new() else {
            return;
        };
        let user = &fixture.user;
        let user_state = || {
            vec![
                run(user, &["symbolic-ref", "HEAD"]),
                run(user, &["status", "--porcelain=v1", "-uall"]),
                run(user, &["stash", "list"]),
                format!("{:?}", std::fs::read(user.join("a.rs")).ok()),
            ]
        };
        let before = user_state();

        let fake = FakeGh::new();
        let database = cx.foreground_executor().block_test(ReviewDb::open_test_db(
            "open_review_leaves_user_clone_untouched",
        ));
        let prepared = fixture.prepare(&fake, &database).await.expect("prepared");

        assert_eq!(user_state(), before);
        assert_eq!(prepared.checkout.head_sha, fixture.pr_sha);
        assert_eq!(prepared.checkout.repo_dir, user.join(".git"));
        assert!(
            prepared
                .checkout
                .worktree
                .starts_with(fixture.base.join("root"))
        );
        assert_eq!(prepared.detail.summary.key, key());
        let paths: Vec<_> = prepared
            .files
            .iter()
            .map(|(diff, base)| (diff.path.as_str(), base.as_deref()))
            .collect();
        assert_eq!(paths, vec![("a.rs", Some("fn a() {}\n"))]);
        let tracked = database.tracked().expect("tracked");
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked[0].worktree, prepared.checkout.worktree);
    }

    #[gpui::test]
    async fn logged_out_fails_before_checkout(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("root");
        let fake = FakeGh::new();
        fake.respond(
            |request| request.args[0] == "auth",
            Ok(include_str!("../test_fixtures/auth_status_logged_out.json").into()),
        );
        let database = cx
            .foreground_executor()
            .block_test(ReviewDb::open_test_db("logged_out_fails_before_checkout"));
        let summary = parse_detail(&key(), DETAIL).expect("detail").summary;
        let error = prepare_review(
            &summary,
            "file:///nowhere".into(),
            root.clone(),
            Vec::new(),
            fake.clone(),
            database.clone(),
        )
        .await
        .err()
        .expect("logged out");
        assert!(error.to_string().contains("Not logged in"), "{error:#}");
        assert_eq!(fake.requests().len(), 1);
        assert!(!root.exists());
        assert!(database.tracked().expect("tracked").is_empty());
    }

    #[gpui::test]
    async fn open_review_from_inside_a_workspace_update_does_not_panic(cx: &mut TestAppContext) {
        init_test(cx);
        let fake = FakeGh::new();
        fake.respond(
            |request| request.args[0] == "auth",
            Ok(include_str!("../test_fixtures/auth_status_logged_out.json").into()),
        );
        cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            crate::set_gh_runner(fake.clone(), cx);
        });
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        cx.update(|cx| AppState::set_global(workspace.read(cx).app_state().clone(), cx));
        let summary = parse_detail(&key(), DETAIL).expect("detail").summary;

        // The inbox's Review button calls open_review while the workspace is being updated.
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        let task = workspace.update_in(&mut visual, |workspace, window, cx| {
            open_review(summary, workspace, window, cx)
        });
        let error = task.await.err().expect("logged out");
        assert!(error.to_string().contains("Not logged in"), "{error:#}");
    }

    #[gpui::test]
    async fn reopening_after_new_head_replaces_the_review(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let Some(mut fixture) = GitFixture::new() else {
            return;
        };
        init_test(cx);
        let fake = FakeGh::new();
        let database = cx.foreground_executor().block_test(ReviewDb::open_test_db(
            "reopening_after_new_head_replaces_the_review",
        ));
        let prepared = fixture.prepare(&fake, &database).await.expect("first");
        let worktree = prepared.checkout.worktree.clone();
        let fs = fs::RealFs::new(None, cx.executor());
        let project = Project::test(fs, [worktree.as_path()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        place_review(
            workspace.clone(),
            window.into(),
            prepared,
            database.clone(),
            &mut cx.to_async(),
        )
        .await
        .expect("placed");

        let first_head = fixture.pr_sha.clone();
        fixture.push_pr_head("fn a() {\n    2;\n}\n");
        assert_ne!(fixture.pr_sha, first_head);
        let prepared = fixture.prepare(&fake, &database).await.expect("second");
        assert_eq!(prepared.checkout.head_sha, fixture.pr_sha);
        place_review(
            workspace.clone(),
            window.into(),
            prepared,
            database.clone(),
            &mut cx.to_async(),
        )
        .await
        .expect("replaced");

        let items = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<ReviewItem>(cx)
                .collect::<Vec<_>>()
        });
        assert_eq!(items.len(), 1);
        let (head, text) = items[0].read_with(cx, |item, cx| {
            (
                item.checkout.head_sha.clone(),
                item.editor().read(cx).text(cx),
            )
        });
        assert_eq!(head, fixture.pr_sha);
        assert!(text.contains("    2;"), "{text}");
        assert!(!text.contains("    1;"), "{text}");
    }

    fn worktree_trusted(project: &Entity<Project>, cx: &mut TestAppContext) -> bool {
        cx.update(|cx| {
            let worktree_store = project.read(cx).worktree_store();
            let Some(worktree) = project.read(cx).worktrees(cx).next() else {
                return false;
            };
            let id = worktree.read(cx).id();
            TrustedWorktrees::try_get_global(cx)
                .expect("trust store")
                .update(cx, |trusted, cx| trusted.can_trust(&worktree_store, id, cx))
        })
    }

    fn init_trust(saved: &str, cx: &mut TestAppContext) {
        let mut saved_paths = project::trusted_worktrees::DbTrustedPaths::default();
        saved_paths
            .entry(None)
            .or_default()
            .insert(PathBuf::from(saved));
        cx.update(|cx| project::trusted_worktrees::init(saved_paths, cx));
    }

    #[gpui::test]
    async fn trust_does_not_survive_a_new_head(cx: &mut TestAppContext) {
        init_test(cx);
        init_trust(path!("/reviews/pr"), cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/reviews/pr"), json!({ "a.rs": "" }))
            .await;
        let mut checkout = Checkout {
            worktree: PathBuf::from(path!("/reviews/pr")),
            head_changed: false,
            ..checkout()
        };

        cx.update(|cx| forget_stale_trust(&checkout, cx));
        let project =
            Project::test_with_worktree_trust(fs.clone(), [path!("/reviews/pr").as_ref()], cx)
                .await;
        assert!(
            worktree_trusted(&project, cx),
            "the same head stays trusted"
        );

        checkout.head_changed = true;
        cx.update(|cx| forget_stale_trust(&checkout, cx));
        assert!(!worktree_trusted(&project, cx));
        let reopened =
            Project::test_with_worktree_trust(fs, [path!("/reviews/pr").as_ref()], cx).await;
        assert!(!worktree_trusted(&reopened, cx));
    }

    #[gpui::test]
    async fn pr_worktree_is_never_trusted_through_a_parent(cx: &mut TestAppContext) {
        init_test(cx);
        init_trust(path!("/reviews"), cx);
        cx.update(|cx| refuse_parent_trust(PathBuf::from(path!("/reviews/pr_review")), cx));
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/reviews/pr_review/pr"), json!({ "a.rs": "" }))
            .await;
        let project =
            Project::test_with_worktree_trust(fs, [path!("/reviews/pr_review/pr").as_ref()], cx)
                .await;
        assert!(!worktree_trusted(&project, cx));

        cx.update(|cx| {
            let worktree_store = project.read(cx).worktree_store();
            TrustedWorktrees::try_get_global(cx)
                .expect("trust store")
                .update(cx, |trusted, cx| {
                    trusted.trust(
                        &worktree_store,
                        [PathTrust::AbsPath(PathBuf::from(path!("/reviews")))]
                            .into_iter()
                            .collect(),
                        cx,
                    )
                });
        });
        assert!(!worktree_trusted(&project, cx));

        cx.update(|cx| {
            let worktree_store = project.read(cx).worktree_store();
            let worktree_ids = project
                .read(cx)
                .worktrees(cx)
                .map(|worktree| PathTrust::Worktree(worktree.read(cx).id()))
                .collect();
            TrustedWorktrees::try_get_global(cx)
                .expect("trust store")
                .update(cx, |trusted, cx| {
                    trusted.trust(&worktree_store, worktree_ids, cx)
                });
        });
        assert!(
            worktree_trusted(&project, cx),
            "trusting the worktree itself works"
        );
    }

    #[gpui::test]
    async fn untrusted_worktree_waits_for_trust(cx: &mut TestAppContext) {
        let (item, mut cx, prepares) = open_item_with(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
            true,
            None,
        )
        .await;
        settle(&mut cx);
        item.read_with(&cx, |item, _| {
            assert_eq!(
                item.context_state(),
                &ContextState::Unavailable(TRUST_REASON.into())
            )
        });
        assert_eq!(prepares.load(Ordering::SeqCst), 0);
        assert!(text(&item, &mut cx).contains("let user = user.trim();"));

        cx.update(|_, cx| {
            let project = item.read(cx).project.clone();
            let worktree_store = project.read(cx).worktree_store();
            let worktree_ids: Vec<_> = project
                .read(cx)
                .worktrees(cx)
                .map(|worktree| PathTrust::Worktree(worktree.read(cx).id()))
                .collect();
            TrustedWorktrees::try_get_global(cx)
                .expect("trust store")
                .update(cx, |trusted, cx| {
                    trusted.trust(&worktree_store, worktree_ids.into_iter().collect(), cx)
                });
        });
        settle(&mut cx);
        item.read_with(&cx, |item, _| {
            assert_eq!(item.context_state(), &ContextState::Ready)
        });
        assert!(prepares.load(Ordering::SeqCst) > 0);
        assert!(text(&item, &mut cx).contains("user == password"));
    }

    fn select_text(item: &Entity<ReviewItem>, needle: &str, cx: &mut VisualTestContext) {
        let offset = text(item, cx).find(needle).expect("text shown");
        item.update_in(cx, |item, window, cx| {
            window.focus(&item.editor().focus_handle(cx), cx);
            item.editor().update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([MultiBufferOffset(offset)..MultiBufferOffset(offset)])
                })
            })
        });
        cx.run_until_parked();
    }

    fn composer(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> Entity<Editor> {
        item.read_with(cx, |item, _| {
            item.draft_views
                .last()
                .and_then(|view| view.composer.clone())
                .expect("composer")
        })
    }

    fn type_text(editor: &Entity<Editor>, text: &str, cx: &mut VisualTestContext) {
        editor.update_in(cx, |editor, window, cx| editor.insert(text, window, cx));
    }

    fn database(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> ReviewDb {
        item.read_with(cx, |item, _| item.database.clone())
    }

    fn stored_draft(path: &str, start_line: u32, anchor_text: &str) -> Draft {
        Draft {
            id: None,
            key: key(),
            path: path.into(),
            side: crate::Side::Right,
            start_line,
            end_line: start_line,
            commit: "old".into(),
            anchor_text: anchor_text.into(),
            body: format!("about {anchor_text}"),
            outdated: false,
            general: false,
            post_state: crate::PostState::Unposted,
        }
    }

    #[gpui::test]
    async fn draft_survives_reload(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
        )
        .await;
        settle(&mut cx);
        select_text(&item, "audit(user);", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        let composer = composer(&item, &mut cx);
        type_text(&composer, "Why audit ", &mut cx);
        type_text(&composer, "here?", &mut cx);
        cx.run_until_parked();

        let database = database(&item, &mut cx);
        let saved = database.drafts(&key()).expect("drafts");
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].body, "Why audit here?");
        assert_eq!(saved[0].path, "src/login.rs");
        assert_eq!((saved[0].start_line, saved[0].end_line), (3, 3));
        assert_eq!(saved[0].anchor_text, "    audit(user);");
        assert_eq!(saved[0].commit, "head");

        let (project, detail, files) = item.read_with(&cx, |item, _| {
            let files: Vec<LoadedFile> = item
                .files
                .iter()
                .map(|file| LoadedFile {
                    diff: file.diff.clone(),
                    contents: file.contents.clone(),
                })
                .collect();
            (item.project.clone(), item.detail.clone(), files)
        });
        cx.update(|window, _| window.remove_window());
        drop(item);
        cx.run_until_parked();

        let window = cx.add_window(|window, cx| {
            ReviewItem::new(
                project,
                detail,
                "alice".into(),
                checkout(),
                files,
                database,
                window,
                cx,
            )
        });
        let reopened = window.root(&mut cx).expect("item");
        cx.run_until_parked();
        reopened.read_with(&cx, |item, _| {
            let bodies: Vec<_> = item
                .drafts()
                .iter()
                .map(|draft| draft.body.as_str())
                .collect();
            assert_eq!(bodies, vec!["Why audit here?"]);
            assert_eq!(item.comment_blocks.len(), 1);
        });
    }

    #[gpui::test]
    async fn comment_on_unchanged_excerpt_creates_draft(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
        )
        .await;
        settle(&mut cx);
        select_text(&item, "user == password", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            let draft = item.drafts().last().expect("draft").clone();
            assert_eq!(draft.path, "src/password.rs");
            assert_eq!(draft.side, crate::Side::Right);
            assert_eq!((draft.start_line, draft.end_line), (2, 2));
            assert_eq!(draft.anchor_text, "    user == password");
            assert_eq!(draft.commit, "head");
            assert_eq!(
                item.draft_note(&draft).as_deref(),
                Some(
                    "Unchanged file: GitHub can't take this comment directly, so Photon adds it \
                     to the review summary under src/password.rs, with a quote."
                )
            );
            assert_eq!(item.comment_blocks.len(), 1);
        });
    }

    #[gpui::test]
    async fn comment_on_deleted_row_uses_base_side(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": { "b.rs": B_HEAD } }),
            vec![modified("src/b.rs", B_BASE)],
            Graph {
                functions: vec![function("callee", "src/b.rs", 0, 2)],
                calls: Vec::new(),
            },
            true,
            None,
        )
        .await;
        settle(&mut cx);
        select_text(&item, "let a = 1;", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            let draft = item.drafts().last().expect("draft");
            assert_eq!(draft.path, "src/b.rs");
            assert_eq!(draft.side, crate::Side::Left);
            assert_eq!((draft.start_line, draft.end_line), (2, 2));
            assert_eq!(draft.anchor_text, "    let a = 1;");
            assert_eq!(item.comment_blocks.len(), 1);
        });
    }

    #[gpui::test]
    async fn reanchored_draft_saved_with_new_commit(cx: &mut TestAppContext) {
        let database = test_database(cx);
        for draft in [
            stored_draft("src/login.rs", 2, "    verify_password(user, password)"),
            stored_draft("src/login.rs", 2, "    old_line();"),
            stored_draft("src/password.rs", 5, "    user == password"),
        ] {
            database.save_draft(draft).await.expect("save");
        }
        let (item, mut cx, _) = open_item_with(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
            false,
            Some(database.clone()),
        )
        .await;
        settle(&mut cx);
        let summary = |drafts: &[Draft]| {
            drafts
                .iter()
                .map(|draft| {
                    (
                        draft.start_line,
                        draft.end_line,
                        draft.commit.clone(),
                        draft.outdated,
                    )
                })
                .collect::<Vec<_>>()
        };
        let expected = vec![
            (4, 4, "head".to_string(), false),
            (2, 2, "old".to_string(), true),
            (2, 2, "head".to_string(), false),
        ];
        assert_eq!(summary(&database.drafts(&key()).expect("drafts")), expected);
        item.read_with(&cx, |item, _| {
            assert_eq!(summary(item.drafts()), expected);
            assert_eq!(item.drafts()[1].anchor_text, "    old_line();");
        });

        // The outdated draft can be placed again on the current selection.
        select_text(&item, "audit(user);", &mut cx);
        item.update(&mut cx, |item, cx| item.replace_draft(1, cx));
        cx.run_until_parked();
        let replaced = database.drafts(&key()).expect("drafts")[1].clone();
        assert_eq!((replaced.start_line, replaced.outdated), (3, false));
        assert_eq!(replaced.anchor_text, "    audit(user);");
        assert_eq!(replaced.body, "about     old_line();");
    }

    #[gpui::test]
    async fn cancel_nonempty_draft_asks_first(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
        )
        .await;
        settle(&mut cx);
        let database = database(&item, &mut cx);
        let local_id = |item: &Entity<ReviewItem>, cx: &mut VisualTestContext| {
            item.read_with(cx, |item, _| {
                item.draft_views.last().expect("view").local_id
            })
        };

        select_text(&item, "audit(user);", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        let empty = local_id(&item, &mut cx);
        item.update_in(&mut cx, |item, window, cx| {
            item.cancel_draft(empty, window, cx)
        });
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        item.read_with(&cx, |item, _| assert!(item.drafts().is_empty()));

        select_text(&item, "audit(user);", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        let typed = local_id(&item, &mut cx);
        type_text(&composer(&item, &mut cx), "Keep me", &mut cx);
        cx.run_until_parked();
        item.update_in(&mut cx, |item, window, cx| {
            item.cancel_draft(typed, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Keep");
        cx.run_until_parked();
        item.read_with(&cx, |item, _| assert_eq!(item.drafts().len(), 1));
        assert_eq!(database.drafts(&key()).expect("drafts").len(), 1);

        item.update_in(&mut cx, |item, window, cx| {
            item.cancel_draft(typed, window, cx)
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("Discard");
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            assert!(item.drafts().is_empty());
            assert!(item.comment_blocks.is_empty());
        });
        assert!(database.drafts(&key()).expect("drafts").is_empty());
    }

    async fn open_with_drafts(
        cx: &mut TestAppContext,
        drafts: Vec<Draft>,
    ) -> (Entity<ReviewItem>, VisualTestContext, ReviewDb) {
        let database = test_database(cx);
        for draft in drafts {
            database.save_draft(draft).await.expect("save");
        }
        let (item, mut cx, _) = open_item_with(
            cx,
            login_tree(),
            vec![login_with_hunk()],
            login_graph(),
            true,
            None,
            false,
            Some(database.clone()),
        )
        .await;
        settle(&mut cx);
        (item, cx, database)
    }

    /// Lines 1–4 of the head are in the diff; line 5 (`}`) is unchanged.
    fn login_with_hunk() -> PrFile {
        PrFile {
            hunks: vec![crate::Hunk {
                old_start: 1,
                old_len: 2,
                new_start: 1,
                new_len: 4,
                body: " fn login(user: &str, password: &str) -> bool {\n\
                       +    let user = user.trim();\n\
                       +    audit(user);\n     verify_password(user, password)\n"
                    .into(),
            }],
            ..modified("src/login.rs", LOGIN_BASE)
        }
    }

    fn current_draft(path: &str, start_line: u32, anchor_text: &str) -> Draft {
        Draft {
            commit: "head".into(),
            ..stored_draft(path, start_line, anchor_text)
        }
    }

    fn bodies(database: &ReviewDb) -> Vec<String> {
        database
            .drafts(&key())
            .expect("drafts")
            .into_iter()
            .map(|draft| draft.body)
            .collect()
    }

    #[gpui::test]
    async fn edit_after_reload(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(
            cx,
            vec![current_draft("src/login.rs", 3, "    audit(user);")],
        )
        .await;
        item.update_in(&mut cx, |item, window, cx| item.edit_draft(0, window, cx));
        cx.run_until_parked();
        let composer = composer(&item, &mut cx);
        assert_eq!(
            composer.read_with(&cx, |editor, cx| editor.text(cx)),
            "about     audit(user);"
        );
        type_text(&composer, " Really?", &mut cx);
        cx.run_until_parked();
        assert_eq!(bodies(&database), vec!["about     audit(user); Really?"]);
        item.update(&mut cx, |item, cx| item.collapse_draft(0, cx));
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            assert!(item.draft_views[0].composer.is_none());
            assert_eq!(item.drafts()[0].body, "about     audit(user); Really?");
        });
    }

    #[gpui::test]
    async fn delete_after_reload(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(
            cx,
            vec![current_draft("src/login.rs", 3, "    audit(user);")],
        )
        .await;
        item.update_in(&mut cx, |item, window, cx| item.cancel_draft(0, window, cx));
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Discard");
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            assert!(item.drafts().is_empty());
            assert!(item.comment_blocks.is_empty());
        });
        assert!(bodies(&database).is_empty());
    }

    #[gpui::test]
    async fn empty_body_leaves_no_row(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
        )
        .await;
        settle(&mut cx);
        let database = database(&item, &mut cx);
        select_text(&item, "audit(user);", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        let composer = composer(&item, &mut cx);
        type_text(&composer, "x", &mut cx);
        cx.run_until_parked();
        assert_eq!(bodies(&database), vec!["x"]);

        composer.update_in(&mut cx, |editor, window, cx| {
            editor.set_text("  ", window, cx)
        });
        cx.run_until_parked();
        assert!(bodies(&database).is_empty());
        item.read_with(&cx, |item, _| assert_eq!(item.drafts().len(), 1));

        type_text(&composer, "y", &mut cx);
        cx.run_until_parked();
        assert_eq!(bodies(&database), vec!["  y"]);
    }

    #[gpui::test]
    async fn failed_save_is_retried_when_added(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![modified("src/login.rs", LOGIN_BASE)],
            login_graph(),
            true,
            None,
        )
        .await;
        settle(&mut cx);
        let database = database(&item, &mut cx);
        select_text(&item, "audit(user);", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        crate::store::FAILING_SAVES.store(1, Ordering::SeqCst);
        type_text(&composer(&item, &mut cx), "Keep me", &mut cx);
        cx.run_until_parked();
        assert!(bodies(&database).is_empty());
        item.read_with(&cx, |item, _| {
            assert!(item.draft_views[0].sync.lock().error.is_some())
        });

        item.update(&mut cx, |item, cx| item.collapse_draft(0, cx));
        cx.run_until_parked();
        assert_eq!(bodies(&database), vec!["Keep me"]);
        item.read_with(&cx, |item, _| {
            assert!(item.draft_views[0].composer.is_none());
            assert!(item.draft_views[0].sync.lock().error.is_none());
        });
    }

    #[gpui::test]
    async fn left_draft_reanchored_outside_diff_is_outdated(cx: &mut TestAppContext) {
        let database = test_database(cx);
        let left = |start_line, anchor_text: &str| Draft {
            side: crate::Side::Left,
            ..stored_draft("src/b.rs", start_line, anchor_text)
        };
        for draft in [left(5, "    let a = 1;"), left(1, "fn callee() {")] {
            database.save_draft(draft).await.expect("save");
        }
        let (_item, mut cx, _) = open_item_with(
            cx,
            json!({ "src": { "b.rs": B_HEAD } }),
            vec![PrFile {
                hunks: vec![crate::Hunk {
                    old_start: 1,
                    old_len: 3,
                    new_start: 1,
                    new_len: 3,
                    body: " fn callee() {\n-    let a = 1;\n+    let b = 2;\n }\n".into(),
                }],
                ..modified("src/b.rs", B_BASE)
            }],
            Graph {
                functions: vec![function("callee", "src/b.rs", 0, 2)],
                calls: Vec::new(),
            },
            true,
            None,
            false,
            Some(database.clone()),
        )
        .await;
        settle(&mut cx);
        let drafts: Vec<_> = database
            .drafts(&key())
            .expect("drafts")
            .into_iter()
            .map(|draft| (draft.start_line, draft.commit, draft.outdated))
            .collect();
        assert_eq!(
            drafts,
            vec![(2, "head".to_string(), false), (1, "old".to_string(), true)]
        );
    }

    #[gpui::test]
    async fn unreadable_file_leaves_draft_untouched(cx: &mut TestAppContext) {
        let (_item, _cx, database) = open_with_drafts(
            cx,
            vec![
                stored_draft("src", 1, "fn a() {}"),
                stored_draft("src/gone.rs", 1, "fn a() {}"),
            ],
        )
        .await;
        let drafts: Vec<_> = database
            .drafts(&key())
            .expect("drafts")
            .into_iter()
            .map(|draft| (draft.path, draft.commit, draft.outdated))
            .collect();
        assert_eq!(
            drafts,
            vec![
                ("src".to_string(), "old".to_string(), false),
                ("src/gone.rs".to_string(), "old".to_string(), true),
            ]
        );
    }

    fn install_fake(cx: &mut VisualTestContext) -> Arc<FakeGh> {
        let fake = FakeGh::new();
        cx.update(|_, cx| crate::set_gh_runner(fake.clone(), cx));
        fake
    }

    fn is_review_post(request: &crate::GhRequest) -> bool {
        request
            .args
            .iter()
            .any(|argument| argument.ends_with("/reviews"))
    }

    fn is_detail(request: &crate::GhRequest) -> bool {
        request
            .stdin
            .as_deref()
            .is_some_and(|stdin| stdin.contains("pullRequest(number"))
    }

    fn request_json(request: &crate::GhRequest) -> serde_json::Value {
        serde_json::from_str(request.stdin.as_deref().expect("stdin")).expect("json")
    }

    fn submit_drafts() -> Vec<Draft> {
        vec![
            current_draft("src/login.rs", 3, "    audit(user);"),
            current_draft("src/login.rs", 5, "}"),
            current_draft("src/password.rs", 2, "    user == password"),
        ]
    }

    fn submit(
        item: &Entity<ReviewItem>,
        event: crate::ReviewEvent,
        cx: &mut VisualTestContext,
    ) -> Result<()> {
        let task = item.update_in(cx, |item, window, cx| item.submit(event, window, cx));
        cx.run_until_parked();
        task.now_or_never().expect("submit finished")
    }

    #[gpui::test]
    async fn submit_sends_one_review_with_every_draft(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, submit_drafts()).await;
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        submit(&item, crate::ReviewEvent::RequestChanges, &mut cx).expect("submitted");

        let posts: Vec<_> = fake.requests().into_iter().filter(is_review_post).collect();
        assert_eq!(posts.len(), 1);
        assert_eq!(
            posts[0].args,
            [
                "api",
                "--hostname",
                "github.com",
                "--method",
                "POST",
                "repos/acme/web/pulls/482/reviews",
                "--input",
                "-"
            ]
        );
        let body = request_json(&posts[0]);
        assert_eq!(body["commit_id"], "head");
        assert_eq!(body["event"], "REQUEST_CHANGES");
        let comments = body["comments"].as_array().expect("comments");
        assert_eq!(comments.len(), 2);
        assert_eq!(
            comments[0],
            json!({"path": "src/login.rs", "body": "about     audit(user);", "line": 3, "side": "RIGHT"})
        );
        assert_eq!(comments[1]["line"], 4);
        assert_eq!(comments[1]["side"], "RIGHT");
        assert!(
            comments[1]["body"]
                .as_str()
                .expect("quote")
                .starts_with(
                    "Unchanged code, [src/login.rs line 5](https://github.com/acme/web/blob/head/src/login.rs#L5):\n\n```rust\n}\n```"
                ),
            "{}",
            comments[1]["body"]
        );
        let review_body = body["body"].as_str().expect("body");
        assert!(
            review_body.starts_with("### src/password.rs\n\n"),
            "{review_body}"
        );
        assert!(review_body.contains("about     user == password"));
    }

    #[gpui::test]
    async fn failed_submit_keeps_drafts_and_shows_error(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, submit_drafts()).await;
        let fake = install_fake(&mut cx);
        let message = "gh: Validation Failed (HTTP 422): line must be part of the diff";
        fake.respond(
            is_review_post,
            Err(GhError::Failed {
                message: message.into(),
            }),
        );
        let before = item.read_with(&cx, |item, _| item.drafts().to_vec());
        let stored = database.drafts(&key()).expect("drafts");

        let error = submit(&item, crate::ReviewEvent::Comment, &mut cx).expect_err("failed");
        assert_eq!(error.to_string(), message);
        assert_eq!(fake.requests().len(), 1);
        item.read_with(&cx, |item, _| {
            assert_eq!(item.drafts(), before.as_slice());
            assert_eq!(item.submit_error.as_deref(), Some(message));
            assert!(!item.submitting);
        });
        assert_eq!(database.drafts(&key()).expect("drafts"), stored);
        assert_eq!(database.last_review(&key()).expect("last review"), None);
    }

    #[gpui::test]
    async fn submit_success_clears_drafts_and_records_last_review(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, submit_drafts()).await;
        database
            .track(Tracked {
                key: key(),
                node_id: "PR_482".into(),
                repo_dir: PathBuf::from(path!("/repo")),
                worktree: PathBuf::from(path!("/repo")),
            })
            .await
            .expect("track");
        let fake = install_fake(&mut cx);
        // GitHub only answers if every sent row was marked before the request went out.
        let marked = database.clone();
        fake.respond(
            move |request| {
                is_review_post(request)
                    && marked.drafts(&key()).is_ok_and(|drafts| {
                        drafts
                            .iter()
                            .all(|draft| draft.post_state == PostState::Sending)
                    })
            },
            Ok("{}".into()),
        );
        fake.respond(is_detail, Ok(DETAIL.into()));
        item.update_in(&mut cx, |item, window, cx| {
            item.summary_editor
                .update(cx, |editor, cx| editor.set_text("Looks good.", window, cx))
        });
        submit(&item, crate::ReviewEvent::Approve, &mut cx).expect("submitted");

        assert!(database.drafts(&key()).expect("drafts").is_empty());
        assert_eq!(
            database
                .last_review(&key())
                .expect("last review")
                .as_deref(),
            Some("head")
        );
        let requests = fake.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(request_json(&requests[0])["event"], "APPROVE");
        assert!(is_detail(&requests[1]));
        item.read_with(&cx, |item, cx| {
            assert!(item.drafts().is_empty());
            assert!(item.submit_error.is_none());
            assert!(item.summary_editor.read(cx).text(cx).is_empty());
        });
    }

    fn reopen(
        database: &ReviewDb,
        cx: &mut TestAppContext,
    ) -> impl std::future::Future<Output = (Entity<ReviewItem>, VisualTestContext, Arc<AtomicUsize>)>
    {
        open_item_with(
            cx,
            login_tree(),
            vec![login_with_hunk()],
            login_graph(),
            true,
            None,
            false,
            Some(database.clone()),
        )
    }

    fn reply_editor(item: &Entity<ReviewItem>, cx: &mut VisualTestContext) -> Entity<Editor> {
        item.read_with(cx, |item, _| {
            item.reply_editors
                .get("PRRT_1")
                .cloned()
                .expect("reply editor")
        })
    }

    #[gpui::test]
    async fn summary_and_reply_text_survive_a_reload(cx: &mut TestAppContext) {
        let (item, mut item_cx, database) = open_with_drafts(cx, Vec::new()).await;
        item.update_in(&mut item_cx, |item, window, cx| {
            item.summary_editor
                .update(cx, |editor, cx| editor.set_text("Mostly fine.", window, cx))
        });
        let editor = reply_editor(&item, &mut item_cx);
        type_text(&editor, "Agreed.", &mut item_cx);
        item_cx.run_until_parked();
        drop(item);

        let (item, mut item_cx, _) = reopen(&database, cx).await;
        item.read_with(&item_cx, |item, cx| {
            assert_eq!(item.summary_editor.read(cx).text(cx), "Mostly fine.");
        });
        let editor = reply_editor(&item, &mut item_cx);
        assert_eq!(
            editor.read_with(&item_cx, |editor, cx| editor.text(cx)),
            "Agreed."
        );

        let fake = install_fake(&mut item_cx);
        fake.respond(
            |request| {
                request
                    .args
                    .iter()
                    .any(|argument| argument.ends_with("/replies"))
            },
            Ok(r#"{"id":2001,"user":{"login":"alice"},"body":"Agreed.","created_at":"t"}"#.into()),
        );
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        let task = item.update(&mut item_cx, |item, cx| item.click_reply("PRRT_1", cx));
        item_cx.run_until_parked();
        task.now_or_never().expect("finished").expect("replied");
        submit(&item, crate::ReviewEvent::Comment, &mut item_cx).expect("submitted");
        item_cx.run_until_parked();
        assert!(database.composer_texts(&key()).expect("texts").is_empty());
    }

    #[gpui::test]
    async fn outdated_drafts_stay_local_on_submit(cx: &mut TestAppContext) {
        let outdated = Draft {
            outdated: true,
            ..stored_draft("src/login.rs", 2, "    gone();")
        };
        let (item, mut cx, database) = open_with_drafts(cx, vec![outdated.clone()]).await;
        item.read_with(&cx, |item, cx| {
            assert_eq!(item.left_out_drafts(), 1);
            assert_eq!(
                item.left_out_note().as_deref(),
                Some("1 draft not included (outdated or not yet re-placed)")
            );
            assert!(!item.can_comment(cx));
        });
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        item.update(&mut cx, |item, cx| {
            let draft = current_draft("src/login.rs", 3, "    audit(user);");
            item.push_draft(draft);
            item.queue_save(1, cx);
        });
        cx.run_until_parked();
        item.read_with(&cx, |item, cx| assert!(item.can_comment(cx)));
        submit(&item, crate::ReviewEvent::Comment, &mut cx).expect("submitted");

        let posts: Vec<_> = fake.requests().into_iter().filter(is_review_post).collect();
        let body = request_json(&posts[0]);
        assert_eq!(body["comments"].as_array().expect("comments").len(), 1);
        assert!(!body.to_string().contains("gone()"));
        let remaining: Vec<_> = database
            .drafts(&key())
            .expect("drafts")
            .into_iter()
            .map(|draft| (draft.anchor_text, draft.outdated))
            .collect();
        assert_eq!(remaining, vec![("    gone();".to_string(), true)]);
        item.read_with(&cx, |item, _| assert_eq!(item.drafts().len(), 1));
    }

    #[gpui::test]
    async fn reply_posts_only_on_click(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        fake.respond(
            |request| {
                request
                    .args
                    .iter()
                    .any(|argument| argument.ends_with("/replies"))
            },
            Ok(r#"{"id":2001,"user":{"login":"alice"},"body":"Agreed.","created_at":"t"}"#.into()),
        );
        let editor = item.read_with(&cx, |item, _| {
            item.reply_editors
                .get("PRRT_1")
                .cloned()
                .expect("reply editor")
        });
        type_text(&editor, "Agreed.", &mut cx);
        cx.run_until_parked();
        assert!(fake.requests().is_empty());

        let task = item.update(&mut cx, |item, cx| item.click_reply("PRRT_1", cx));
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("replied");
        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .args
                .contains(&"repos/acme/web/pulls/482/comments/1001/replies".to_string())
        );
        assert_eq!(request_json(&requests[0]), json!({"body": "Agreed."}));
        item.read_with(&cx, |item, _| {
            let thread = &item.detail.threads[0];
            let last = thread.comments.last().expect("comment");
            assert_eq!((last.database_id, last.body.as_str()), (2001, "Agreed."));
        });
        assert!(
            editor
                .read_with(&cx, |editor, cx| editor.text(cx))
                .is_empty()
        );
    }

    #[gpui::test]
    async fn resolve_and_viewed_apply_immediately(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        let ok = || Ok(r#"{"data":{"x":{}}}"#.to_string());
        fake.respond(
            |request| {
                request
                    .stdin
                    .as_deref()
                    .is_some_and(|stdin| stdin.contains("resolveReviewThread("))
            },
            ok(),
        );
        fake.respond(
            |request| {
                request
                    .stdin
                    .as_deref()
                    .is_some_and(|stdin| stdin.contains("markFileAsViewed("))
            },
            ok(),
        );
        let resolved = |item: &Entity<ReviewItem>, cx: &mut VisualTestContext| {
            item.read_with(cx, |item, _| item.detail.threads[0].is_resolved)
        };
        let viewed = |item: &Entity<ReviewItem>, path: &str, cx: &mut VisualTestContext| {
            item.read_with(cx, |item, _| {
                item.detail
                    .files
                    .iter()
                    .any(|file| file.path == path && file.viewed)
            })
        };

        let task = item.update(&mut cx, |item, cx| {
            item.set_thread_resolved("PRRT_1", true, cx)
        });
        assert!(resolved(&item, &mut cx));
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("resolved");
        assert!(resolved(&item, &mut cx));
        assert_eq!(fake.requests().len(), 1);

        // No response is queued for unresolve, so FakeGh fails it.
        let task = item.update(&mut cx, |item, cx| {
            item.set_thread_resolved("PRRT_1", false, cx)
        });
        assert!(!resolved(&item, &mut cx));
        cx.run_until_parked();
        assert!(task.now_or_never().expect("finished").is_err());
        assert!(resolved(&item, &mut cx));
        item.read_with(&cx, |item, _| {
            assert!(item.thread_errors.contains_key("PRRT_1"))
        });
        assert_eq!(fake.requests().len(), 2);

        let task = item.update(&mut cx, |item, cx| {
            item.set_file_viewed("src/retry.rs", true, cx)
        });
        assert!(viewed(&item, "src/retry.rs", &mut cx));
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("viewed");
        assert!(viewed(&item, "src/retry.rs", &mut cx));
        let request = request_json(&fake.requests()[2]);
        assert_eq!(
            request["variables"],
            json!({"pullRequestId": "PR_482", "path": "src/retry.rs"})
        );

        let task = item.update(&mut cx, |item, cx| {
            item.set_file_viewed("src/upload.rs", false, cx)
        });
        assert!(!viewed(&item, "src/upload.rs", &mut cx));
        cx.run_until_parked();
        assert!(task.now_or_never().expect("finished").is_err());
        assert!(viewed(&item, "src/upload.rs", &mut cx));
        item.read_with(&cx, |item, _| {
            assert!(item.file_errors.contains_key("src/upload.rs"))
        });
        assert_eq!(fake.requests().len(), 4);
    }

    #[gpui::test]
    async fn no_requests_without_user_action(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, submit_drafts()).await;
        let fake = install_fake(&mut cx);
        select_text(&item, "audit(user);", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        type_text(&composer(&item, &mut cx), "Typed, not sent", &mut cx);
        let step = step_keys(&item, &mut cx)[0].clone();
        item.update(&mut cx, |item, cx| item.set_step_viewed(&step, true, cx));
        cx.run_until_parked();
        assert!(fake.requests().is_empty());
        assert!(
            database
                .viewed_steps(&key())
                .expect("steps")
                .contains(&step)
        );
        assert_eq!(database.drafts(&key()).expect("drafts").len(), 4);
    }

    #[gpui::test]
    async fn review_closed_mid_submit_leaves_no_row(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        // The draft's first save hasn't run when Submit is clicked, so it has no row id yet.
        let task = item.update_in(&mut cx, |item, window, cx| {
            let local_id = item.push_draft(current_draft("src/login.rs", 3, "    audit(user);"));
            item.queue_save(local_id, cx);
            item.submit(crate::ReviewEvent::Comment, window, cx)
        });
        cx.update(|window, _| window.remove_window());
        drop(item);
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("posted");
        assert_eq!(
            fake.requests().iter().filter(|r| is_review_post(r)).count(),
            1
        );
        assert!(database.drafts(&key()).expect("drafts").is_empty());
    }

    #[gpui::test]
    async fn sending_row_reopens_marked_and_is_not_sent(cx: &mut TestAppContext) {
        let maybe_posted = Draft {
            post_state: crate::PostState::Sending,
            ..current_draft("src/login.rs", 3, "    audit(user);")
        };
        let (item, mut cx, database) = open_with_drafts(
            cx,
            vec![maybe_posted, current_draft("src/login.rs", 5, "}")],
        )
        .await;
        item.read_with(&cx, |item, _| {
            assert_eq!(item.drafts()[0].post_state, PostState::Sending);
            assert_eq!(item.sendable_drafts(), 1);
            assert_eq!(item.left_out_drafts(), 0);
        });
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        submit(&item, crate::ReviewEvent::Comment, &mut cx).expect("submitted");
        let posts: Vec<_> = fake.requests().into_iter().filter(is_review_post).collect();
        let comments = request_json(&posts[0])["comments"].clone();
        assert_eq!(comments.as_array().expect("comments").len(), 1);
        assert_eq!(comments[0]["line"], 4);
        let stored = database.drafts(&key()).expect("drafts");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].post_state, PostState::Sending);

        item.update(&mut cx, |item, cx| item.include_again(0, cx));
        cx.run_until_parked();
        item.read_with(&cx, |item, _| assert_eq!(item.sendable_drafts(), 1));
        assert_eq!(
            database.drafts(&key()).expect("drafts")[0].post_state,
            PostState::Unposted
        );
    }

    #[gpui::test]
    async fn second_click_while_pending_sends_nothing(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        let ok = || Ok(r#"{"data":{"x":{}}}"#.to_string());
        fake.respond(|_| true, ok());
        fake.respond(|_| true, ok());
        item.update(&mut cx, |item, cx| {
            item.set_thread_resolved("PRRT_1", true, cx).detach();
            item.set_thread_resolved("PRRT_1", false, cx).detach();
            item.set_file_viewed("src/retry.rs", true, cx).detach();
            item.set_file_viewed("src/retry.rs", false, cx).detach();
        });
        cx.run_until_parked();
        assert_eq!(fake.requests().len(), 2);
        item.read_with(&cx, |item, _| {
            assert!(item.detail.threads[0].is_resolved);
            assert!(item.file_viewed("src/retry.rs"));
            assert!(item.resolving.is_empty() && item.marking_viewed.is_empty());
        });
    }

    #[gpui::test]
    async fn failed_refresh_after_post_shows_note(cx: &mut TestAppContext) {
        let (item, mut cx, _) = open_with_drafts(cx, submit_drafts()).await;
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        let task = item.update_in(&mut cx, |item, window, cx| {
            item.summary_editor
                .update(cx, |editor, cx| editor.set_text("Sent", window, cx));
            let task = item.submit(crate::ReviewEvent::Comment, window, cx);
            item.summary_editor
                .update(cx, |editor, cx| editor.set_text("Typed after", window, cx));
            task
        });
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("posted");
        item.read_with(&cx, |item, cx| {
            assert_eq!(
                item.submit_note.as_deref(),
                Some("Review posted. Couldn't refresh: FakeGh: unexpected request")
            );
            assert!(item.submit_error.is_none());
            assert!(item.drafts().is_empty());
            assert_eq!(item.summary_editor.read(cx).text(cx), "Typed after");
        });
    }

    #[gpui::test]
    async fn sending_general_draft_can_be_included_or_deleted(cx: &mut TestAppContext) {
        let stuck = Draft {
            outdated: true,
            general: true,
            post_state: PostState::Sending,
            ..stored_draft("src/login.rs", 2, "    gone();")
        };
        let (item, mut cx, database) = open_with_drafts(cx, vec![stuck.clone(), stuck]).await;
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(
                gpui::AvailableSpace::Definite(px(1200.)),
                gpui::AvailableSpace::Definite(px(800.)),
            ),
            |_, _| item.clone().into_any_element(),
        );
        item.read_with(&cx, |item, _| {
            assert_eq!(item.posted_note(&item.drafts()[0]), Some(INTERRUPTED_NOTE));
            assert_eq!(item.sendable_drafts(), 0);
            assert_eq!(
                item.left_out_note().as_deref(),
                Some("2 may already be posted")
            );
        });

        item.update(&mut cx, |item, cx| item.include_again(0, cx));
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            assert_eq!(item.posted_note(&item.drafts()[0]), None);
            assert_eq!(item.sendable_drafts(), 1);
            assert_eq!(
                item.left_out_note().as_deref(),
                Some("1 may already be posted")
            );
        });

        item.update_in(&mut cx, |item, window, cx| item.cancel_draft(1, window, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Discard");
        cx.run_until_parked();
        let stored = database.drafts(&key()).expect("drafts");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].post_state, PostState::Unposted);
    }

    #[gpui::test]
    async fn draft_inserted_during_submit_is_marked_before_post(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        let marked = database.clone();
        fake.respond(
            move |request| {
                is_review_post(request)
                    && marked.drafts(&key()).is_ok_and(|drafts| {
                        drafts.len() == 1 && drafts[0].post_state == PostState::Sending
                    })
            },
            Ok("{}".into()),
        );
        fake.respond(is_detail, Ok(DETAIL.into()));
        let task = item.update_in(&mut cx, |item, window, cx| {
            let local_id = item.push_draft(current_draft("src/login.rs", 3, "    audit(user);"));
            item.queue_save(local_id, cx);
            item.submit(crate::ReviewEvent::Comment, window, cx)
        });
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("posted");
        assert!(database.drafts(&key()).expect("drafts").is_empty());
    }

    #[gpui::test]
    async fn draft_edited_during_submit_says_earlier_version_posted(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(
            cx,
            vec![current_draft("src/login.rs", 3, "    audit(user);")],
        )
        .await;
        let fake = FakeGh::new();
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        let (release, gate) = futures::channel::oneshot::channel::<()>();
        cx.update(|_, cx| {
            crate::set_gh_runner(
                Arc::new(GatedGh {
                    fake: fake.clone(),
                    gate: gate.shared(),
                }),
                cx,
            )
        });
        let task = item.update_in(&mut cx, |item, window, cx| {
            item.submit(crate::ReviewEvent::Comment, window, cx)
        });
        cx.run_until_parked();
        item.update(&mut cx, |item, cx| {
            item.drafts[0].body = "Edited while posting".into();
            item.queue_save(0, cx);
        });
        release.send(()).expect("release");
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("posted");
        item.read_with(&cx, |item, _| {
            assert_eq!(
                item.posted_note(&item.drafts()[0]),
                Some(EARLIER_VERSION_NOTE)
            );
            assert_eq!(item.sendable_drafts(), 0);
        });
        let stored = database.drafts(&key()).expect("drafts");
        assert_eq!(stored[0].body, "Edited while posting");
        assert_eq!(stored[0].post_state, PostState::EarlierVersionPosted);
    }

    #[gpui::test]
    async fn failed_mark_sends_nothing(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(cx, Vec::new()).await;
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        crate::store::FAILING_SAVES.store(1, Ordering::SeqCst);
        let task = item.update_in(&mut cx, |item, window, cx| {
            let local_id = item.push_draft(current_draft("src/login.rs", 3, "    audit(user);"));
            item.queue_save(local_id, cx);
            item.submit(crate::ReviewEvent::Comment, window, cx)
        });
        cx.run_until_parked();
        assert!(task.now_or_never().expect("finished").is_err());
        assert!(fake.requests().is_empty());
        item.read_with(&cx, |item, _| {
            assert!(item.submit_error.is_some());
            assert_eq!(item.sendable_drafts(), 1);
        });
        let stored = database.drafts(&key()).expect("drafts");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].post_state, PostState::Unposted);
    }

    /// Answers like the fake, but only once the gate opens.
    struct GatedGh {
        fake: Arc<FakeGh>,
        gate: Gate,
    }

    impl GhRunner for GatedGh {
        fn run(
            &self,
            request: crate::GhRequest,
        ) -> futures::future::BoxFuture<'static, Result<String, GhError>> {
            let answer = self.fake.run(request);
            let gate = self.gate.clone();
            Box::pin(async move {
                gate.await.ok();
                answer.await
            })
        }
    }

    #[gpui::test]
    async fn edit_during_post_survives_closing_the_review(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(
            cx,
            vec![current_draft("src/login.rs", 3, "    audit(user);")],
        )
        .await;
        let fake = FakeGh::new();
        fake.respond(is_review_post, Ok("{}".into()));
        let (release, gate) = futures::channel::oneshot::channel::<()>();
        cx.update(|_, cx| {
            crate::set_gh_runner(
                Arc::new(GatedGh {
                    fake: fake.clone(),
                    gate: gate.shared(),
                }),
                cx,
            )
        });
        let task = item.update_in(&mut cx, |item, window, cx| {
            item.submit(crate::ReviewEvent::Comment, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(fake.requests().len(), 1);
        item.update(&mut cx, |item, cx| {
            item.drafts[0].body = "Edited while posting".into();
            item.queue_save(0, cx);
        });
        cx.run_until_parked();
        cx.update(|window, _| window.remove_window());
        drop(item);
        release.send(()).expect("release");
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("posted");
        let stored = database.drafts(&key()).expect("drafts");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].body, "Edited while posting");
        assert_eq!(stored[0].post_state, PostState::EarlierVersionPosted);
    }

    #[gpui::test]
    async fn drafts_removed_or_emptied_while_waiting_are_left_out(cx: &mut TestAppContext) {
        let (item, mut cx, database) = open_with_drafts(
            cx,
            vec![
                current_draft("src/login.rs", 3, "    audit(user);"),
                current_draft("src/login.rs", 5, "}"),
                current_draft("src/login.rs", 2, "    let user = user.trim();"),
            ],
        )
        .await;
        let fake = install_fake(&mut cx);
        fake.respond(is_review_post, Ok("{}".into()));
        fake.respond(is_detail, Ok(DETAIL.into()));
        let task = item.update_in(&mut cx, |item, window, cx| {
            let task = item.submit(crate::ReviewEvent::Comment, window, cx);
            item.discard_draft(0, cx);
            if let Some(index) = item.draft_index(1) {
                item.drafts[index].body = String::new();
            }
            item.queue_save(1, cx);
            task
        });
        cx.run_until_parked();
        task.now_or_never().expect("finished").expect("posted");
        let posts: Vec<_> = fake.requests().into_iter().filter(is_review_post).collect();
        let comments = request_json(&posts[0])["comments"].clone();
        assert_eq!(comments.as_array().expect("comments").len(), 1);
        assert_eq!(comments[0]["line"], 2);
        assert!(database.drafts(&key()).expect("drafts").is_empty());
        item.read_with(&cx, |item, _| {
            assert!(item.submit_error.is_none());
            let left: Vec<_> = item
                .drafts()
                .iter()
                .map(|draft| (draft.body.as_str(), draft.post_state))
                .collect();
            assert_eq!(left, vec![("", PostState::Unposted)]);
        });
    }

    #[gpui::test]
    async fn since_review_mode_blocks_base_comments_and_viewed(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            json!({ "src": { "b.rs": B_HEAD } }),
            vec![modified("src/b.rs", B_BASE)],
            Graph {
                functions: vec![function("callee", "src/b.rs", 0, 2)],
                calls: Vec::new(),
            },
            true,
            None,
        )
        .await;
        settle(&mut cx);
        item.update(&mut cx, |item, _| item.full_files = Some(Vec::new()));
        select_text(&item, "let a = 1;", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        let step = step_keys(&item, &mut cx)[0].clone();
        item.update(&mut cx, |item, cx| item.set_step_viewed(&step, true, cx));
        item.read_with(&cx, |item, _| {
            assert!(item.drafts().is_empty());
            assert_eq!(item.since_review_note.as_deref(), Some(LEFT_COMMENT_NOTE));
            assert!(item.viewed_steps.is_empty());
        });
        select_text(&item, "let b = 2;", &mut cx);
        cx.dispatch_action(AddComment);
        cx.run_until_parked();
        item.read_with(&cx, |item, _| {
            assert_eq!(
                item.drafts().last().map(|draft| draft.side),
                Some(Side::Right)
            );
        });
    }

    #[gpui::test]
    async fn second_click_while_loading_cancels(cx: &mut TestAppContext) {
        let (item, mut cx) = open_item(
            cx,
            login_tree(),
            vec![login_with_hunk()],
            login_graph(),
            true,
            None,
        )
        .await;
        item.update_in(&mut cx, |item, window, cx| {
            item.loading_since_review = true;
            item.toggle_changes_since_review(&ToggleChangesSinceReview, window, cx);
            assert!(!item.loading_since_review);
            assert!(!item.showing_changes_since_review());
        });
    }

    #[gpui::test]
    async fn changes_since_review_toggle_offered_only_when_pushed(cx: &mut TestAppContext) {
        for (on_github, last_review, offered) in [
            (None, None, false),
            (None, Some("head"), false),
            (None, Some("older"), true),
            (Some("older"), None, true),
            (Some("older"), Some("head"), true),
            (Some("head"), Some("older"), false),
        ] {
            let database = test_database(cx);
            database
                .track(Tracked {
                    key: key(),
                    node_id: "PR_482".into(),
                    repo_dir: PathBuf::from(path!("/repo")),
                    worktree: PathBuf::from(path!("/repo")),
                })
                .await
                .expect("track");
            if let Some(commit) = last_review {
                database
                    .set_last_review(key(), commit.into())
                    .await
                    .expect("set last review");
            }
            let mut detail = parse_detail(&key(), DETAIL).expect("detail");
            detail.summary.last_review_commit = on_github.map(str::to_string);
            let (item, cx, _) = open_item_with_detail(
                cx,
                login_tree(),
                vec![login_with_hunk()],
                login_graph(),
                true,
                None,
                false,
                Some(database),
                detail,
            )
            .await;
            assert_eq!(
                item.read_with(&cx, |item, _| item.changes_since_review_offered()),
                offered,
                "github {on_github:?}, database {last_review:?}"
            );
        }
    }
}
