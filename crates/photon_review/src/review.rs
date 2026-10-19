use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use buffer_diff::BufferDiff;
use editor::{
    Editor, EditorEvent, ExcerptRange, MultiBuffer, PathKey, RowHighlightOptions,
    display_map::{
        BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, RenderBlock,
    },
};
use futures::future::{join_all, try_join_all};
use gpui::{
    App, AsyncWindowContext, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    HighlightStyle, Render, SharedString, StyledText, Subscription, Task, WeakEntity, Window,
    actions, rgb,
};
use language::{Buffer, Capability, Point};
use multi_buffer::build_excerpt_ranges;
use project::{Project, lsp_store::OpenLspBufferHandle};
use ui::prelude::*;
use util::{ResultExt as _, rel_path::RelPath};
use workspace::{
    AppState, Item, OpenMode, OpenOptions, Toast, Workspace, notifications::NotificationId,
    open_paths,
};

use crate::{
    CallerMark, ChangedFunction, Checkout, ChecksState, ContextCache, ContextError, ContextExcerpt,
    FileAnalysis, FileDiff, FileKind, FileStatus, FunctionContext, GhError, GhRunner, LoadedFile,
    MapFile, MapFunction, PrDetail, PrSummary, RepoId, ReviewDb, Step, Tracked,
    analyse_loaded_file, auth_status_request, base_text, build_loaded_file, build_review_map,
    call_edges, caller_touched, candidate_repos, classify_caller, classify_path,
    context::buffer_path, context_excerpts, detail_request, fetch_url, find_clone, functions,
    gh_runner, git_diff, parse_auth_status, parse_detail, prepare_checkout, review_root,
};

actions!(
    photon_review,
    [
        /// Shows the next step of the pull request review map.
        NextStep,
        /// Shows the previous step of the pull request review map.
        PreviousStep
    ]
);

const HUNK_CONTEXT_LINES: u32 = 3;
const UNCHANGED_BACKGROUND: u32 = 0x07090b;
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
    _lsp_handles: Vec<OpenLspBufferHandle>,
    _subscriptions: Vec<Subscription>,
    _context_task: Task<()>,
}

pub(crate) struct PreparedReview {
    pub checkout: Checkout,
    pub detail: PrDetail,
    pub files: Vec<(FileDiff, Option<String>)>,
}

/// Errors are shown as a toast in the window the user clicked from.
pub fn open_review(
    summary: PrSummary,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let app_state = workspace.app_state().clone();
    let runner = gh_runner(cx);
    let database = ReviewDb::global(cx);
    let candidates = candidate_repos(cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let result = open_in_new_window(summary, app_state, runner, database, candidates, cx).await;
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

async fn open_in_new_window(
    summary: PrSummary,
    app_state: Arc<AppState>,
    runner: Arc<dyn GhRunner>,
    database: ReviewDb,
    candidates: Task<Vec<PathBuf>>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let candidates = candidates.await;
    let url = fetch_url(&repo_id(&summary));
    let prepared = cx
        .background_spawn(async move {
            prepare_review(&summary, url, review_root(), candidates, runner, database).await
        })
        .await?;
    let PreparedReview {
        checkout,
        detail,
        files,
    } = prepared;
    let opened = cx
        .update(|_, cx| {
            open_paths(
                std::slice::from_ref(&checkout.worktree),
                app_state,
                OpenOptions {
                    open_mode: OpenMode::NewWindow,
                    ..OpenOptions::default()
                },
                cx,
            )
        })?
        .await?;
    let project = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.project().clone());
    let loads = cx.update(|_, cx| {
        files
            .into_iter()
            .map(|(diff, base)| {
                let abs_path = checkout.worktree.join(&diff.path);
                build_loaded_file(&project, abs_path, base, diff, cx)
            })
            .collect::<Vec<_>>()
    })?;
    let files = try_join_all(loads).await?;
    let key = detail.summary.key.clone();
    opened.window.update(cx, |_, window, cx| {
        opened.workspace.update(cx, |workspace, cx| {
            let existing = workspace
                .items_of_type::<ReviewItem>(cx)
                .find(|item| item.read(cx).detail.summary.key == key);
            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
                return;
            }
            let item = cx.new(|cx| ReviewItem::new(project, detail, checkout, files, window, cx));
            workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
        })
    })?;
    Ok(())
}

fn repo_id(summary: &PrSummary) -> RepoId {
    RepoId {
        host: summary.key.host.to_lowercase(),
        owner: summary.key.owner.to_lowercase(),
        repo: summary.key.repo.to_lowercase(),
    }
}

/// Everything `open_review` does before it opens a window: the checkout, the detail and the
/// diff. The user's clone only gains Photon's refs and worktree entry (see `prepare_checkout`).
pub(crate) async fn prepare_review(
    summary: &PrSummary,
    fetch_url: String,
    root: PathBuf,
    candidates: Vec<PathBuf>,
    runner: Arc<dyn GhRunner>,
    database: ReviewDb,
) -> Result<PreparedReview> {
    let key = &summary.key;
    let clone = find_clone(candidates, &repo_id(summary)).await;
    let checkout = prepare_checkout(key, &summary.base_ref, &fetch_url, clone, &root).await?;
    database
        .track(Tracked {
            key: key.clone(),
            node_id: summary.node_id.clone(),
            repo_dir: checkout.repo_dir.clone(),
            worktree: checkout.worktree.clone(),
        })
        .await?;

    let hosts = parse_auth_status(&runner.run(auth_status_request()).await?)?;
    let login = hosts
        .into_iter()
        .find(|auth| auth.host.eq_ignore_ascii_case(&key.host))
        .and_then(|auth| auth.login)
        .ok_or_else(|| GhError::LoggedOut {
            host: Some(key.host.clone()),
        })?;
    let detail = parse_detail(key, &runner.run(detail_request(key, &login)).await?)?;

    let diffs = git_diff(&checkout.worktree, &checkout.merge_base, &checkout.head_sha).await?;
    let mut files = Vec::with_capacity(diffs.len());
    for diff in diffs {
        let base = base_text(&checkout.worktree, &checkout.merge_base, &diff).await?;
        files.push((diff, base));
    }
    Ok(PreparedReview {
        checkout,
        detail,
        files,
    })
}

fn status_label(diff: &FileDiff) -> &'static str {
    if diff.submodule {
        return "submodule";
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

impl ReviewItem {
    pub fn new(
        project: Entity<Project>,
        detail: PrDetail,
        checkout: Checkout,
        files: Vec<LoadedFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            Editor::for_multibuffer(multibuffer.clone(), Some(project.clone()), window, cx)
        });
        // Call hierarchy needs every head buffer open in its language server.
        let mut lsp_handles = Vec::new();
        let mut changed_rows = HashMap::new();
        for file in &files {
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
        let subscription = cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::SelectionsChanged { .. }) {
                this.update_impact(cx);
            }
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
            viewed_steps: HashSet::new(),
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
            _lsp_handles: lsp_handles,
            _subscriptions: vec![subscription],
            _context_task: Task::ready(()),
        };
        this.rebuild_map(&[], cx);
        this._context_task = this.load_context(cx);
        this
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
                    this.fetch_contexts(cx)
                })
                .log_err()
            else {
                return;
            };
            let results = join_all(fetches).await;
            this.update(cx, |this, cx| this.set_contexts(results, cx))
                .log_err();
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
                    ),
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
                        Label::new(format!("0 draft comments · {}", self.progress_text()))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .child(Button::new("photon-review-comment", "Comment"))
                            .child(
                                Button::new("photon-review-approve", "Approve")
                                    .style(ButtonStyle::Filled),
                            )
                            .child(Button::new(
                                "photon-review-request-changes",
                                "Request changes",
                            )),
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
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;
    use util::path;

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

    fn answer_call_hierarchy(server: &mut lsp::FakeLanguageServer, graph: &Arc<Graph>, gate: Gate) {
        server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
            let graph = graph.clone();
            move |params, _| {
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
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/repo"), tree).await;
        let project = Project::test(fs, [path!("/repo").as_ref()], cx).await;
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
                initializer: Some(Box::new(move |server| {
                    answer_call_hierarchy(server, &graph, gate.clone())
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
                hunks: Vec::new(),
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
        let detail = parse_detail(&key(), DETAIL).expect("detail");
        let window = cx.add_window(|window, cx| {
            ReviewItem::new(project.clone(), detail, checkout(), loaded, window, cx)
        });
        let item = window.root(cx).expect("item");
        let cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        (item, cx)
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

    #[gpui::test]
    async fn open_review_leaves_user_clone_untouched(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        if Command::new("git").arg("--version").output().is_err() {
            return;
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
        std::fs::write(seed.join("a.rs"), "fn a() {\n    1;\n}\n").expect("write");
        run(&seed, &["commit", "-q", "-am", "pr"]);
        let pr_sha = run(&seed, &["rev-parse", "HEAD"]);
        let upstream_url = format!("file://{}", upstream.display());
        run(&seed, &["push", "-q", &upstream_url, "main", "pr"]);
        run(&upstream, &["update-ref", "refs/pull/482/head", &pr_sha]);
        run(&base, &["clone", "-q", &upstream_url, "user"]);
        run(
            &user,
            &["remote", "set-url", "origin", "git@github.com:acme/web.git"],
        );
        std::fs::write(user.join("a.rs"), "dirty\n").expect("write");
        let user_state = || {
            vec![
                run(&user, &["symbolic-ref", "HEAD"]),
                run(&user, &["status", "--porcelain=v1", "-uall"]),
                run(&user, &["stash", "list"]),
                format!("{:?}", std::fs::read(user.join("a.rs")).ok()),
            ]
        };
        let before = user_state();

        let fake = FakeGh::new();
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
        let database = cx.foreground_executor().block_test(ReviewDb::open_test_db(
            "open_review_leaves_user_clone_untouched",
        ));
        let summary = parse_detail(&key(), DETAIL).expect("detail").summary;
        let prepared = prepare_review(
            &summary,
            upstream_url,
            base.join("root"),
            vec![user.clone()],
            fake.clone(),
            database.clone(),
        )
        .await
        .expect("prepared");

        assert_eq!(user_state(), before);
        assert_eq!(prepared.checkout.head_sha, pr_sha);
        assert_eq!(prepared.checkout.repo_dir, user.join(".git"));
        assert!(prepared.checkout.worktree.starts_with(base.join("root")));
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
}
