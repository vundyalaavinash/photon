use std::collections::BTreeSet;
use std::ops::Range;

use buffer_diff::BufferDiffSnapshot;
use gpui::{App, AppContext as _, Task};
use language::{BufferSnapshot, OutlineItem, Point};

use crate::diff::LoadedFile;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionChange {
    Added,
    BodyOnly,
    Signature,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFunction {
    pub path: String,
    pub name: String,
    pub outline_path: Vec<String>,
    pub range: Range<Point>,
    pub selection_point: Point,
    pub change: FunctionChange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileAnalysis {
    Functions {
        functions: Vec<ChangedFunction>,
        file_level_rows: Vec<Range<u32>>,
    },
    Unanalysable,
}

/// Head rows of each hunk; a deletion-only hunk counts as the single row at the deletion point.
pub fn changed_rows(diff: &BufferDiffSnapshot, buffer: &BufferSnapshot) -> Vec<Range<u32>> {
    diff.hunks(&buffer.text)
        .map(|hunk| {
            let start = hunk.range.start.row;
            let end = if hunk.range.is_empty() {
                start + 1
            } else {
                hunk.range.end.row.max(start + 1)
            };
            start..end
        })
        .collect()
}

pub fn signature_text(snapshot: &BufferSnapshot, item: &OutlineItem<Point>) -> String {
    let end = item
        .body_range
        .as_ref()
        .map_or(item.range.end, |body| body.start);
    let text: String = snapshot.text_for_range(item.range.start..end).collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct NamedItem {
    item: OutlineItem<Point>,
    name: String,
    outline_path: Vec<String>,
}

// Names come from the outline's name captures, not its display text, so adding `pub` or `async`
// to a function is a signature change rather than a new function.
fn named_items(snapshot: &BufferSnapshot) -> Vec<NamedItem> {
    let items = snapshot.outline_items_as_points_containing(0..snapshot.len(), false, None);
    let mut ancestors: Vec<String> = Vec::new();
    items
        .into_iter()
        .map(|item| {
            let name = item_name(&item);
            ancestors.truncate(item.depth);
            ancestors.push(name.clone());
            NamedItem {
                item,
                name,
                outline_path: ancestors.clone(),
            }
        })
        .collect()
}

fn item_name(item: &OutlineItem<Point>) -> String {
    let names: Vec<&str> = item
        .name_ranges
        .iter()
        .filter_map(|range| item.text.get(range.clone()))
        .collect();
    if names.is_empty() {
        item.text.to_string()
    } else {
        names.join(" ")
    }
}

pub fn analyse_file(
    path: &str,
    buffer: &BufferSnapshot,
    base: &BufferSnapshot,
    changed_rows: &[Range<u32>],
) -> FileAnalysis {
    if buffer.language().is_none() {
        return FileAnalysis::Unanalysable;
    }
    let head_items = named_items(buffer);
    if head_items.is_empty() {
        return FileAnalysis::Unanalysable;
    }
    let base_items = named_items(base);

    let mut touched_items = BTreeSet::new();
    let mut loose_rows = BTreeSet::new();
    for row in changed_rows.iter().flat_map(|rows| rows.clone()) {
        let deepest = head_items
            .iter()
            .enumerate()
            .filter(|(_, named)| {
                named.item.range.start.row <= row && row <= named.item.range.end.row
            })
            .max_by_key(|(_, named)| named.item.depth);
        match deepest {
            Some((index, _)) => {
                touched_items.insert(index);
            }
            None => {
                loose_rows.insert(row);
            }
        }
    }

    let functions = touched_items
        .into_iter()
        .filter_map(|index| head_items.get(index))
        .map(|named| {
            let base_match = base_items
                .iter()
                .find(|candidate| candidate.outline_path == named.outline_path);
            let change = match base_match {
                None => FunctionChange::Added,
                Some(base_named)
                    if signature_text(base, &base_named.item)
                        != signature_text(buffer, &named.item) =>
                {
                    FunctionChange::Signature
                }
                Some(_) => FunctionChange::BodyOnly,
            };
            ChangedFunction {
                path: path.to_string(),
                name: named.name.clone(),
                outline_path: named.outline_path.clone(),
                range: named.item.range.clone(),
                selection_point: named.item.selection_range.start,
                change,
            }
        })
        .collect();

    let mut file_level_rows: Vec<Range<u32>> = Vec::new();
    for row in loose_rows {
        match file_level_rows.last_mut() {
            Some(last) if last.end == row => last.end = row + 1,
            _ => file_level_rows.push(row..row + 1),
        }
    }
    FileAnalysis::Functions {
        functions,
        file_level_rows,
    }
}

/// `None` for files without text contents (binary, submodule).
pub fn analyse_loaded_file(file: &LoadedFile, cx: &mut App) -> Task<Option<FileAnalysis>> {
    let Some((buffer, buffer_diff)) = &file.contents else {
        return Task::ready(None);
    };
    let path = file.diff.path.clone();
    let buffer = buffer.clone();
    let buffer_diff = buffer_diff.clone();
    cx.spawn(async move |cx| {
        let (head_idle, base_idle) = cx.update(|cx| {
            (
                buffer.read(cx).parsing_idle(),
                buffer_diff
                    .read(cx)
                    .base_text_buffer()
                    .read(cx)
                    .parsing_idle(),
            )
        });
        head_idle.await;
        base_idle.await;
        let (snapshot, diff_snapshot) = cx.update(|cx| {
            (
                buffer.read(cx).snapshot(),
                buffer_diff.read(cx).snapshot(cx),
            )
        });
        let analysis = cx
            .background_spawn(async move {
                let rows = changed_rows(&diff_snapshot, &snapshot);
                analyse_file(&path, &snapshot, diff_snapshot.base_text(), &rows)
            })
            .await;
        Some(analysis)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffer_diff::BufferDiff;
    use gpui::TestAppContext;
    use language::Buffer;

    const BASE: &str = "\
use std::fmt;

struct Foo;

impl Foo {
    fn bar(&self) -> u32 {
        1
    }

    fn baz(&self) {}
}

fn top(a: u32) -> u32 {
    a
}

fn gone() {}
";

    async fn analyse(cx: &mut TestAppContext, base: &str, head: &str, rust: bool) -> FileAnalysis {
        let language = rust.then(language::rust_lang);
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(head, cx);
            buffer.set_language(language.clone(), cx);
            buffer
        });
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
        let buffer_diff = cx.new(|cx| BufferDiff::new(&snapshot.text, language, None, cx));
        buffer_diff
            .update(cx, |buffer_diff, cx| {
                buffer_diff.set_base_text(Some(base.into()), snapshot.text.clone(), cx)
            })
            .await;
        cx.run_until_parked();
        let (snapshot, diff_snapshot) = cx.read(|cx| {
            (
                buffer.read(cx).snapshot(),
                buffer_diff.read(cx).snapshot(cx),
            )
        });
        let rows = changed_rows(&diff_snapshot, &snapshot);
        analyse_file("a.rs", &snapshot, diff_snapshot.base_text(), &rows)
    }

    fn functions(analysis: FileAnalysis) -> (Vec<(String, FunctionChange)>, Vec<Range<u32>>) {
        match analysis {
            FileAnalysis::Functions {
                functions,
                file_level_rows,
            } => (
                functions
                    .into_iter()
                    .map(|function| (function.name, function.change))
                    .collect(),
                file_level_rows,
            ),
            FileAnalysis::Unanalysable => panic!("expected functions"),
        }
    }

    #[gpui::test]
    async fn changed_body_is_body_only(cx: &mut TestAppContext) {
        let head = BASE.replace("    a\n", "    a + 1\n");
        let (found, rows) = functions(analyse(cx, BASE, &head, true).await);
        assert_eq!(found, vec![("top".into(), FunctionChange::BodyOnly)]);
        assert!(rows.is_empty());
    }

    #[gpui::test]
    async fn changed_params_is_signature(cx: &mut TestAppContext) {
        let head = BASE.replace("fn top(a: u32)", "fn top(a: u32, b: u32)");
        let (found, _) = functions(analyse(cx, BASE, &head, true).await);
        assert_eq!(found, vec![("top".into(), FunctionChange::Signature)]);
    }

    #[gpui::test]
    async fn changed_return_type_is_signature(cx: &mut TestAppContext) {
        let head = BASE.replace("fn top(a: u32) -> u32", "fn top(a: u32) -> u64");
        let (found, _) = functions(analyse(cx, BASE, &head, true).await);
        assert_eq!(found, vec![("top".into(), FunctionChange::Signature)]);
    }

    #[gpui::test]
    async fn new_function_is_added(cx: &mut TestAppContext) {
        let head = format!("{BASE}\nfn fresh() {{\n    1\n}}\n");
        let (found, _) = functions(analyse(cx, BASE, &head, true).await);
        assert_eq!(found, vec![("fresh".into(), FunctionChange::Added)]);
    }

    #[gpui::test]
    async fn nested_method_picks_innermost(cx: &mut TestAppContext) {
        let head = BASE.replace("        1\n", "        2\n");
        let FileAnalysis::Functions { functions, .. } = analyse(cx, BASE, &head, true).await else {
            panic!("expected functions");
        };
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name, "bar");
        assert_eq!(functions[0].outline_path, vec!["Foo", "bar"]);
        assert_eq!(functions[0].change, FunctionChange::BodyOnly);
        assert_eq!(functions[0].selection_point, Point::new(5, 7));
    }

    #[gpui::test]
    async fn change_outside_functions_is_file_level(cx: &mut TestAppContext) {
        let head = BASE.replace("use std::fmt;", "use std::io;");
        let (found, rows) = functions(analyse(cx, BASE, &head, true).await);
        assert!(found.is_empty());
        assert_eq!(rows, vec![0..1]);
    }

    #[gpui::test]
    async fn plain_text_file_is_unanalysable(cx: &mut TestAppContext) {
        let analysis = analyse(cx, "a\n", "b\n", false).await;
        assert_eq!(analysis, FileAnalysis::Unanalysable);
    }

    #[gpui::test]
    async fn signature_ignores_whitespace_and_body(cx: &mut TestAppContext) {
        let head = BASE.replace(
            "fn top(a: u32) -> u32 {",
            "fn top(\n    a: u32,\n) ->   u32 {",
        );
        let (found, _) = functions(analyse(cx, BASE, &head, true).await);
        assert_eq!(found, vec![("top".into(), FunctionChange::Signature)]);
        let head = BASE.replace("fn top(a: u32) -> u32 {", "fn top(a:   u32)  ->  u32 {");
        let (found, _) = functions(analyse(cx, BASE, &head, true).await);
        assert_eq!(found, vec![("top".into(), FunctionChange::BodyOnly)]);
    }

    #[gpui::test]
    async fn deletion_only_hunk_counts_the_deletion_row(cx: &mut TestAppContext) {
        let head = BASE
            .replace("    fn baz(&self) {}\n\n", "")
            .replace("        1\n", "");
        let (found, _) = functions(analyse(cx, BASE, &head, true).await);
        assert_eq!(found, vec![("bar".into(), FunctionChange::BodyOnly)]);
    }

    #[gpui::test]
    async fn deleted_function_is_not_listed(cx: &mut TestAppContext) {
        let head = BASE.replace("\nfn gone() {}\n", "");
        let (found, rows) = functions(analyse(cx, BASE, &head, true).await);
        assert!(found.is_empty());
        assert_eq!(rows.len(), 1);
    }
}
