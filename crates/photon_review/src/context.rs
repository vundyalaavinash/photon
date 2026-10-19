use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::pin::pin;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures::future::{self, Either};
use gpui::{App, AsyncApp, Entity, Task};
use language::{Buffer, Point, ToPoint as _};
use project::{IncomingCall, OutgoingCall, Project};

use crate::functions::{ChangedFunction, FunctionChange};

pub const CONTEXT_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
];

const CONTEXT_FETCH_LIMIT: Duration = Duration::from_secs(30);

pub const UNCHANGED_EXCERPT_LINES: u32 = 8;

#[derive(Clone, Debug)]
pub struct FunctionContext {
    pub callers: Vec<IncomingCall>,
    pub callees: Vec<OutgoingCall>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextError {
    NoCallHierarchy,
    TimedOut,
}

type CacheKey = (String, String, Vec<String>);

#[derive(Default)]
pub struct ContextCache {
    entries: Rc<RefCell<HashMap<CacheKey, FunctionContext>>>,
}

impl ContextCache {
    pub fn fetch(
        &mut self,
        head_sha: &str,
        project: Entity<Project>,
        buffer: Entity<Buffer>,
        function: &ChangedFunction,
        cx: &mut App,
    ) -> Task<Result<FunctionContext, ContextError>> {
        let key = (
            head_sha.to_string(),
            function.path.clone(),
            function.outline_path.clone(),
        );
        if let Some(context) = self.entries.borrow().get(&key) {
            return Task::ready(Ok(context.clone()));
        }
        let entries = self.entries.clone();
        let position = function.selection_point;
        // A server that hangs instead of erroring would hold each attempt for the whole LSP
        // request timeout, so the attempts and their delays share one overall limit.
        let limit = cx.background_executor().timer(CONTEXT_FETCH_LIMIT);
        cx.spawn(async move |cx| {
            let attempts = pin!(async {
                let mut delays = CONTEXT_RETRY_DELAYS.iter();
                loop {
                    match fetch_once(&project, &buffer, position, cx).await {
                        Ok(Some(context)) => {
                            entries.borrow_mut().insert(key, context.clone());
                            return Ok(context);
                        }
                        Ok(None) => return Err(ContextError::NoCallHierarchy),
                        Err(error) => {
                            log::debug!("call hierarchy not ready: {error:#}");
                            match delays.next() {
                                Some(delay) => cx.background_executor().timer(*delay).await,
                                None => return Err(ContextError::TimedOut),
                            }
                        }
                    }
                }
            });
            match future::select(attempts, limit).await {
                Either::Left((result, _)) => result,
                Either::Right(_) => Err(ContextError::TimedOut),
            }
        })
    }
}

/// `Ok(None)` when no language server offers call hierarchy; `Err` when one may still answer.
async fn fetch_once(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    position: Point,
    cx: &mut AsyncApp,
) -> Result<Option<FunctionContext>> {
    if !cx.update(|cx| call_hierarchy_available(project, buffer, cx))? {
        return Ok(None);
    }
    let prepare = project.update(cx, |project, cx| {
        project.prepare_call_hierarchy(buffer, position, cx)
    });
    let Some(items) = prepare.await? else {
        return Ok(None);
    };
    // Local requests drop server errors (rust-analyzer's "content modified" while indexing) and
    // answer with no items, so an empty answer from a capable server means "not ready yet".
    // ponytail: a server with genuinely nothing at this position also answers empty, so it
    // reports TimedOut after 30 s; tell "indexing" from "nothing here" via LSP `$/progress`.
    let item = items
        .into_iter()
        .next()
        .context("language server returned no call hierarchy item")?;
    let incoming = project.update(cx, |project, cx| project.incoming_calls(item.clone(), cx));
    let outgoing = project.update(cx, |project, cx| project.outgoing_calls(item, cx));
    let (Some(callers), Some(callees)) = (incoming.await?, outgoing.await?) else {
        return Ok(None);
    };
    Ok(Some(FunctionContext { callers, callees }))
}

/// `Err` while a server for the buffer is still starting and its capabilities are unknown.
fn call_hierarchy_available(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    cx: &mut App,
) -> Result<bool> {
    // Remote projects check capabilities on the host, and `prepare_call_hierarchy` answers
    // `None` there when no server can help.
    if !project.read(cx).is_local() {
        return Ok(true);
    }
    let lsp_store = project.read(cx).lsp_store();
    lsp_store.update(cx, |lsp_store, cx| {
        let server_ids = buffer.update(cx, |buffer, cx| {
            lsp_store.language_servers_for_local_buffer(buffer, cx)
        });
        let mut starting = false;
        for server_id in server_ids {
            match lsp_store.lsp_server_capabilities.get(&server_id) {
                Some(capabilities) => {
                    let supported = match &capabilities.call_hierarchy_provider {
                        Some(lsp::CallHierarchyServerCapability::Simple(supported)) => *supported,
                        Some(lsp::CallHierarchyServerCapability::Options(_)) => true,
                        None => false,
                    };
                    if supported {
                        return Ok(true);
                    }
                }
                None => starting = true,
            }
        }
        anyhow::ensure!(!starting, "language server is starting");
        Ok(false)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallerMark {
    InThisPr,
    Unchanged,
    NotUpdated,
    Check,
}

pub fn classify_caller(change: Option<FunctionChange>, caller_touched: bool) -> CallerMark {
    if caller_touched {
        return CallerMark::InThisPr;
    }
    match change {
        None => CallerMark::Unchanged,
        Some(FunctionChange::BodyOnly | FunctionChange::Added) => CallerMark::Check,
        Some(FunctionChange::Signature) => CallerMark::NotUpdated,
    }
}

/// A caller is touched when one of its call sites lies in its file's changed rows.
pub fn caller_touched(
    call: &IncomingCall,
    changed_rows: &HashMap<String, Vec<Range<u32>>>,
    cx: &App,
) -> bool {
    call.from_ranges.iter().any(|location| {
        let Some(rows) = buffer_path(&location.buffer, cx).and_then(|path| changed_rows.get(&path))
        else {
            return false;
        };
        let snapshot = location.buffer.read(cx).snapshot();
        let start = location.range.start.to_point(&snapshot).row;
        let end = location.range.end.to_point(&snapshot).row;
        rows.iter()
            .any(|changed| changed.start <= end && start < changed.end)
    })
}

pub fn call_edges(
    functions: &[ChangedFunction],
    contexts: &[Option<FunctionContext>],
    cx: &App,
) -> Vec<(usize, usize)> {
    let mut edges = Vec::new();
    for (caller_index, context) in contexts.iter().enumerate() {
        let Some(context) = context else {
            continue;
        };
        for call in &context.callees {
            let Some(path) = buffer_path(&call.to.buffer, cx) else {
                continue;
            };
            let target = call
                .to
                .selection_range
                .start
                .to_point(&call.to.buffer.read(cx).snapshot());
            let callee_index = functions.iter().position(|function| {
                function.path == path
                    && function.range.start <= target
                    && target <= function.range.end
            });
            if let Some(callee_index) = callee_index
                && !edges.contains(&(caller_index, callee_index))
            {
                edges.push((caller_index, callee_index));
            }
        }
    }
    edges
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Callee,
    Caller,
}

#[derive(Clone, Debug)]
pub struct ContextExcerpt {
    pub buffer: Entity<Buffer>,
    pub range: Range<Point>,
    pub name: String,
    pub label: String,
    pub full_rows: u32,
}

pub fn context_excerpts(
    function: &ChangedFunction,
    context: &FunctionContext,
    changed_rows: &HashMap<String, Vec<Range<u32>>>,
    cx: &App,
) -> Vec<ContextExcerpt> {
    let callees = context
        .callees
        .iter()
        .map(|call| (Direction::Callee, &call.to, &call.from_ranges));
    let callers = context
        .callers
        .iter()
        .map(|call| (Direction::Caller, &call.from, &call.from_ranges));
    let mut seen: Vec<(String, Range<Point>)> = Vec::new();
    let mut excerpts = Vec::new();
    for (direction, item, call_sites) in callees.chain(callers) {
        let Some(path) = buffer_path(&item.buffer, cx) else {
            continue;
        };
        let snapshot = item.buffer.read(cx).snapshot();
        let start = item.range.start.to_point(&snapshot);
        // A server may answer with an inverted range; show its start row alone.
        let range = start..item.range.end.to_point(&snapshot).max(start);
        let is_changed = changed_rows.get(&path).is_some_and(|rows| {
            rows.iter()
                .any(|changed| changed.start <= range.end.row && range.start.row < changed.end)
        });
        if is_changed || seen.contains(&(path.clone(), range.clone())) {
            continue;
        }
        seen.push((path.clone(), range.clone()));

        let line = call_sites
            .iter()
            .map(|location| {
                location
                    .range
                    .start
                    .to_point(&location.buffer.read(cx).snapshot())
                    .row
            })
            .min()
            .unwrap_or(range.start.row)
            + 1;
        let full_rows = range.end.row.saturating_sub(range.start.row) + 1;
        let shown = if full_rows > UNCHANGED_EXCERPT_LINES {
            let last_row = range.start.row + UNCHANGED_EXCERPT_LINES - 1;
            range.start..Point::new(last_row, snapshot.line_len(last_row))
        } else {
            range
        };
        excerpts.push(ContextExcerpt {
            buffer: item.buffer.clone(),
            range: shown,
            name: item.name.clone(),
            label: context_label(direction, &path, &item.name, &function.name, line),
            full_rows,
        });
    }
    excerpts
}

pub fn context_label(
    direction: Direction,
    path: &str,
    name: &str,
    changed_name: &str,
    line: u32,
) -> String {
    match direction {
        Direction::Callee => format!("unchanged · {path} · {name}() · called on line {line}"),
        Direction::Caller => {
            format!("unchanged · {path} · {name}() · calls {changed_name}() on line {line}")
        }
    }
}

pub(crate) fn buffer_path(buffer: &Entity<Buffer>, cx: &App) -> Option<String> {
    let file = buffer.read(cx).file()?;
    Some(file.path().as_unix_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use futures::StreamExt as _;
    use gpui::TestAppContext;
    use language::FakeLspAdapter;
    use project::lsp_store::OpenLspBufferHandle;
    use serde_json::json;
    use settings::SettingsStore;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use util::path;

    const SOURCE: &str = "\
fn changed() {
    helper();
}

fn helper() {
    let a = 1;
    let a = 1;
    let a = 1;
    let a = 1;
    let a = 1;
    let a = 1;
    let a = 1;
    let a = 1;
    let a = 1;
    let a = 1;
}

fn caller() {
    changed();
}
";

    struct Setup {
        project: Entity<Project>,
        buffer: Entity<Buffer>,
        server: lsp::FakeLanguageServer,
        uri: lsp::Uri,
        _handle: OpenLspBufferHandle,
    }

    async fn setup(cx: &mut TestAppContext, call_hierarchy: bool) -> Setup {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/repo"), json!({ "src": { "main.rs": SOURCE } }))
            .await;
        let project = Project::test(fs, [path!("/repo").as_ref()], cx).await;
        let registry = project.read_with(cx, |project, _| project.languages().clone());
        registry.add(language::rust_lang());
        let mut servers = registry.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    call_hierarchy_provider: call_hierarchy
                        .then_some(lsp::CallHierarchyServerCapability::Simple(true)),
                    ..lsp::ServerCapabilities::default()
                },
                ..FakeLspAdapter::default()
            },
        );
        let (buffer, handle) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(path!("/repo/src/main.rs"), cx)
            })
            .await
            .expect("open buffer");
        let server = servers.next().await.expect("fake server");
        cx.executor().run_until_parked();
        let uri = lsp::Uri::from_file_path(path!("/repo/src/main.rs")).expect("uri");
        Setup {
            project,
            buffer,
            server,
            uri,
            _handle: handle,
        }
    }

    fn lsp_item(name: &str, uri: &lsp::Uri, start: u32, end: u32) -> lsp::CallHierarchyItem {
        lsp::CallHierarchyItem {
            name: name.to_string(),
            kind: lsp::SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: uri.clone(),
            range: lsp::Range::new(lsp::Position::new(start, 0), lsp::Position::new(end, 1)),
            selection_range: lsp::Range::new(
                lsp::Position::new(start, 3),
                lsp::Position::new(start, 3 + name.len() as u32),
            ),
            data: None,
        }
    }

    fn line_range(line: u32, start: u32, end: u32) -> lsp::Range {
        lsp::Range::new(
            lsp::Position::new(line, start),
            lsp::Position::new(line, end),
        )
    }

    fn changed_function(
        name: &str,
        start: u32,
        end: u32,
        change: FunctionChange,
    ) -> ChangedFunction {
        ChangedFunction {
            path: "src/main.rs".into(),
            name: name.into(),
            outline_path: vec![name.into()],
            range: Point::new(start, 0)..Point::new(end, 1),
            selection_point: Point::new(start, 3),
            change,
        }
    }

    /// Prepare answers with `changed`, which calls `helper` (twice in the answer, and itself
    /// recursively) and is called by `caller`. Returns the prepare call counter.
    fn answer_calls(setup: &Setup) -> Arc<AtomicUsize> {
        let prepares = Arc::new(AtomicUsize::new(0));
        setup
            .server
            .set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
                let uri = setup.uri.clone();
                let prepares = prepares.clone();
                move |_, _| {
                    prepares.fetch_add(1, Ordering::SeqCst);
                    let uri = uri.clone();
                    async move { Ok(Some(vec![lsp_item("changed", &uri, 0, 2)])) }
                }
            });
        setup
            .server
            .set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
                let uri = setup.uri.clone();
                move |_, _| {
                    let uri = uri.clone();
                    async move {
                        Ok(Some(vec![lsp::CallHierarchyIncomingCall {
                            from: lsp_item("caller", &uri, 17, 19),
                            from_ranges: vec![line_range(18, 4, 11)],
                        }]))
                    }
                }
            });
        setup
            .server
            .set_request_handler::<lsp::request::CallHierarchyOutgoingCalls, _, _>({
                let uri = setup.uri.clone();
                move |_, _| {
                    let uri = uri.clone();
                    async move {
                        let helper = lsp::CallHierarchyOutgoingCall {
                            to: lsp_item("helper", &uri, 4, 15),
                            from_ranges: vec![line_range(1, 4, 10)],
                        };
                        Ok(Some(vec![
                            helper.clone(),
                            helper,
                            lsp::CallHierarchyOutgoingCall {
                                to: lsp_item("changed", &uri, 0, 2),
                                from_ranges: vec![line_range(1, 4, 10)],
                            },
                        ]))
                    }
                }
            });
        prepares
    }

    async fn fetch(
        cx: &mut TestAppContext,
        cache: &mut ContextCache,
        head: &str,
        setup: &Setup,
    ) -> Result<FunctionContext, ContextError> {
        let function = changed_function("changed", 0, 2, FunctionChange::BodyOnly);
        let task = cx.update(|cx| {
            cache.fetch(
                head,
                setup.project.clone(),
                setup.buffer.clone(),
                &function,
                cx,
            )
        });
        cx.executor().run_until_parked();
        task.await
    }

    #[gpui::test]
    async fn context_fetches_callers_and_callees(cx: &mut TestAppContext) {
        let setup = setup(cx, true).await;
        answer_calls(&setup);
        let context = fetch(cx, &mut ContextCache::default(), "head", &setup)
            .await
            .expect("context");
        let callers: Vec<_> = context
            .callers
            .iter()
            .map(|call| call.from.name.clone())
            .collect();
        let callees: Vec<_> = context
            .callees
            .iter()
            .map(|call| call.to.name.clone())
            .collect();
        assert_eq!(callers, vec!["caller"]);
        assert_eq!(callees, vec!["helper", "helper", "changed"]);

        let touched = HashMap::from_iter([("src/main.rs".to_string(), vec![18..19])]);
        let untouched = HashMap::from_iter([("src/main.rs".to_string(), vec![1..2])]);
        cx.read(|cx| {
            assert!(caller_touched(&context.callers[0], &touched, cx));
            assert!(!caller_touched(&context.callers[0], &untouched, cx));
        });
    }

    #[gpui::test]
    async fn context_retries_until_server_ready(cx: &mut TestAppContext) {
        let setup = setup(cx, true).await;
        answer_calls(&setup);
        let prepares = Arc::new(AtomicUsize::new(0));
        setup
            .server
            .set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
                let uri = setup.uri.clone();
                let prepares = prepares.clone();
                move |_, _| {
                    let attempt = prepares.fetch_add(1, Ordering::SeqCst);
                    let uri = uri.clone();
                    async move {
                        if attempt == 0 {
                            Err(anyhow::anyhow!("content modified"))
                        } else {
                            Ok(Some(vec![lsp_item("changed", &uri, 0, 2)]))
                        }
                    }
                }
            });
        let function = changed_function("changed", 0, 2, FunctionChange::BodyOnly);
        let mut cache = ContextCache::default();
        let task = cx.update(|cx| {
            cache.fetch(
                "head",
                setup.project.clone(),
                setup.buffer.clone(),
                &function,
                cx,
            )
        });
        cx.executor().run_until_parked();
        assert_eq!(prepares.load(Ordering::SeqCst), 1);
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.executor().run_until_parked();
        assert_eq!(prepares.load(Ordering::SeqCst), 2);
        let context = task.await.expect("context after retry");
        assert_eq!(context.callers.len(), 1);
    }

    #[gpui::test]
    async fn no_call_hierarchy_support_is_reported(cx: &mut TestAppContext) {
        let setup = setup(cx, false).await;
        let result = fetch(cx, &mut ContextCache::default(), "head", &setup).await;
        assert_eq!(result.err(), Some(ContextError::NoCallHierarchy));
    }

    #[gpui::test]
    async fn context_times_out_after_retries(cx: &mut TestAppContext) {
        let setup = setup(cx, true).await;
        let prepares = Arc::new(AtomicUsize::new(0));
        setup
            .server
            .set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
                let prepares = prepares.clone();
                move |_, _| {
                    prepares.fetch_add(1, Ordering::SeqCst);
                    async move { Err(anyhow::anyhow!("content modified")) }
                }
            });
        let function = changed_function("changed", 0, 2, FunctionChange::BodyOnly);
        let mut cache = ContextCache::default();
        let task = cx.update(|cx| {
            cache.fetch(
                "head",
                setup.project.clone(),
                setup.buffer.clone(),
                &function,
                cx,
            )
        });
        cx.executor().run_until_parked();
        for _ in 0..29 {
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.executor().run_until_parked();
        }
        assert_eq!(prepares.load(Ordering::SeqCst), 5);
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.executor().run_until_parked();
        assert_eq!(prepares.load(Ordering::SeqCst), 6);
        assert_eq!(task.await.err(), Some(ContextError::TimedOut));
    }

    #[gpui::test]
    async fn hanging_server_times_out_at_thirty_seconds(cx: &mut TestAppContext) {
        let setup = setup(cx, true).await;
        setup
            .server
            .set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>(|_, _| {
                futures::future::pending::<anyhow::Result<Option<Vec<lsp::CallHierarchyItem>>>>()
            });
        let function = changed_function("changed", 0, 2, FunctionChange::BodyOnly);
        let mut cache = ContextCache::default();
        let task = cx.update(|cx| {
            cache.fetch(
                "head",
                setup.project.clone(),
                setup.buffer.clone(),
                &function,
                cx,
            )
        });
        let result = Rc::new(RefCell::new(None));
        cx.update(|cx| {
            cx.spawn({
                let result = result.clone();
                async move |_| *result.borrow_mut() = Some(task.await.err())
            })
            .detach()
        });
        cx.executor().run_until_parked();
        for _ in 0..29 {
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.executor().run_until_parked();
        }
        assert_eq!(*result.borrow(), None);
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.executor().run_until_parked();
        assert_eq!(*result.borrow(), Some(Some(ContextError::TimedOut)));
    }

    #[gpui::test]
    async fn inverted_range_does_not_underflow(cx: &mut TestAppContext) {
        let setup = setup(cx, true).await;
        answer_calls(&setup);
        setup
            .server
            .set_request_handler::<lsp::request::CallHierarchyOutgoingCalls, _, _>({
                let uri = setup.uri.clone();
                move |_, _| {
                    let mut helper = lsp_item("helper", &uri, 6, 6);
                    helper.range =
                        lsp::Range::new(lsp::Position::new(6, 0), lsp::Position::new(4, 1));
                    async move {
                        Ok(Some(vec![lsp::CallHierarchyOutgoingCall {
                            to: helper,
                            from_ranges: vec![line_range(1, 4, 10)],
                        }]))
                    }
                }
            });
        let context = fetch(cx, &mut ContextCache::default(), "head", &setup)
            .await
            .expect("context");
        let function = changed_function("changed", 0, 2, FunctionChange::BodyOnly);
        let changed_rows = HashMap::from_iter([("src/main.rs".to_string(), vec![1..2])]);
        let excerpts = cx.read(|cx| context_excerpts(&function, &context, &changed_rows, cx));
        let helper = excerpts.first().expect("helper excerpt");
        // Project already reorders the endpoints when it converts the LSP range.
        assert_eq!(helper.range, Point::new(4, 1)..Point::new(6, 0));
        assert_eq!(helper.full_rows, 3);
    }

    #[gpui::test]
    async fn context_cached_per_head(cx: &mut TestAppContext) {
        let setup = setup(cx, true).await;
        let prepares = answer_calls(&setup);
        let mut cache = ContextCache::default();
        fetch(cx, &mut cache, "one", &setup).await.expect("first");
        fetch(cx, &mut cache, "one", &setup).await.expect("cached");
        assert_eq!(prepares.load(Ordering::SeqCst), 1);
        fetch(cx, &mut cache, "two", &setup)
            .await
            .expect("new head");
        assert_eq!(prepares.load(Ordering::SeqCst), 2);
    }

    #[gpui::test]
    async fn edges_between_changed_functions(cx: &mut TestAppContext) {
        let setup = setup(cx, true).await;
        answer_calls(&setup);
        let context = fetch(cx, &mut ContextCache::default(), "head", &setup)
            .await
            .expect("context");
        let functions = vec![
            changed_function("caller", 17, 19, FunctionChange::BodyOnly),
            changed_function("changed", 0, 2, FunctionChange::BodyOnly),
            changed_function("helper", 4, 15, FunctionChange::Signature),
        ];
        let edges = cx.read(|cx| call_edges(&functions, &[None, Some(context), None], cx));
        assert_eq!(edges, vec![(1, 2), (1, 1)]);
    }

    #[gpui::test]
    async fn long_context_is_cut_to_eight_rows(cx: &mut TestAppContext) {
        let setup = setup(cx, true).await;
        answer_calls(&setup);
        let context = fetch(cx, &mut ContextCache::default(), "head", &setup)
            .await
            .expect("context");
        let function = changed_function("changed", 0, 2, FunctionChange::BodyOnly);
        let changed_rows = HashMap::from_iter([("src/main.rs".to_string(), vec![1..2])]);
        let excerpts = cx.read(|cx| context_excerpts(&function, &context, &changed_rows, cx));
        let summary: Vec<_> = excerpts
            .iter()
            .map(|excerpt| {
                (
                    excerpt.range.clone(),
                    excerpt.full_rows,
                    excerpt.label.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    Point::new(4, 0)..Point::new(11, 14),
                    12,
                    "unchanged · src/main.rs · helper() · called on line 2",
                ),
                (
                    Point::new(17, 0)..Point::new(19, 1),
                    3,
                    "unchanged · src/main.rs · caller() · calls changed() on line 19",
                ),
            ]
        );
        assert!(
            excerpts
                .iter()
                .all(|excerpt| excerpt.buffer == setup.buffer)
        );
    }

    #[test]
    fn classify_caller_marks() {
        use FunctionChange::*;
        assert_eq!(classify_caller(Some(Signature), true), CallerMark::InThisPr);
        assert_eq!(classify_caller(None, false), CallerMark::Unchanged);
        assert_eq!(classify_caller(Some(BodyOnly), false), CallerMark::Check);
        assert_eq!(classify_caller(Some(Added), false), CallerMark::Check);
        assert_eq!(
            classify_caller(Some(Signature), false),
            CallerMark::NotUpdated
        );
    }

    #[test]
    fn context_labels() {
        assert_eq!(
            context_label(Direction::Callee, "src/a b.rs", "helper", "changed", 7),
            "unchanged · src/a b.rs · helper() · called on line 7"
        );
        assert_eq!(
            context_label(Direction::Caller, "src/main.rs", "caller", "changed", 19),
            "unchanged · src/main.rs · caller() · calls changed() on line 19"
        );
    }
}
