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

const MODIFIER_WORDS: [&str; 17] = [
    "async",
    "const",
    "unsafe",
    "default",
    "static",
    "abstract",
    "final",
    "override",
    "virtual",
    "declare",
    "sealed",
    "synchronized",
    "native",
    "readonly",
    "open",
    "inline",
    "suspend",
];
const VISIBILITY_WORDS: [&str; 5] = ["public", "private", "protected", "internal", "export"];
const TYPE_KEYWORDS: [&str; 11] = [
    "class",
    "struct",
    "interface",
    "enum",
    "trait",
    "impl",
    "record",
    "type",
    "mod",
    "module",
    "namespace",
];

// A prefix match inside an identifier (`constant`, `public`) is not a keyword.
fn strip_keyword<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(keyword)?;
    if rest.starts_with(|character: char| character.is_alphanumeric() || character == '_') {
        return None;
    }
    Some(rest.trim_start())
}

fn strip_visibility(mut text: &str) -> &str {
    loop {
        text = text.trim_start();
        if let Some(rest) = strip_keyword(text, "pub") {
            text = match rest.starts_with('(') {
                true => rest
                    .split_once(')')
                    .map_or(rest, |(_, after)| after.trim_start()),
                false => rest,
            };
        } else if let Some(rest) = VISIBILITY_WORDS
            .iter()
            .find_map(|word| strip_keyword(text, word))
        {
            text = rest;
        } else {
            return text;
        }
    }
}

/// Modifiers stay part of the compared signature because they change how callers call the
/// function; they are skipped only to see what kind of item the header describes.
fn strip_modifiers(mut text: &str) -> &str {
    loop {
        text = text.trim_start();
        let next = MODIFIER_WORDS
            .iter()
            .find_map(|word| strip_keyword(text, word))
            .or_else(|| {
                let rest = strip_keyword(text, "extern")?;
                match rest
                    .strip_prefix('"')
                    .and_then(|quoted| quoted.split_once('"'))
                {
                    Some((_, after)) => Some(after.trim_start()),
                    None => Some(rest),
                }
            });
        match next {
            Some(rest) => text = rest,
            None => return text,
        }
    }
}

/// Outlines of most languages give no body range, so the header is cut at the first top-level
/// `;`, at the `:` that ends a Python `def`, right after an arrow function's `=>`, or at a `{`
/// that opens a body: one at top level not following a `:` return-type marker, or one directly
/// after `)` at any depth, as in a `function () {` callback passed to a call.
fn header_of(text: &str) -> &str {
    let python_def = strip_keyword(strip_modifiers(strip_visibility(text)), "def").is_some();
    // One entry per open bracket, true once a `:` annotation starts inside it: a `=>` there
    // belongs to a function type such as `(cb: (x: number) => void)`, not to an arrow function.
    let mut annotations: Vec<bool> = Vec::new();
    let mut previous = ' ';
    for (index, character) in text.char_indices() {
        let in_annotation = annotations.last().copied().unwrap_or(false);
        match character {
            '{' if previous == ')' || (annotations.is_empty() && previous != ':') => {
                return &text[..index];
            }
            '(' | '[' | '<' | '{' => annotations.push(in_annotation),
            ')' | ']' | '}' => {
                annotations.pop();
            }
            '>' if previous == '=' && !in_annotation => return &text[..index + 1],
            '>' if previous != '-' && previous != '=' => {
                annotations.pop();
            }
            ';' if annotations.is_empty() => return &text[..index],
            ':' if python_def && annotations.is_empty() => return &text[..index],
            ':' | ',' => {
                let outer = annotations
                    .len()
                    .checked_sub(2)
                    .and_then(|outer| annotations.get(outer))
                    .copied()
                    .unwrap_or(false);
                if let Some(current) = annotations.last_mut() {
                    *current = character == ':' || outer;
                }
            }
            _ => {}
        }
        if !character.is_whitespace() {
            previous = character;
        }
    }
    text
}

// `type(text) {` is a method named `type`; a type keyword is followed by a name, or by `<` as in
// Rust's `impl<T>`.
fn starts_with_type_keyword(kind: &str) -> bool {
    TYPE_KEYWORDS
        .iter()
        .chain(["object"].iter())
        .any(|keyword| {
            kind.strip_prefix(keyword).is_some_and(|rest| {
                rest.starts_with('<')
                    || (rest.starts_with(char::is_whitespace)
                        && rest.trim_start().starts_with(|character: char| {
                            character.is_alphabetic() || character == '_' || character == '$'
                        }))
            })
        })
}

fn assigns_at_top_level(header: &str) -> bool {
    let characters: Vec<char> = header.chars().collect();
    let mut nesting = 0usize;
    for (index, character) in characters.iter().enumerate() {
        let previous = index.checked_sub(1).and_then(|index| characters.get(index));
        let next = characters.get(index + 1);
        match character {
            '(' | '[' | '<' => nesting += 1,
            ')' | ']' => nesting = nesting.saturating_sub(1),
            '>' if !matches!(previous, Some('-' | '=')) => nesting = nesting.saturating_sub(1),
            '=' if nesting == 0
                && !matches!(next, Some('>' | '='))
                && !matches!(previous, Some('<' | '>' | '!' | '=')) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn is_callable(header: &str) -> bool {
    let kind = strip_modifiers(header);
    if starts_with_type_keyword(kind) || !kind.contains('(') {
        return false;
    }
    // `const LIMIT: Duration = Duration::from_secs(1)` is a value, `const run = () => {}` is not.
    !assigns_at_top_level(kind) || kind.contains("=>") || kind.contains("function")
}

/// Signature with visibility, whitespace and trailing commas removed, or `None` when the item
/// is not callable.
fn callable_signature(snapshot: &BufferSnapshot, item: &OutlineItem<Point>) -> Option<String> {
    let collapsed = signature_text(snapshot, item);
    let stripped = header_of(strip_visibility(&collapsed)).trim_end();
    if !is_callable(stripped) {
        return None;
    }
    let compact: Vec<char> = stripped
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let mut normalised = String::with_capacity(compact.len());
    for (index, character) in compact.iter().enumerate() {
        let before_closer = matches!(compact.get(index + 1), Some(')' | '>' | ']'));
        if *character != ',' || !before_closer {
            normalised.push(*character);
        }
    }
    Some(normalised)
}

struct NamedItem {
    signature: Option<String>,
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
                signature: callable_signature(snapshot, &item),
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
                named.signature.is_some()
                    && named.item.range.start.row <= row
                    && row <= named.item.range.end.row
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
            let base_match = base_items.iter().find(|candidate| {
                candidate.signature.is_some() && candidate.outline_path == named.outline_path
            });
            let change = match base_match {
                None => FunctionChange::Added,
                Some(base_named) if base_named.signature != named.signature => {
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
        let (language, registry, file_path) = cx.update(|cx| {
            let buffer = buffer.read(cx);
            (
                buffer.language().cloned(),
                buffer.language_registry(),
                buffer.file().map(|file| file.full_path(cx)),
            )
        });
        let language = match (language, registry, file_path) {
            (Some(language), ..) => Some(language),
            (None, Some(registry), Some(path)) => {
                registry.load_language_for_file_path(&path).await.ok()
            }
            _ => None,
        };
        if let Some(language) = language {
            buffer.update(cx, |buffer, cx| {
                if buffer.language().is_none() {
                    buffer.set_language(Some(language.clone()), cx);
                }
            });
            let base_buffer = buffer_diff.read_with(cx, |diff, _| diff.base_text_buffer().clone());
            base_buffer.update(cx, |base, cx| {
                if base.language().is_none() {
                    base.set_language(Some(language), cx);
                }
            });
        }
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
    use crate::diff::{FileDiff, FileStatus, build_loaded_file};
    use buffer_diff::BufferDiff;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use language::{Buffer, Language};
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use std::sync::Arc;

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
        analyse_with(cx, base, head, rust.then(language::rust_lang)).await
    }

    async fn analyse_with(
        cx: &mut TestAppContext,
        base: &str,
        head: &str,
        language: Option<Arc<Language>>,
    ) -> FileAnalysis {
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
        assert_eq!(found, vec![("top".into(), FunctionChange::BodyOnly)]);
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

    const WIDE: &str = "\
fn wide(first: u32, second: Vec<u32>) -> u32 {
    first
}
";

    #[gpui::test]
    async fn rustfmt_rewrap_is_not_a_signature_change(cx: &mut TestAppContext) {
        let head = "\
fn wide(
    first: u32,
    second: Vec<
        u32,
    >,
) -> u32 {
    first + 1
}
";
        let (found, _) = functions(analyse(cx, WIDE, head, true).await);
        assert_eq!(found, vec![("wide".into(), FunctionChange::BodyOnly)]);
    }

    #[gpui::test]
    async fn parameter_type_change_is_signature(cx: &mut TestAppContext) {
        let head = WIDE.replace("Vec<u32>", "Vec<u64>");
        let (found, _) = functions(analyse(cx, WIDE, &head, true).await);
        assert_eq!(found, vec![("wide".into(), FunctionChange::Signature)]);
    }

    #[gpui::test]
    async fn visibility_change_alone_is_not_a_signature_change(cx: &mut TestAppContext) {
        let head = BASE.replace("fn top(", "pub(crate) fn top(");
        let (found, _) = functions(analyse(cx, BASE, &head, true).await);
        assert_eq!(found, vec![("top".into(), FunctionChange::BodyOnly)]);
    }

    #[gpui::test]
    async fn adding_async_is_a_signature_change(cx: &mut TestAppContext) {
        let head = BASE.replace("fn top(", "async fn top(");
        let (found, _) = functions(analyse(cx, BASE, &head, true).await);
        assert_eq!(found, vec![("top".into(), FunctionChange::Signature)]);
    }

    const WITH_CONST: &str = "\
struct Foo;

impl Foo {
    const LIMIT: u32 = 1;

    fn bar(&self) {}

    fn baz(&self) {}
}

trait Speak {
    fn speak(&self, loud: bool);
}
";

    #[gpui::test]
    async fn associated_const_edit_is_file_level(cx: &mut TestAppContext) {
        let head = WITH_CONST.replace("LIMIT: u32 = 1", "LIMIT: u32 = 2");
        let (found, rows) = functions(analyse(cx, WITH_CONST, &head, true).await);
        assert_eq!(found, vec![]);
        assert_eq!(rows, vec![3..4]);
    }

    #[gpui::test]
    async fn deleting_a_method_does_not_list_the_impl(cx: &mut TestAppContext) {
        let head = WITH_CONST.replace("    fn baz(&self) {}\n", "");
        let (found, _) = functions(analyse(cx, WITH_CONST, &head, true).await);
        assert_eq!(found, vec![]);
    }

    #[gpui::test]
    async fn trait_method_signature_change_is_signature(cx: &mut TestAppContext) {
        let head = WITH_CONST.replace("loud: bool", "loud: u8");
        let (found, _) = functions(analyse(cx, WITH_CONST, &head, true).await);
        assert_eq!(found, vec![("speak".into(), FunctionChange::Signature)]);
    }

    #[gpui::test]
    async fn struct_field_edit_is_file_level(cx: &mut TestAppContext) {
        let base = "struct Foo {\n    x: u32,\n}\n\nconst LIMIT: u32 = 1;\n";
        let head = "struct Foo {\n    x: u64,\n}\n\nconst LIMIT: u32 = 2;\n";
        let (found, rows) = functions(analyse(cx, base, head, true).await);
        assert_eq!(found, vec![]);
        assert_eq!(rows, vec![1..2, 4..5]);
    }

    #[gpui::test]
    async fn new_struct_beside_impl_does_not_disturb_methods(cx: &mut TestAppContext) {
        let base = BASE.replace("struct Foo;\n\n", "");
        let head = BASE.replace("        1\n", "        2\n");
        let (found, rows) = functions(analyse(cx, &base, &head, true).await);
        assert_eq!(found, vec![("bar".into(), FunctionChange::BodyOnly)]);
        assert_eq!(rows, vec![2..4]);
    }

    #[gpui::test]
    async fn loaded_rust_file_is_analysed_with_its_language(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
        });
        let fs = FakeFs::new(cx.executor());
        let head = BASE.replace("    a\n", "    a + 1\n");
        fs.insert_tree("/repo", json!({ "a.rs": head })).await;
        let project = Project::test(fs, ["/repo".as_ref()], cx).await;
        project.read_with(cx, |project, _| {
            project.languages().add(language::rust_lang());
        });
        let diff = FileDiff {
            path: "a.rs".into(),
            status: FileStatus::Modified,
            binary: false,
            submodule: false,
            symlink: false,
            hunks: Vec::new(),
        };
        let loaded = cx
            .update(|cx| {
                build_loaded_file(&project, "/repo/a.rs".into(), Some(BASE.into()), diff, cx)
            })
            .await
            .expect("loaded");
        let analysis = cx
            .update(|cx| analyse_loaded_file(&loaded, cx))
            .await
            .expect("analysis");
        let (found, _) = functions(analysis);
        assert_eq!(found, vec![("top".into(), FunctionChange::BodyOnly)]);
    }

    fn python_lang() -> Arc<Language> {
        Arc::new(
            Language::new(
                grammars::load_config("python"),
                Some(tree_sitter_python::LANGUAGE.into()),
            )
            .with_queries(grammars::load_queries("python"))
            .expect("python queries"),
        )
    }

    fn typescript_lang() -> Arc<Language> {
        Arc::new(
            Language::new(
                grammars::load_config("typescript"),
                Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            )
            .with_queries(grammars::load_queries("typescript"))
            .expect("typescript queries"),
        )
    }

    const PYTHON: &str = "\
import os

class Greeter:
    greeting = 'hi'

    def greet(self, name: str) -> str:
        return self.greeting + name

def top(a, b):
    return a
";

    #[gpui::test]
    async fn python_body_and_signature_changes(cx: &mut TestAppContext) {
        let head = PYTHON.replace("return a\n", "return a + b\n");
        let (found, rows) = functions(analyse_with(cx, PYTHON, &head, Some(python_lang())).await);
        assert_eq!(found, vec![("top".into(), FunctionChange::BodyOnly)]);
        assert!(rows.is_empty());

        let head = PYTHON.replace("def top(a, b)", "def top(a, b, c)");
        let (found, _) = functions(analyse_with(cx, PYTHON, &head, Some(python_lang())).await);
        assert_eq!(found, vec![("top".into(), FunctionChange::Signature)]);
    }

    #[gpui::test]
    async fn python_method_is_the_method_not_the_class(cx: &mut TestAppContext) {
        let head = PYTHON.replace("self.greeting + name", "name");
        let (found, _) = functions(analyse_with(cx, PYTHON, &head, Some(python_lang())).await);
        assert_eq!(found, vec![("greet".into(), FunctionChange::BodyOnly)]);

        let head = PYTHON.replace("name: str", "name: bytes");
        let (found, _) = functions(analyse_with(cx, PYTHON, &head, Some(python_lang())).await);
        assert_eq!(found, vec![("greet".into(), FunctionChange::Signature)]);
    }

    #[gpui::test]
    async fn python_class_attribute_edit_is_file_level(cx: &mut TestAppContext) {
        let head = PYTHON.replace("'hi'", "'hello'");
        let (found, rows) = functions(analyse_with(cx, PYTHON, &head, Some(python_lang())).await);
        assert_eq!(found, vec![]);
        assert_eq!(rows, vec![3..4]);
    }

    const TYPESCRIPT: &str = "\
import { x } from './x';

class Greeter {
    greeting: string = 'hi';

    greet(name: string): string {
        return this.greeting + name;
    }
}

function top(a: number, b: number): number {
    return a;
}
";

    #[gpui::test]
    async fn typescript_body_and_signature_changes(cx: &mut TestAppContext) {
        let head = TYPESCRIPT.replace("return a;", "return a + b;");
        let (found, _) =
            functions(analyse_with(cx, TYPESCRIPT, &head, Some(typescript_lang())).await);
        assert_eq!(found, vec![("top".into(), FunctionChange::BodyOnly)]);

        let head = TYPESCRIPT.replace("b: number)", "b: string)");
        let (found, _) =
            functions(analyse_with(cx, TYPESCRIPT, &head, Some(typescript_lang())).await);
        assert_eq!(found, vec![("top".into(), FunctionChange::Signature)]);
    }

    #[gpui::test]
    async fn typescript_method_is_the_method_not_the_class(cx: &mut TestAppContext) {
        let head = TYPESCRIPT.replace("this.greeting + name", "name");
        let (found, _) =
            functions(analyse_with(cx, TYPESCRIPT, &head, Some(typescript_lang())).await);
        assert_eq!(found, vec![("greet".into(), FunctionChange::BodyOnly)]);
    }

    #[gpui::test]
    async fn typescript_property_edit_is_file_level(cx: &mut TestAppContext) {
        let head = TYPESCRIPT.replace("'hi'", "'hello'");
        let (found, rows) =
            functions(analyse_with(cx, TYPESCRIPT, &head, Some(typescript_lang())).await);
        assert_eq!(found, vec![]);
        assert_eq!(rows, vec![3..4]);
    }

    const CALLBACKS: &str = "\
import { x } from './x';

describe('suite', () => {
    it('adds', () => {
        expect(1).toBe(1);
    });
});

const handler = useCallback((e) => {
    console.log(e);
}, []);

const f = (a) =>
    compute(a);
";

    async fn analyse_typescript(
        cx: &mut TestAppContext,
        base: &str,
        head: &str,
    ) -> Vec<(String, FunctionChange)> {
        functions(analyse_with(cx, base, head, Some(typescript_lang())).await).0
    }

    #[gpui::test]
    async fn typescript_test_block_body_edit_is_body_only(cx: &mut TestAppContext) {
        let head = CALLBACKS.replace("toBe(1)", "toBe(2)");
        let found = analyse_typescript(cx, CALLBACKS, &head).await;
        assert_eq!(found, vec![("adds".into(), FunctionChange::BodyOnly)]);
    }

    #[gpui::test]
    async fn typescript_use_callback_body_edit_is_body_only(cx: &mut TestAppContext) {
        let head = CALLBACKS.replace("console.log(e)", "console.info(e)");
        let found = analyse_typescript(cx, CALLBACKS, &head).await;
        assert_eq!(found, vec![("handler".into(), FunctionChange::BodyOnly)]);
    }

    #[gpui::test]
    async fn typescript_expression_arrow_body_edit_is_body_only(cx: &mut TestAppContext) {
        let head = CALLBACKS.replace("compute(a);", "compute(a) + 1;");
        let found = analyse_typescript(cx, CALLBACKS, &head).await;
        assert_eq!(found, vec![("f".into(), FunctionChange::BodyOnly)]);
    }

    #[gpui::test]
    async fn typescript_arrow_parameter_change_is_signature(cx: &mut TestAppContext) {
        let head = CALLBACKS.replace("f = (a) =>", "f = (a, b) =>");
        let found = analyse_typescript(cx, CALLBACKS, &head).await;
        assert_eq!(found, vec![("f".into(), FunctionChange::Signature)]);
    }

    #[gpui::test]
    async fn methods_named_like_type_keywords_are_callable(cx: &mut TestAppContext) {
        let base = "\
class Recorder {
    type(text: string) {
        return text;
    }

    record(event: string) {
        return event;
    }
}
";
        let head = base.replace("return text;", "return text + 1;");
        let found = analyse_typescript(cx, base, &head).await;
        assert_eq!(found, vec![("type".into(), FunctionChange::BodyOnly)]);

        let head = base.replace("return event;", "return event + 1;");
        let found = analyse_typescript(cx, base, &head).await;
        assert_eq!(found, vec![("record".into(), FunctionChange::BodyOnly)]);
    }

    #[gpui::test]
    async fn typescript_object_return_type_is_part_of_the_signature(cx: &mut TestAppContext) {
        let base = "\
function shape(): { a: number } {
    return { a: 1 };
}
";
        let head = base.replace("a: number", "a: string");
        let found = analyse_typescript(cx, base, &head).await;
        assert_eq!(found, vec![("shape".into(), FunctionChange::Signature)]);

        let head = base.replace("a: 1", "a: 2");
        let found = analyse_typescript(cx, base, &head).await;
        assert_eq!(found, vec![("shape".into(), FunctionChange::BodyOnly)]);
    }
}
