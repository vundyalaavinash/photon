use std::path::PathBuf;

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, AsyncWindowContext, Context, Task, Window};
use settings::{RegisterSetting, Settings, SettingsLocation};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use util::{ResultExt as _, rel_path::RelPath};
use workspace::{
    AppState, OpenMode, OpenOptions, Toast, Workspace, notifications::NotificationId, open_paths,
};

use crate::{
    Checkout, PrSummary, ReviewDb, Tracked, candidate_repos, fetch_url, find_clone,
    prepare_checkout, repo_id, review_root,
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
                .filter(|command| !command.trim().is_empty()),
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
    let candidates = candidate_repos(cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let result = open_terminal(summary, app_state, database, candidates, cx).await;
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
pub(crate) async fn prepare_local_checkout(
    summary: &PrSummary,
    fetch_url: String,
    root: PathBuf,
    candidates: Vec<PathBuf>,
    database: ReviewDb,
) -> Result<Checkout> {
    let clone = find_clone(candidates, &repo_id(summary)).await;
    let checkout =
        prepare_checkout(&summary.key, &summary.base_ref, &fetch_url, clone, &root).await?;
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

async fn open_terminal(
    summary: PrSummary,
    app_state: std::sync::Arc<AppState>,
    database: ReviewDb,
    candidates: Task<Vec<PathBuf>>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let candidates = candidates.await;
    let url = fetch_url(&repo_id(&summary));
    let checkout = cx
        .background_spawn(async move {
            prepare_local_checkout(&summary, url, review_root(), candidates, database).await
        })
        .await?;
    let worktree = checkout.worktree;

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
    let workspace = opened.workspace;
    let window = opened.window;

    let terminal = window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let cwd = worktree.clone();
                TerminalPanel::add_center_terminal(workspace, window, cx, move |project, cx| {
                    project.create_terminal_shell(Some(cwd), cx)
                })
            })
        })?
        .await?;

    let scan = workspace.update(cx, |workspace, cx| {
        let (tree, _) = workspace.project().read(cx).find_worktree(&worktree, cx)?;
        let id = tree.read(cx).id();
        let scan = tree.read(cx).as_local()?.scan_complete();
        Some((id, scan))
    });
    let Some((worktree_id, scan)) = scan else {
        return Err(anyhow!("{} did not open as a worktree", worktree.display()));
    };
    scan.await;

    let test_command = cx.update(|_, cx| test_command(worktree_id, cx))?;
    let Some(test_command) = test_command else {
        return Ok(());
    };
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

fn test_command(worktree_id: project::WorktreeId, cx: &App) -> Option<String> {
    PrReviewSettings::get(
        Some(SettingsLocation {
            worktree_id,
            path: RelPath::empty(),
        }),
        cx,
    )
    .test_command
    .clone()
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
        assert_eq!(again, checkout);
        assert_eq!(fixture.user_state(), before);
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
                test_command(worktree_id, cx)
            })
        };
        assert_eq!(read(&plain, cx), None);
        assert_eq!(read(&configured, cx).as_deref(), Some("npm test"));
    }
}
