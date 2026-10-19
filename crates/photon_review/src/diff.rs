use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use buffer_diff::BufferDiff;
use gpui::{App, AppContext as _, AsyncApp, Entity, Task};
use language::Buffer;
use project::Project;

use crate::checkout::git_raw;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed { from: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub status: FileStatus,
    pub binary: bool,
    pub submodule: bool,
    pub symlink: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// Binary files, submodules and symlinks have no text Photon shows or opens. A symlink is
    /// never followed: a pull request can point one at any file on the reviewer's machine.
    pub fn is_opaque(&self) -> bool {
        self.binary || self.submodule || self.symlink
    }
}

pub struct LoadedFile {
    pub diff: FileDiff,
    pub contents: Option<(Entity<Buffer>, Entity<BufferDiff>)>,
}

pub fn parse_unified_diff(patch: &str) -> Result<Vec<FileDiff>> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut skipping = false;
    for line in patch.split_inclusive('\n') {
        let text = line.strip_suffix('\n').unwrap_or(line);
        if let Some(header) = text.strip_prefix("diff --git ") {
            let Some((_, new_path)) = split_header_paths(header) else {
                log::warn!("skipping file with malformed diff header {text:?}");
                skipping = true;
                continue;
            };
            skipping = false;
            files.push(FileDiff {
                path: new_path,
                status: FileStatus::Modified,
                binary: false,
                submodule: false,
                symlink: false,
                hunks: Vec::new(),
            });
            continue;
        }
        if skipping {
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if let Some(header) = text.strip_prefix("@@ ") {
            file.hunks.push(parse_hunk_header(header)?);
        } else if let Some(hunk) = file.hunks.last_mut() {
            hunk.body.push_str(line);
        } else if text.starts_with("new file mode") {
            file.status = FileStatus::Added;
            file.mark_mode(text);
        } else if text.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
            file.mark_mode(text);
        } else if text.starts_with("index ")
            || text.starts_with("old mode ")
            || text.starts_with("new mode ")
        {
            file.mark_mode(text);
        } else if let Some(from) = text.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed {
                from: unquote_git_path(from)?,
            };
        } else if let Some(to) = text.strip_prefix("rename to ") {
            file.path = unquote_git_path(to)?;
        } else if text.starts_with("Binary files ") || text.starts_with("GIT binary patch") {
            file.binary = true;
        }
    }
    Ok(files)
}

fn parse_hunk_header(header: &str) -> Result<Hunk> {
    let malformed = || anyhow!("malformed hunk header {header:?}");
    let ranges = header.split(" @@").next().ok_or_else(malformed)?;
    let (old, new) = ranges.split_once(' ').ok_or_else(malformed)?;
    let parse_range = |range: &str, sign: char| -> Result<(u32, u32)> {
        let range = range.strip_prefix(sign).ok_or_else(malformed)?;
        match range.split_once(',') {
            Some((start, len)) => Ok((start.parse()?, len.parse()?)),
            None => Ok((range.parse()?, 1)),
        }
    };
    let (old_start, old_len) = parse_range(old, '-')?;
    let (new_start, new_len) = parse_range(new, '+')?;
    Ok(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        body: String::new(),
    })
}

/// Splits the `a/<old> b/<new>` part of a `diff --git` line. Unquoted paths may contain spaces,
/// so the unquoted case relies on both sides being equal, which holds for every non-rename;
/// renames get their real paths from the `rename from/to` lines.
fn split_header_paths(header: &str) -> Option<(String, String)> {
    let strip = |path: String, prefix: &str| path.strip_prefix(prefix).map(str::to_string);
    if header.starts_with('"') {
        let (old, rest) = take_quoted(header)?;
        let rest = rest.strip_prefix(' ')?;
        let new = if rest.starts_with('"') {
            unquote_git_path(rest).ok()?
        } else {
            rest.to_string()
        };
        return Some((strip(old, "a/")?, strip(new, "b/")?));
    }
    if let Some(index) = header.find(" \"b/") {
        let new = unquote_git_path(&header[index + 1..]).ok()?;
        return Some((strip(header[..index].to_string(), "a/")?, strip(new, "b/")?));
    }
    let rest = header.strip_prefix("a/")?;
    let length = rest.len().saturating_sub(3) / 2;
    if let (Some(old), Some(new)) = (
        rest.get(..length),
        rest.get(length..).and_then(|new| new.strip_prefix(" b/")),
    ) && old == new
    {
        return Some((old.to_string(), new.to_string()));
    }
    // A rename with unquoted names; the `rename from/to` lines override both paths.
    let index = rest.rfind(" b/")?;
    Some((rest[..index].to_string(), rest[index + 3..].to_string()))
}

fn take_quoted(text: &str) -> Option<(String, &str)> {
    let mut escaped = false;
    for (index, character) in text.char_indices().skip(1) {
        match character {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => {
                let path = unquote_git_path(&text[..=index]).ok()?;
                return Some((path, &text[index + 1..]));
            }
            _ => {}
        }
    }
    None
}

/// Undoes git's C-style quoting; unquoted input is returned unchanged.
fn unquote_git_path(path: &str) -> Result<String> {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
    else {
        return Ok(path.to_string());
    };
    let bytes = inner.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        index += 1;
        if byte != b'\\' {
            output.push(byte);
            continue;
        }
        let escape = *bytes
            .get(index)
            .ok_or_else(|| anyhow!("dangling escape in {path:?}"))?;
        index += 1;
        output.push(match escape {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            b'0'..=b'3' => {
                let digits = bytes
                    .get(index - 1..index + 2)
                    .and_then(|digits| std::str::from_utf8(digits).ok())
                    .and_then(|digits| u8::from_str_radix(digits, 8).ok())
                    .ok_or_else(|| anyhow!("bad octal escape in {path:?}"))?;
                index += 2;
                digits
            }
            other => other,
        });
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

pub async fn git_diff(worktree: &Path, from: &str, to: &str) -> Result<Vec<FileDiff>> {
    // `diff` and `show` read only local objects: no hooks or credentials apply, so the plain
    // helper is enough here.
    for reference in [from, to] {
        if reference.starts_with('-') {
            bail!("invalid ref {reference:?}");
        }
    }
    // The explicit flags override user and system diff config, so hunks and statuses match
    // GitHub's diff and `commentable_lines` lines up with it.
    let patch = git_raw(
        worktree,
        &[
            "-c",
            "core.quotepath=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--diff-algorithm=myers",
            "--inter-hunk-context=0",
            "--ignore-submodules=none",
            "--submodule=short",
            "--find-renames",
            "-U3",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            from,
            to,
            "--",
        ],
    )
    .await?;
    parse_unified_diff(&patch)
}

const GITLINK_MODE: &str = " 160000";
const SYMLINK_MODE: &str = " 120000";

impl FileDiff {
    fn mark_mode(&mut self, line: &str) {
        self.submodule |= line.ends_with(GITLINK_MODE);
        self.symlink |= line.ends_with(SYMLINK_MODE);
    }
}

pub fn commentable_lines(file: &FileDiff) -> Vec<RangeInclusive<u32>> {
    if file.is_opaque() {
        return Vec::new();
    }
    file.hunks
        .iter()
        .filter(|hunk| hunk.new_len > 0)
        .map(|hunk| hunk.new_start..=hunk.new_start + hunk.new_len - 1)
        .collect()
}

/// Base lines (1-based) that the diff removes, which are the only lines a `Side::Left` comment
/// can sit on.
pub fn deleted_lines(file: &FileDiff) -> Vec<RangeInclusive<u32>> {
    if file.is_opaque() {
        return Vec::new();
    }
    let mut ranges: Vec<RangeInclusive<u32>> = Vec::new();
    for hunk in &file.hunks {
        let mut old_line = hunk.old_start;
        for line in hunk.body.lines() {
            match line.as_bytes().first() {
                Some(b'-') => {
                    match ranges.last_mut() {
                        Some(last) if *last.end() + 1 == old_line => {
                            *last = *last.start()..=old_line;
                        }
                        _ => ranges.push(old_line..=old_line),
                    }
                    old_line += 1;
                }
                Some(b' ') => old_line += 1,
                _ => {}
            }
        }
    }
    ranges
}

/// `None` for added and opaque entries, which have no text base to show.
pub async fn base_text(worktree: &Path, commit: &str, file: &FileDiff) -> Result<Option<String>> {
    if file.is_opaque() {
        return Ok(None);
    }
    let old_path = match &file.status {
        FileStatus::Added => return Ok(None),
        FileStatus::Renamed { from } => from,
        FileStatus::Modified | FileStatus::Deleted => &file.path,
    };
    let object = format!("{commit}:{old_path}");
    Ok(Some(git_raw(worktree, &["show", &object]).await?))
}

const RELOAD_ATTEMPTS: usize = 3;

/// A newer reload of the buffer (the file watcher's, after the checkout wrote the file) replaces
/// ours and cancels its receiver, so ours is retried until it lands. A dirty buffer keeps its
/// edits, as with Zed's own watcher.
async fn reload_from_disk(buffer: &Entity<Buffer>, cx: &mut AsyncApp) {
    for _ in 0..RELOAD_ATTEMPTS {
        if buffer.read_with(cx, |buffer, _| buffer.is_dirty()) {
            return;
        }
        if buffer
            .update(cx, |buffer, cx| buffer.reload(cx))
            .await
            .is_ok()
        {
            return;
        }
        log::debug!("buffer reload was superseded, retrying");
    }
    log::warn!("buffer reload kept being superseded; showing its current text");
}

pub fn build_loaded_file(
    project: &Entity<Project>,
    abs_path: PathBuf,
    base: Option<String>,
    diff: FileDiff,
    cx: &mut App,
) -> Task<Result<LoadedFile>> {
    if diff.is_opaque() {
        return Task::ready(Ok(LoadedFile {
            diff,
            contents: None,
        }));
    }
    let registry = project.read(cx).languages().clone();
    let fs = project.read(cx).fs().clone();
    let buffer_task = if diff.status == FileStatus::Deleted {
        let registry = registry.clone();
        cx.spawn(async move |cx| {
            let language = registry.load_language_for_file_path(&abs_path).await.ok();
            Ok(Some(cx.new(|cx| {
                let mut buffer = Buffer::local("", cx);
                buffer.set_language_registry(registry);
                buffer.set_language(language, cx);
                buffer
            })))
        })
    } else {
        let project = project.clone();
        cx.spawn(async move |cx| {
            // The diff's modes come from the PR; what's on disk is checked again before opening.
            let metadata = fs.metadata(&abs_path).await?;
            if metadata
                .is_some_and(|metadata| metadata.is_symlink || metadata.is_dir || metadata.is_fifo)
            {
                return Ok(None);
            }
            project
                .update(cx, |project, cx| project.open_local_buffer(abs_path, cx))
                .await
                .map(Some)
        })
    };
    cx.spawn(async move |cx| {
        let Some(buffer) = buffer_task.await? else {
            log::warn!("not opening {:?}: it isn't a regular file", diff.path);
            return Ok(LoadedFile {
                diff,
                contents: None,
            });
        };
        // The project may still hold this buffer from a review of an older head; read the text
        // the checkout just wrote rather than waiting for the file watcher.
        if diff.status != FileStatus::Deleted {
            reload_from_disk(&buffer, cx).await;
        }
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
        let buffer_diff = cx.new(|cx| {
            BufferDiff::new(
                &snapshot.text,
                snapshot.language().cloned(),
                Some(registry),
                cx,
            )
        });
        buffer_diff
            .update(cx, |buffer_diff, cx| {
                buffer_diff.set_base_text(base.map(Into::into), snapshot.text.clone(), cx)
            })
            .await;
        Ok(LoadedFile {
            diff,
            contents: Some((buffer, buffer_diff)),
        })
    })
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "synchronous fixture setup")]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use std::process::Command;
    use std::sync::Arc;

    fn file(path: &str, status: FileStatus, binary: bool) -> FileDiff {
        FileDiff {
            path: path.into(),
            status,
            binary,
            submodule: false,
            symlink: false,
            hunks: Vec::new(),
        }
    }

    const PATCH: &str = "\
diff --git a/src/mod.rs b/src/mod.rs
index 111..222 100644
--- a/src/mod.rs
+++ b/src/mod.rs
@@ -1,3 +1,3 @@
 a
-b
+B
 c
@@ -20 +20,2 @@ fn x() {
 z
+extra
\\ No newline at end of file
diff --git a/new.rs b/new.rs
new file mode 100644
index 000..333
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,2 @@
+one
+two
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
index 444..000
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-one
-two
diff --git a/old.rs b/renamed.rs
similarity index 90%
rename from old.rs
rename to renamed.rs
index 555..666 100644
--- a/old.rs
+++ b/renamed.rs
@@ -1 +1 @@
-x
+y
diff --git a/same.rs b/moved.rs
similarity index 100%
rename from same.rs
rename to moved.rs
diff --git a/logo.png b/logo.png
new file mode 100644
index 000..777
Binary files /dev/null and b/logo.png differ
";

    #[test]
    fn parses_modified_added_deleted_renamed_binary() {
        let files = parse_unified_diff(PATCH).expect("parse");
        let summary: Vec<_> = files
            .iter()
            .map(|file| {
                (
                    file.path.as_str(),
                    file.status.clone(),
                    file.binary,
                    file.hunks.len(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("src/mod.rs", FileStatus::Modified, false, 2),
                ("new.rs", FileStatus::Added, false, 1),
                ("gone.rs", FileStatus::Deleted, false, 1),
                (
                    "renamed.rs",
                    FileStatus::Renamed {
                        from: "old.rs".into()
                    },
                    false,
                    1
                ),
                (
                    "moved.rs",
                    FileStatus::Renamed {
                        from: "same.rs".into()
                    },
                    false,
                    0
                ),
                ("logo.png", FileStatus::Added, true, 0),
            ]
        );
        let hunks = &files[0].hunks;
        assert_eq!(
            (
                hunks[0].old_start,
                hunks[0].old_len,
                hunks[0].new_start,
                hunks[0].new_len
            ),
            (1, 3, 1, 3)
        );
        assert_eq!(hunks[0].body, " a\n-b\n+B\n c\n");
        assert_eq!(
            (
                hunks[1].old_start,
                hunks[1].old_len,
                hunks[1].new_start,
                hunks[1].new_len
            ),
            (20, 1, 20, 2)
        );
        assert!(hunks[1].body.ends_with("\\ No newline at end of file\n"));
        assert_eq!(files[1].hunks[0].old_len, 0);
    }

    #[test]
    fn unquotes_git_paths() {
        let patch = "\
diff --git \"a/a\\tb.txt\" \"b/a\\tb.txt\"
new file mode 100644
Binary files /dev/null and \"b/a\\tb.txt\" differ
diff --git \"a/caf\\303\\251.rs\" \"b/caf\\303\\251.rs\"
new file mode 100644
Binary files /dev/null and \"b/caf\\303\\251.rs\" differ
diff --git a/dir/a b.txt b/dir/a b.txt
index 1..2 100644
--- a/dir/a b.txt\t
+++ b/dir/a b.txt\t
@@ -1 +1 @@
-x
+y
diff --git \"a/old \\\"q\\\".rs\" b/new name.rs
similarity index 100%
rename from \"old \\\"q\\\".rs\"
rename to new name.rs
";
        let paths: Vec<_> = parse_unified_diff(patch)
            .expect("parse")
            .into_iter()
            .map(|file| (file.path, file.status))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("a\tb.txt".to_string(), FileStatus::Added),
                ("café.rs".to_string(), FileStatus::Added),
                ("dir/a b.txt".to_string(), FileStatus::Modified),
                (
                    "new name.rs".to_string(),
                    FileStatus::Renamed {
                        from: "old \"q\".rs".into()
                    }
                ),
            ]
        );
    }

    #[test]
    fn deleted_lines_follow_minus_rows() {
        let mut diff = file("a.rs", FileStatus::Modified, false);
        diff.hunks = vec![
            Hunk {
                old_start: 3,
                old_len: 5,
                new_start: 3,
                new_len: 3,
                body: " a\n-b\n-c\n+C\n d\n-e\n f\n\\ No newline at end of file\n".into(),
            },
            Hunk {
                old_start: 20,
                old_len: 1,
                new_start: 18,
                new_len: 0,
                body: "-z\n".into(),
            },
        ];
        assert_eq!(deleted_lines(&diff), vec![4..=5, 7..=7, 20..=20]);
        diff.submodule = true;
        assert!(deleted_lines(&diff).is_empty());
    }

    #[test]
    fn commentable_lines_include_context() {
        let hunk = |new_start, new_len| Hunk {
            old_start: 1,
            old_len: 1,
            new_start,
            new_len,
            body: String::new(),
        };
        let mut diff = file("a.rs", FileStatus::Modified, false);
        diff.hunks = vec![hunk(10, 7), hunk(30, 2), hunk(50, 0)];
        assert_eq!(commentable_lines(&diff), vec![10..=16, 30..=31]);
    }

    #[test]
    fn malformed_header_skips_only_that_file() {
        let patch = "\
diff --git garbage
index 1..2 100644
@@ -1 +1 @@
-x
+y
diff --git a/ok.rs b/ok.rs
index 1..2 100644
@@ -1 +1 @@
-a
+b
";
        let files = parse_unified_diff(patch).expect("parse");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "ok.rs");
        assert_eq!(files[0].hunks.len(), 1);
    }

    #[gpui::test]
    async fn submodule_entries_have_no_lines_or_base(_cx: &mut TestAppContext) {
        let patch = "\
diff --git a/vendor/lib b/vendor/lib
index 1111111..2222222 160000
--- a/vendor/lib
+++ b/vendor/lib
@@ -1 +1 @@
-Subproject commit 1111111111111111111111111111111111111111
+Subproject commit 2222222222222222222222222222222222222222
";
        let files = parse_unified_diff(patch).expect("parse");
        assert!(commentable_lines(&files[0]).is_empty());
        let missing = Path::new("/nonexistent-photon-worktree");
        assert_eq!(
            base_text(missing, "HEAD", &files[0]).await.expect("base"),
            None
        );
        let binary = file("logo.png", FileStatus::Modified, true);
        assert_eq!(
            base_text(missing, "HEAD", &binary).await.expect("base"),
            None
        );
        assert!(git_diff(missing, "--output=x", "HEAD").await.is_err());
    }

    #[test]
    fn text_file_mentioning_subproject_commit_stays_normal() {
        let patch = "\
diff --git a/notes.txt b/notes.txt
index 1..2 100644
--- a/notes.txt
+++ b/notes.txt
@@ -1 +1 @@
-Subproject commit 1111
+Subproject commit 2222
";
        let files = parse_unified_diff(patch).expect("parse");
        assert!(!files[0].submodule);
        assert_eq!(commentable_lines(&files[0]), vec![1..=1]);
    }

    #[gpui::test]
    async fn gitlink_with_submodule_log_config_is_a_submodule(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path();
        run(repo, &["init", "-q"]);
        run(repo, &["config", "diff.submodule", "log"]);
        let link = |sha: &str| format!("160000,{sha},vendor/lib");
        run(
            repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &link(&"1".repeat(40)),
            ],
        );
        run(repo, &["commit", "-q", "-m", "base"]);
        let base = run(repo, &["rev-parse", "HEAD"]);
        run(
            repo,
            &["update-index", "--cacheinfo", &link(&"2".repeat(40))],
        );
        run(repo, &["commit", "-q", "-m", "head"]);
        let head = run(repo, &["rev-parse", "HEAD"]);

        let files = git_diff(repo, &base, &head).await.expect("diff");
        assert_eq!(files.len(), 1);
        assert!(files[0].submodule);
        assert!(commentable_lines(&files[0]).is_empty());
        assert_eq!(base_text(repo, &base, &files[0]).await.expect("base"), None);
    }

    #[gpui::test]
    async fn user_diff_config_does_not_change_hunks(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path();
        run(repo, &["init", "-q"]);
        run(repo, &["config", "diff.algorithm", "patience"]);
        run(repo, &["config", "diff.context", "10"]);
        run(repo, &["config", "diff.interHunkContext", "10"]);
        let lines = |change: bool| -> String {
            (1..=40)
                .map(|number| match (change, number) {
                    (true, 10 | 25) => format!("changed {number}\n"),
                    _ => format!("line {number}\n"),
                })
                .collect()
        };
        std::fs::write(repo.join("a.txt"), lines(false)).expect("write");
        run(repo, &["add", "."]);
        run(repo, &["commit", "-q", "-m", "base"]);
        let base = run(repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("a.txt"), lines(true)).expect("write");
        run(repo, &["commit", "-q", "-am", "head"]);
        let head = run(repo, &["rev-parse", "HEAD"]);

        let files = git_diff(repo, &base, &head).await.expect("diff");
        let expected = run(
            repo,
            &[
                "-c",
                "diff.algorithm=myers",
                "-c",
                "diff.context=3",
                "-c",
                "diff.interHunkContext=0",
                "diff",
                "-U3",
                "--no-color",
                &base,
                &head,
            ],
        );
        assert_eq!(
            files,
            parse_unified_diff(&format!("{expected}\n")).expect("parse")
        );
        assert_eq!(files[0].hunks.len(), 2);
        assert_eq!(files[0].hunks[0].old_len, 7);
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
    async fn diff_handles_renames_deletes_binaries_and_spaces(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path();
        run(repo, &["init", "-q"]);
        std::fs::create_dir(repo.join("dir")).expect("dir");
        let old_contents = "fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\nfn five() {}\n";
        std::fs::write(repo.join("dir/a b.txt"), "x\n").expect("write");
        std::fs::write(repo.join("old.rs"), old_contents).expect("write");
        std::fs::write(repo.join("gone.rs"), "bye\n").expect("write");
        run(repo, &["add", "."]);
        run(repo, &["commit", "-q", "-m", "base"]);
        let base = run(repo, &["rev-parse", "HEAD"]);

        std::fs::write(repo.join("dir/a b.txt"), "y\n").expect("write");
        std::fs::rename(repo.join("old.rs"), repo.join("new.rs")).expect("rename");
        std::fs::write(repo.join("new.rs"), old_contents.replace("two", "TWO")).expect("write");
        std::fs::remove_file(repo.join("gone.rs")).expect("remove");
        std::fs::write(repo.join("logo.png"), [0u8, 159, 146, 150, 0, 1]).expect("write");
        std::fs::write(repo.join("café.rs"), "fn cafe() {}\n").expect("write");
        run(repo, &["add", "-A"]);
        run(repo, &["commit", "-q", "-m", "head"]);
        let head = run(repo, &["rev-parse", "HEAD"]);

        let mut files = git_diff(repo, &base, &head).await.expect("diff");
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let summary: Vec<_> = files
            .iter()
            .map(|file| (file.path.as_str(), file.status.clone(), file.binary))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("café.rs", FileStatus::Added, false),
                ("dir/a b.txt", FileStatus::Modified, false),
                ("gone.rs", FileStatus::Deleted, false),
                ("logo.png", FileStatus::Added, true),
                (
                    "new.rs",
                    FileStatus::Renamed {
                        from: "old.rs".into()
                    },
                    false
                ),
            ]
        );
        let renamed = &files[4];
        assert_eq!(
            base_text(repo, &base, renamed).await.expect("base"),
            Some(old_contents.to_string())
        );
        assert_eq!(base_text(repo, &base, &files[0]).await.expect("base"), None);
        assert_eq!(
            base_text(repo, &base, &files[2]).await.expect("base"),
            Some("bye\n".to_string())
        );
    }

    async fn loaded(
        cx: &mut TestAppContext,
        base: Option<&str>,
        head: Option<&str>,
        status: FileStatus,
    ) -> LoadedFile {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
        });
        let fs = FakeFs::new(cx.executor());
        let tree = match head {
            Some(head) => json!({ "a.txt": head }),
            None => json!({}),
        };
        fs.insert_tree("/repo", tree).await;
        let project = Project::test(fs, ["/repo".as_ref()], cx).await;
        let diff = file("a.txt", status, false);
        let base = base.map(str::to_string);
        cx.update(|cx| build_loaded_file(&project, "/repo/a.txt".into(), base, diff, cx))
            .await
            .expect("loaded")
    }

    #[gpui::test]
    async fn loaded_file_hunks_match_patch(cx: &mut TestAppContext) {
        let loaded = loaded(
            cx,
            Some("a\nb\nc\n"),
            Some("a\nB\nc\n"),
            FileStatus::Modified,
        )
        .await;
        let (buffer, diff) = loaded.contents.expect("contents");
        let rows: Vec<_> = cx.read(|cx| {
            let snapshot = buffer.read(cx).snapshot();
            diff.read(cx)
                .snapshot(cx)
                .hunks(&snapshot.text)
                .map(|hunk| (hunk.range.start.row, hunk.range.end.row))
                .collect()
        });
        assert_eq!(rows, vec![(1, 2)]);
    }

    #[gpui::test]
    async fn deleted_file_shows_base_as_removed(cx: &mut TestAppContext) {
        let loaded = loaded(cx, Some("a\nb\n"), None, FileStatus::Deleted).await;
        let (buffer, diff) = loaded.contents.expect("contents");
        let hunks: Vec<_> = cx.read(|cx| {
            let snapshot = buffer.read(cx).snapshot();
            assert_eq!(snapshot.text(), "");
            diff.read(cx)
                .snapshot(cx)
                .hunks(&snapshot.text)
                .map(|hunk| hunk.diff_base_byte_range)
                .collect()
        });
        assert_eq!(hunks, vec![0..4]);
    }

    #[gpui::test]
    async fn submodule_has_no_buffer(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/repo", json!({ "vendor": { "lib": {} } }))
            .await;
        let project = Project::test(fs, ["/repo".as_ref()], cx).await;
        let mut diff = file("vendor/lib", FileStatus::Modified, false);
        diff.submodule = true;
        let loaded = cx
            .update(|cx| build_loaded_file(&project, "/repo/vendor/lib".into(), None, diff, cx))
            .await
            .expect("loaded");
        assert!(loaded.contents.is_none());
    }

    const SYMLINK_PATCH: &str = "\
diff --git a/key b/key
new file mode 120000
index 0000000..1111111
--- /dev/null
+++ b/key
@@ -0,0 +1 @@
+/Users/someone/.ssh/id_rsa
\\ No newline at end of file
diff --git a/zero b/zero
index 2222222..3333333 120000
--- a/zero
+++ b/zero
@@ -1 +1 @@
-/tmp/x
\\ No newline at end of file
+/dev/zero
\\ No newline at end of file
diff --git a/old_link b/old_link
deleted file mode 120000
index 4444444..0000000
--- a/old_link
+++ /dev/null
@@ -1 +0,0 @@
-target
\\ No newline at end of file
diff --git a/plain.rs b/plain.rs
index 5555555..6666666 100644
--- a/plain.rs
+++ b/plain.rs
@@ -1 +1 @@
-a
+b
";

    #[test]
    fn symlinks_are_opaque() {
        let files = parse_unified_diff(SYMLINK_PATCH).expect("parse");
        let opaque: Vec<_> = files
            .iter()
            .map(|file| (file.path.as_str(), file.symlink, file.is_opaque()))
            .collect();
        assert_eq!(
            opaque,
            vec![
                ("key", true, true),
                ("zero", true, true),
                ("old_link", true, true),
                ("plain.rs", false, false),
            ]
        );
        assert!(commentable_lines(&files[0]).is_empty());
    }

    #[gpui::test]
    async fn symlink_is_never_opened(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/secret", json!({ "id_rsa": "private" }))
            .await;
        fs.insert_tree("/repo", json!({ "plain.rs": "b" })).await;
        fs.insert_symlink("/repo/key", "/secret/id_rsa".into())
            .await;
        let project = Project::test(fs, ["/repo".as_ref()], cx).await;

        let parsed = parse_unified_diff(SYMLINK_PATCH).expect("parse");
        let key = parsed.into_iter().next().expect("key entry");
        let loaded = cx
            .update(|cx| build_loaded_file(&project, "/repo/key".into(), None, key, cx))
            .await
            .expect("loaded");
        assert!(loaded.contents.is_none());

        // A symlink the diff didn't report as one is still skipped.
        let unmarked = file("key", FileStatus::Added, false);
        let loaded = cx
            .update(|cx| build_loaded_file(&project, "/repo/key".into(), None, unmarked, cx))
            .await
            .expect("loaded");
        assert!(loaded.contents.is_none());
        let is_open = project.read_with(cx, |project, cx| {
            project
                .buffer_store()
                .read(cx)
                .buffers()
                .any(|buffer| buffer.read(cx).text() == "private")
        });
        assert!(!is_open);
    }

    async fn open_text_buffer(cx: &mut TestAppContext) -> (Arc<FakeFs>, Entity<Buffer>) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/repo", json!({ "a.txt": "old\n" })).await;
        let project = Project::test(fs.clone(), ["/repo".as_ref()], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/repo/a.txt", cx)
            })
            .await
            .expect("buffer");
        (fs, buffer)
    }

    #[gpui::test]
    async fn superseded_reload_still_reads_disk(cx: &mut TestAppContext) {
        let (fs, buffer) = open_text_buffer(cx).await;
        fs.insert_file("/repo/a.txt", b"new\n".to_vec()).await;
        let mut async_cx = cx.to_async();
        let mut reload = Box::pin(reload_from_disk(&buffer, &mut async_cx));
        assert!(futures::poll!(reload.as_mut()).is_pending());
        // Replaces our reload, as the file watcher's reload would.
        let _newer = buffer.update(cx, |buffer, cx| buffer.reload(cx));
        reload.await;
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), "new\n");
    }

    #[gpui::test]
    async fn reload_keeps_unsaved_edits(cx: &mut TestAppContext) {
        let (fs, buffer) = open_text_buffer(cx).await;
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "edited ")], None, cx));
        fs.insert_file("/repo/a.txt", b"new\n".to_vec()).await;
        reload_from_disk(&buffer, &mut cx.to_async()).await;
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "edited old\n"
        );
    }
}
