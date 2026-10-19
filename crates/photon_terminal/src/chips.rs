use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Result, bail};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Task, WeakEntity, Window, rems,
};
use picker::{Picker, PickerDelegate};
use terminal::Terminal;
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::{ResultExt as _, command::new_command, paths::PathExt as _};
use workspace::{OpenOptions, OpenVisible, Workspace};

use crate::history::is_subsequence;

const PICKER_LIMIT: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitInfo {
    pub branch: String,
    pub changed: usize,
    pub branches: Vec<String>,
}

/// Reads branch, dirty-file count and local branches without taking git's index lock or
/// touching the network. None outside a repository (or when git is missing).
pub fn git_info(cwd: &Path, cx: &App) -> Task<Option<GitInfo>> {
    let cwd = cwd.to_path_buf();
    cx.background_spawn(async move {
        let branch = match git_output(&cwd, &["symbolic-ref", "--short", "-q", "HEAD"]).await {
            Some(branch) => branch,
            // Detached HEAD has no symbolic ref.
            None => git_output(&cwd, &["rev-parse", "--short", "HEAD"]).await?,
        };
        let changed = git_output(&cwd, &["status", "--porcelain"])
            .await
            .map_or(0, |status| status.lines().count());
        let branches = git_output(&cwd, &["branch", "--format=%(refname:short)"])
            .await
            .map(|branches| branches.lines().map(str::to_string).collect())
            .unwrap_or_default();
        Some(GitInfo {
            branch,
            changed,
            branches,
        })
    })
}

async fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    match run_git(cwd, args, false).await {
        Ok(output) => Some(output),
        // Git failing outside a repository is routine; git missing is not.
        Err(error) if error.is::<std::io::Error>() => {
            log::error!("running git: {error:?}");
            None
        }
        Err(_) => None,
    }
}

/// Read-only callers pass `optional_locks: false` so a background `git status` never holds
/// the index lock while the user runs git; `add` and `restore` must write the index anyway.
/// Fails with git's stderr.
pub(crate) async fn run_git(cwd: &Path, args: &[&str], optional_locks: bool) -> Result<String> {
    let mut command = new_command("git");
    command.arg("-C").arg(cwd).args(args);
    if !optional_locks {
        command.env("GIT_OPTIONAL_LOCKS", "0");
    }
    let output = command.output().await?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

pub fn cd_command(path: &Path) -> String {
    format!("cd -- {}", shell_quote(&path.to_string_lossy()))
}

pub fn switch_command(branch: &str, create: bool) -> String {
    // `-c` takes the next argument as the new name, so `--` would become the name. A name
    // starting with `-` is still safe there: git rejects it as an invalid branch name.
    if create {
        format!("git switch -c {}", shell_quote(branch))
    } else {
        format!("git switch -- {}", shell_quote(branch))
    }
}

pub fn folder_chip_text(cwd: &Path) -> String {
    format!("📁 {} ▾", cwd.compact().display())
}

pub fn branch_chip_text(info: &GitInfo) -> String {
    let changed = if info.changed > 0 {
        format!("  ± {} changed", info.changed)
    } else {
        String::new()
    };
    format!("⎇ {} ▾{changed}", info.branch)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ChipAction {
    Run(String),
    OpenFolder(PathBuf),
}

struct ChipItem {
    label: SharedString,
    highlighted: bool,
    action: ChipAction,
}

enum ChipSource {
    Folders {
        current: Option<PathBuf>,
        recent: Vec<PathBuf>,
    },
    Branches {
        info: GitInfo,
    },
}

pub struct ChipPicker {
    picker: Entity<Picker<ChipPickerDelegate>>,
}

impl ChipPicker {
    pub fn folders(
        terminal: WeakEntity<Terminal>,
        workspace: WeakEntity<Workspace>,
        current: Option<PathBuf>,
        recent: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(
            terminal,
            workspace,
            ChipSource::Folders { current, recent },
            "Change directory…",
            window,
            cx,
        )
    }

    pub fn branches(
        terminal: WeakEntity<Terminal>,
        workspace: WeakEntity<Workspace>,
        info: GitInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(
            terminal,
            workspace,
            ChipSource::Branches { info },
            "Switch branch…",
            window,
            cx,
        )
    }

    fn new(
        terminal: WeakEntity<Terminal>,
        workspace: WeakEntity<Workspace>,
        source: ChipSource,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = ChipPickerDelegate {
            picker: cx.entity().downgrade(),
            terminal,
            workspace,
            source,
            placeholder,
            items: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for ChipPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(rems(30.))
            .elevation_3(_cx)
            .child(self.picker.clone())
    }
}

impl Focusable for ChipPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for ChipPicker {}
impl workspace::ModalView for ChipPicker {}

pub struct ChipPickerDelegate {
    picker: WeakEntity<ChipPicker>,
    terminal: WeakEntity<Terminal>,
    workspace: WeakEntity<Workspace>,
    source: ChipSource,
    placeholder: &'static str,
    items: Vec<ChipItem>,
    selected_index: usize,
}

impl ChipPickerDelegate {
    fn items_for(&self, query: &str) -> Vec<ChipItem> {
        let needle = query.to_lowercase();
        let matches = |text: &str| is_subsequence(&needle, &text.to_lowercase());
        match &self.source {
            ChipSource::Folders { current, recent } => {
                let mut items: Vec<ChipItem> = recent
                    .iter()
                    .filter(|path| matches(&path.to_string_lossy()))
                    .take(PICKER_LIMIT)
                    .map(|path| ChipItem {
                        label: path.compact().to_string_lossy().into_owned().into(),
                        highlighted: current.as_ref() == Some(path),
                        action: ChipAction::Run(cd_command(path)),
                    })
                    .collect();
                if let Some(current) = current {
                    items.push(ChipItem {
                        label: "Open folder in Photon".into(),
                        highlighted: false,
                        action: ChipAction::OpenFolder(current.clone()),
                    });
                }
                items
            }
            ChipSource::Branches { info } => {
                let mut items: Vec<ChipItem> = info
                    .branches
                    .iter()
                    .filter(|branch| matches(branch))
                    .take(PICKER_LIMIT)
                    .map(|branch| ChipItem {
                        label: branch.clone().into(),
                        highlighted: *branch == info.branch,
                        action: ChipAction::Run(switch_command(branch, false)),
                    })
                    .collect();
                let name = query.trim();
                if name.is_empty() {
                    items.push(ChipItem {
                        label: "New branch… (type a name)".into(),
                        highlighted: false,
                        action: ChipAction::Run(String::new()),
                    });
                } else if !info.branches.iter().any(|branch| branch == name) {
                    items.push(ChipItem {
                        label: format!("New branch “{name}”").into(),
                        highlighted: false,
                        action: ChipAction::Run(switch_command(name, true)),
                    });
                }
                items
            }
        }
    }
}

impl PickerDelegate for ChipPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "terminal chips"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        self.placeholder.into()
    }

    fn match_count(&self) -> usize {
        self.items.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.items = self.items_for(&query);
        self.selected_index = self
            .items
            .iter()
            .position(|item| item.highlighted)
            .unwrap_or(0);
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let action = self
            .items
            .get(self.selected_index)
            .map(|item| item.action.clone());
        match action {
            Some(ChipAction::Run(command)) if !command.is_empty() => {
                self.terminal
                    .update(cx, |terminal, cx| terminal.run_command(&command, cx))
                    .log_err();
            }
            Some(ChipAction::OpenFolder(path)) => {
                self.workspace
                    .update(cx, |workspace, cx| {
                        workspace
                            .open_paths(
                                vec![path],
                                OpenOptions {
                                    visible: Some(OpenVisible::All),
                                    ..Default::default()
                                },
                                None,
                                window,
                                cx,
                            )
                            .detach();
                    })
                    .log_err();
            }
            _ => return,
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let item = self.items.get(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    Label::new(item.label.clone())
                        .single_line()
                        .color(if item.highlighted {
                            Color::Accent
                        } else {
                            Color::Default
                        }),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn cd_command_quotes_spaces_and_quotes() {
        assert_eq!(
            cd_command(Path::new("/tmp/it's here")),
            r"cd -- '/tmp/it'\''s here'"
        );
    }

    #[test]
    fn switch_command_variants() {
        assert_eq!(switch_command("main", false), "git switch -- 'main'");
        assert_eq!(switch_command("feat/x", true), "git switch -c 'feat/x'");
        assert_eq!(switch_command("-x; rm", false), "git switch -- '-x; rm'");
    }

    #[test]
    fn chip_text_formats() {
        let info = GitInfo {
            branch: "main".into(),
            changed: 2,
            branches: Vec::new(),
        };
        assert_eq!(branch_chip_text(&info), "⎇ main ▾  ± 2 changed");
        let clean = GitInfo { changed: 0, ..info };
        assert_eq!(branch_chip_text(&clean), "⎇ main ▾");
        assert_eq!(folder_chip_text(Path::new("/tmp/x")), "📁 /tmp/x ▾");
    }

    async fn git(directory: &Path, args: &[&str]) {
        let status = util::command::new_command("git")
            .arg("-C")
            .arg(directory)
            .args(args)
            .status()
            .await
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    #[gpui::test]
    async fn git_info_reads_temp_repo(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path();
        git(path, &["init", "-b", "main"]).await;
        git(path, &["config", "user.name", "Test"]).await;
        git(path, &["config", "user.email", "test@example.com"]).await;
        std::fs::write(path.join("a.txt"), "one").expect("write");
        git(path, &["add", "."]).await;
        git(path, &["commit", "-m", "init"]).await;
        git(path, &["branch", "feature"]).await;
        std::fs::write(path.join("a.txt"), "two").expect("write");

        let info = cx
            .update(|cx| git_info(path, cx))
            .await
            .expect("inside a repo");
        assert_eq!(info.branch, "main");
        assert_eq!(info.changed, 1);
        assert!(info.branches.contains(&"feature".to_string()));

        let head = util::command::new_command("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .expect("rev-parse");
        let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
        git(path, &["checkout", "--detach", "-q"]).await;
        let detached = cx
            .update(|cx| git_info(path, cx))
            .await
            .expect("inside a repo");
        assert!(sha.starts_with(&detached.branch));
        assert_ne!(detached.branch, "HEAD");
    }

    async fn run_in_shell(directory: &Path, command: &str) {
        let output = util::command::new_command("sh")
            .arg("-c")
            .arg(command)
            .current_dir(directory)
            .output()
            .await
            .expect("run sh");
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[gpui::test]
    async fn switch_commands_work_in_a_real_repo(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path();
        git(path, &["init", "-b", "main"]).await;
        git(path, &["config", "user.name", "Test"]).await;
        git(path, &["config", "user.email", "test@example.com"]).await;
        git(path, &["commit", "--allow-empty", "-m", "init"]).await;

        run_in_shell(path, &switch_command("feat/new", true)).await;
        let info = cx.update(|cx| git_info(path, cx)).await.expect("repo");
        assert_eq!(info.branch, "feat/new");
        assert!(info.branches.contains(&"feat/new".to_string()));

        run_in_shell(path, &switch_command("main", false)).await;
        let info = cx.update(|cx| git_info(path, cx)).await.expect("repo");
        assert_eq!(info.branch, "main");
    }

    #[gpui::test]
    async fn git_info_none_outside_repo(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("tempdir");
        let info = cx.update(|cx| git_info(directory.path(), cx)).await;
        assert!(info.is_none());
    }
}
