use std::sync::Arc;

use anyhow::Result;
use futures::future::join_all;
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    Render, SharedString, Task, WeakEntity, Window, actions, px,
};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    ChecksState, GhError, GhRunner, Inbox, InboxGroup, PrSummary, ReviewDb, SinceReview,
    auth_status_request, cleanup_checkout, gh_runner, inbox_request, parse_auth_status,
    parse_inbox,
};

actions!(
    photon_review,
    [
        /// Toggles the pull request inbox.
        ToggleInbox,
        /// Reloads the pull request inbox from GitHub.
        RefreshInbox
    ]
);

const INBOX_PANEL_NAME: &str = "PhotonReviewInbox";
const DEFAULT_HOST: &str = "github.com";
const EMPTY_INBOX_TEXT: &str = "No open pull requests.";

pub(crate) fn init_inbox(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleInbox, window, cx| {
            workspace.toggle_panel_focus::<InboxPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &RefreshInbox, _, cx| {
            if let Some(panel) = workspace.panel::<InboxPanel>(cx) {
                panel.update(cx, |panel, cx| panel.refresh(cx));
            }
        });
    })
    .detach();
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostInbox {
    pub host: String,
    pub result: Result<Inbox, GhError>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InboxState {
    Idle,
    Loading,
    Loaded(Vec<HostInbox>),
    Failed(GhError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowMeta {
    pub title: String,
    pub meta: String,
    pub badges: Vec<String>,
    pub primary_action: &'static str,
}

fn plural(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

pub fn row_meta(summary: &PrSummary, has_drafts: bool) -> RowMeta {
    let key = &summary.key;
    let repository = if key.host == DEFAULT_HOST {
        format!("{}/{}", key.owner, key.repo)
    } else {
        format!("{}/{}/{}", key.host, key.owner, key.repo)
    };
    let mut badges = Vec::new();
    match summary.checks {
        ChecksState::Passing => badges.push("checks passing".to_string()),
        ChecksState::Failing => badges.push("checks failing".to_string()),
        ChecksState::Pending => badges.push("checks pending".to_string()),
        ChecksState::Unknown => {}
    }
    if summary.unresolved_threads > 0 {
        let count = summary.unresolved_threads;
        badges.push(format!(
            "{count} unresolved thread{}",
            if count == 1 { "" } else { "s" }
        ));
    }
    match summary.since_review {
        SinceReview::NewCommits(count) => {
            badges.push(format!("{} since your review", plural(count, "new commit")))
        }
        SinceReview::ForcePushed => badges.push("force-pushed since your review".to_string()),
        SinceReview::NotReviewed | SinceReview::UpToDate => {}
    }
    RowMeta {
        title: summary.title.clone(),
        meta: format!("#{} · {} · {repository}", key.number, summary.author),
        badges,
        primary_action: if summary.last_review_commit.is_some() || has_drafts {
            "Continue review"
        } else {
            "Start review"
        },
    }
}

fn section_label(group: InboxGroup, count: usize) -> String {
    let name = match group {
        InboxGroup::ReviewRequested => "Review requested",
        InboxGroup::Yours => "Yours",
        InboxGroup::Mentioned => "Mentioned",
    };
    format!("{name} · {count}")
}

fn host_line(hosts: &[HostInbox]) -> String {
    hosts
        .iter()
        .map(|host| host.host.as_str())
        .collect::<Vec<_>>()
        .join(" + ")
}

fn badge_color(badge: &str) -> Color {
    if badge.starts_with("checks failing") {
        Color::Error
    } else if badge.starts_with("checks passing") {
        Color::Success
    } else if badge.starts_with("checks pending") {
        Color::Warning
    } else if badge.contains("since your review") {
        Color::Accent
    } else {
        Color::Muted
    }
}

// Task 9 replaces this with `crate::review::open_review`.
fn start_review(_summary: &PrSummary, _window: &mut Window, _cx: &mut App) {}

// Task 13 replaces this with `crate::run_locally::run_locally`.
fn run_locally(_summary: &PrSummary, _window: &mut Window, _cx: &mut App) {}

pub struct InboxPanel {
    _workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    state: InboxState,
    load_task: Task<()>,
}

impl InboxPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            cx.update(|_, cx| {
                cx.new(|cx| Self {
                    _workspace: workspace,
                    focus_handle: cx.focus_handle(),
                    state: InboxState::Idle,
                    load_task: Task::ready(()),
                })
            })
        })
    }

    pub fn state(&self) -> &InboxState {
        &self.state
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let runner = gh_runner(cx);
        let database = ReviewDb::global(cx);
        self.state = InboxState::Loading;
        self.load_task = cx.spawn(async move |this, cx| {
            let state = cx.background_spawn(load_inbox(runner, database)).await;
            this.update(cx, |this, cx| {
                this.state = state;
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }
}

fn failed(error: anyhow::Error) -> GhError {
    GhError::Failed {
        message: format!("{error:#}"),
    }
}

async fn load_inbox(runner: Arc<dyn GhRunner>, database: ReviewDb) -> InboxState {
    let hosts = match runner.run(auth_status_request()).await {
        Ok(output) => match parse_auth_status(&output) {
            Ok(hosts) => hosts,
            Err(error) => return InboxState::Failed(failed(error)),
        },
        Err(error) => return InboxState::Failed(error),
    };
    if hosts.is_empty() {
        return InboxState::Failed(GhError::LoggedOut { host: None });
    }

    let tracked = database.tracked().log_err().unwrap_or_default();
    let results = join_all(hosts.into_iter().map(|auth| {
        let runner = runner.clone();
        let node_ids: Vec<String> = tracked
            .iter()
            .filter(|tracked| tracked.key.host == auth.host)
            .map(|tracked| tracked.node_id.clone())
            .collect();
        async move {
            let result = match auth.login {
                None => Err(GhError::LoggedOut {
                    host: Some(auth.host.clone()),
                }),
                Some(login) => match runner
                    .run(inbox_request(&auth.host, &login, &node_ids))
                    .await
                {
                    Ok(output) => parse_inbox(&auth.host, &output).map_err(failed),
                    Err(error) => Err(error),
                },
            };
            HostInbox {
                host: auth.host,
                result,
            }
        }
    }))
    .await;

    for host in &results {
        let Ok(inbox) = &host.result else { continue };
        for node_id in &inbox.closed_tracked {
            let Some(closed) = tracked
                .iter()
                .find(|tracked| tracked.key.host == host.host && &tracked.node_id == node_id)
            else {
                continue;
            };
            let cleaned = cleanup_checkout(&closed.repo_dir, &closed.key, &closed.worktree).await;
            if cleaned.log_err().is_some() {
                database.untrack(closed.key.clone()).await.log_err();
            }
        }
    }
    InboxState::Loaded(results)
}

impl EventEmitter<PanelEvent> for InboxPanel {}

impl Focusable for InboxPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for InboxPanel {
    fn persistent_name() -> &'static str {
        INBOX_PANEL_NAME
    }

    fn panel_key() -> &'static str {
        INBOX_PANEL_NAME
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(360.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::PullRequest)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Pull requests")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleInbox)
    }

    fn activation_priority(&self) -> u32 {
        8
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.refresh(cx);
        }
    }
}

impl InboxPanel {
    fn render_note(text: impl Into<SharedString>, color: Color) -> impl IntoElement {
        Label::new(text).color(color).size(LabelSize::Small)
    }

    fn render_row(&self, summary: &PrSummary, cx: &mut Context<Self>) -> impl IntoElement {
        let row = row_meta(summary, false);
        let key = &summary.key;
        let id = format!("{}/{}/{}#{}", key.host, key.owner, key.repo, key.number);
        let primary_summary = summary.clone();
        let local_summary = summary.clone();
        v_flex()
            .id(SharedString::from(id.clone()))
            .gap_1()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new(row.title))
            .child(
                Label::new(row.meta)
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .children(row.badges.iter().map(|badge| {
                        Label::new(badge.clone())
                            .color(badge_color(badge))
                            .size(LabelSize::XSmall)
                    })),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Button::new(SharedString::from(format!("{id}-run")), "Run locally")
                            .style(ButtonStyle::Subtle)
                            .on_click(move |_, window, cx| run_locally(&local_summary, window, cx)),
                    )
                    .child(
                        Button::new(
                            SharedString::from(format!("{id}-review")),
                            row.primary_action,
                        )
                        .style(ButtonStyle::Filled)
                        .on_click(move |_, window, cx| start_review(&primary_summary, window, cx)),
                    ),
            )
    }
}

impl Render for InboxPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = v_flex()
            .id("photon-review-inbox-body")
            .flex_1()
            .overflow_y_scroll();
        let mut host_line_text = None;
        match &self.state {
            InboxState::Idle => {}
            InboxState::Loading => body = body.child(Self::render_note("Loading…", Color::Muted)),
            InboxState::Failed(error) => {
                body = body.child(Self::render_note(error.to_string(), Color::Error))
            }
            InboxState::Loaded(hosts) => {
                host_line_text = Some(host_line(hosts));
                let mut any_rows = false;
                for host in hosts {
                    match &host.result {
                        Err(error) => {
                            body = body.child(Self::render_note(error.to_string(), Color::Error))
                        }
                        Ok(inbox) => {
                            for (group, rows) in &inbox.sections {
                                if rows.is_empty() {
                                    continue;
                                }
                                any_rows = true;
                                body = body.child(
                                    Label::new(section_label(*group, rows.len()))
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                );
                                for summary in rows {
                                    body = body.child(self.render_row(summary, cx));
                                }
                            }
                        }
                    }
                }
                if !any_rows && hosts.iter().all(|host| host.result.is_ok()) {
                    body = body.child(Self::render_note(EMPTY_INBOX_TEXT, Color::Muted));
                }
            }
        }
        v_flex()
            .key_context(INBOX_PANEL_NAME)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .justify_between()
                    .child(Label::new("Pull requests"))
                    .children(
                        host_line_text.map(|text| {
                            Label::new(text).color(Color::Muted).size(LabelSize::Small)
                        }),
                    )
                    .child(
                        IconButton::new("photon-review-refresh", IconName::RotateCw)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(RefreshInbox.boxed_clone(), cx)
                            }),
                    ),
            )
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use gpui::{AvailableSpace, TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use workspace::{MultiWorkspace, Workspace};

    use super::*;
    use crate::{FakeGh, PrKey, Tracked, set_gh_runner};

    const AUTH_ONE_HOST: &str =
        r#"{"hosts":{"github.com":[{"state":"success","active":true,"login":"alice"}]}}"#;
    const AUTH_TWO_HOSTS: &str = include_str!("../test_fixtures/auth_status.json");
    const INBOX: &str = include_str!("../test_fixtures/inbox.json");

    fn init_test(cx: &mut TestAppContext, fake: &Arc<FakeGh>) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            cx.set_global(db::AppDatabase::test_new());
            set_gh_runner(fake.clone(), cx);
            init_inbox(cx);
        });
    }

    fn respond_auth(fake: &FakeGh, json: &'static str) {
        fake.respond(|request| request.args[0] == "auth", Ok(json.into()));
    }

    fn respond_inbox(fake: &FakeGh, host: &'static str) {
        fake.respond(
            move |request| request.args[0] == "api" && request.args.iter().any(|arg| arg == host),
            Ok(INBOX.into()),
        );
    }

    async fn add_panel(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, Entity<InboxPanel>, VisualTestContext) {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let weak_workspace = workspace.downgrade();
        let panel = window
            .update(cx, |_, window, cx| {
                cx.spawn_in(window, async move |_this, cx| {
                    InboxPanel::load(weak_workspace, cx.clone()).await
                })
            })
            .expect("window")
            .await
            .expect("load panel");
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        workspace.update_in(&mut visual, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        (workspace, panel, visual)
    }

    fn render(panel: &Entity<InboxPanel>, cx: &mut VisualTestContext) {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(
                AvailableSpace::Definite(px(360.)),
                AvailableSpace::Definite(px(600.)),
            ),
            |_, _| panel.clone().into_any_element(),
        );
    }

    fn set_active(panel: &Entity<InboxPanel>, active: bool, cx: &mut VisualTestContext) {
        panel.update_in(cx, |panel, window, cx| panel.set_active(active, window, cx));
        cx.run_until_parked();
    }

    fn request_summary(fake: &FakeGh) -> Vec<String> {
        fake.requests()
            .iter()
            .map(|request| {
                if request.args[0] == "auth" {
                    "auth".to_string()
                } else {
                    format!("api {}", request.args[3])
                }
            })
            .collect()
    }

    fn summary(number: u64) -> PrSummary {
        PrSummary {
            key: PrKey {
                host: "github.com".into(),
                owner: "acme".into(),
                repo: "web".into(),
                number,
            },
            node_id: format!("PR_{number}"),
            title: "Add retry".into(),
            author: "alice".into(),
            url: String::new(),
            head_sha: "a1".into(),
            base_ref: "main".into(),
            checks: ChecksState::Unknown,
            unresolved_threads: 0,
            last_review_commit: None,
            since_review: SinceReview::NotReviewed,
        }
    }

    #[gpui::test]
    async fn panel_creation_runs_no_gh(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        let (_workspace, panel, cx) = add_panel(cx).await;
        cx.run_until_parked();
        assert!(fake.requests().is_empty());
        panel.read_with(&cx, |panel, _| {
            assert!(matches!(panel.state(), InboxState::Idle))
        });
    }

    #[gpui::test]
    async fn opening_panel_loads_groups(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, AUTH_ONE_HOST);
        respond_inbox(&fake, "github.com");
        let (_workspace, panel, mut cx) = add_panel(cx).await;
        set_active(&panel, true, &mut cx);
        assert_eq!(request_summary(&fake), ["auth", "api github.com"]);
        panel.update_in(&mut cx, |panel, window, _| {
            let InboxState::Loaded(hosts) = panel.state() else {
                panic!("expected Loaded, got {:?}", panel.state());
            };
            assert_eq!(hosts.len(), 1);
            let inbox = hosts[0].result.as_ref().expect("inbox");
            assert_eq!(inbox.sections.len(), 3);
            assert_eq!(host_line(hosts), "github.com");
            window.refresh();
        });
        render(&panel, &mut cx);
    }

    #[gpui::test]
    async fn refresh_on_open_and_manual_only(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        for _ in 0..3 {
            respond_auth(&fake, AUTH_ONE_HOST);
            respond_inbox(&fake, "github.com");
        }
        let (workspace, panel, mut cx) = add_panel(cx).await;
        set_active(&panel, true, &mut cx);
        assert_eq!(fake.requests().len(), 2);

        cx.executor().advance_clock(Duration::from_secs(3600));
        cx.run_until_parked();
        assert_eq!(fake.requests().len(), 2);

        workspace.update_in(&mut cx, |_, window, cx| {
            window.dispatch_action(RefreshInbox.boxed_clone(), cx)
        });
        cx.run_until_parked();
        assert_eq!(fake.requests().len(), 4);

        set_active(&panel, false, &mut cx);
        assert_eq!(fake.requests().len(), 4);
        set_active(&panel, true, &mut cx);
        assert_eq!(fake.requests().len(), 6);
    }

    #[gpui::test]
    async fn gh_missing_shows_install_hint(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        fake.respond(|_| true, Err(GhError::NotInstalled));
        let (_workspace, panel, mut cx) = add_panel(cx).await;
        set_active(&panel, true, &mut cx);
        panel.read_with(&cx, |panel, _| {
            let InboxState::Failed(error) = panel.state() else {
                panic!("expected Failed");
            };
            assert_eq!(error, &GhError::NotInstalled);
            assert!(error.to_string().contains("brew install gh"));
        });
        render(&panel, &mut cx);
    }

    #[gpui::test]
    async fn logged_out_shows_login_hint(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, r#"{"hosts":{}}"#);
        let (_workspace, panel, mut cx) = add_panel(cx).await;
        set_active(&panel, true, &mut cx);
        panel.read_with(&cx, |panel, _| {
            assert!(matches!(
                panel.state(),
                InboxState::Failed(GhError::LoggedOut { host: None })
            ));
        });
    }

    #[gpui::test]
    async fn one_host_logged_out_still_lists_others(cx: &mut TestAppContext) {
        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, AUTH_TWO_HOSTS);
        respond_inbox(&fake, "github.com");
        let (_workspace, panel, mut cx) = add_panel(cx).await;
        set_active(&panel, true, &mut cx);
        assert_eq!(request_summary(&fake), ["auth", "api github.com"]);
        panel.update_in(&mut cx, |panel, _, _| {
            let InboxState::Loaded(hosts) = panel.state() else {
                panic!("expected Loaded");
            };
            let by_host = |name: &str| hosts.iter().find(|host| host.host == name).expect("host");
            assert!(by_host("github.com").result.is_ok());
            let error = by_host("ghe.corp").result.as_ref().expect_err("logged out");
            assert_eq!(
                error,
                &GhError::LoggedOut {
                    host: Some("ghe.corp".into())
                }
            );
            assert!(
                error
                    .to_string()
                    .contains("gh auth login --hostname ghe.corp")
            );
            assert_eq!(host_line(hosts), "ghe.corp + github.com");
        });
        render(&panel, &mut cx);
    }

    #[gpui::test]
    async fn closed_tracked_pr_is_untracked(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let repo = tempfile::tempdir().expect("tempdir");
        crate::checkout::git(repo.path(), &["init", "-q"])
            .await
            .expect("git init");

        let fake = FakeGh::new();
        init_test(cx, &fake);
        respond_auth(&fake, AUTH_ONE_HOST);
        respond_inbox(&fake, "github.com");
        let key = PrKey {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "web".into(),
            number: 1,
        };
        let db = cx.update(|cx| ReviewDb::global(cx));
        db.track(Tracked {
            key,
            node_id: "PR_x".into(),
            repo_dir: repo.path().to_path_buf(),
            worktree: repo.path().join("missing-worktree"),
        })
        .await
        .expect("track");
        assert_eq!(db.tracked().expect("tracked").len(), 1);

        let (_workspace, panel, mut cx) = add_panel(cx).await;
        set_active(&panel, true, &mut cx);
        for _ in 0..50 {
            if db.tracked().expect("tracked").is_empty() {
                break;
            }
            cx.executor().timer(Duration::from_millis(100)).await;
            cx.run_until_parked();
        }
        assert_eq!(db.tracked().expect("tracked"), vec![]);
        drop(panel);
    }

    #[test]
    fn row_meta_texts() {
        let mut row = summary(482);
        let meta = row_meta(&row, false);
        assert_eq!(meta.title, "Add retry");
        assert_eq!(meta.meta, "#482 · alice · acme/web");
        assert!(meta.badges.is_empty());
        assert_eq!(meta.primary_action, "Start review");

        row.key.host = "ghe.corp".into();
        assert_eq!(
            row_meta(&row, false).meta,
            "#482 · alice · ghe.corp/acme/web"
        );
        row.key.host = "github.com".into();

        for (checks, text) in [
            (ChecksState::Passing, Some("checks passing")),
            (ChecksState::Failing, Some("checks failing")),
            (ChecksState::Pending, Some("checks pending")),
            (ChecksState::Unknown, None),
        ] {
            row.checks = checks;
            assert_eq!(
                row_meta(&row, false).badges,
                text.map(String::from).into_iter().collect::<Vec<_>>()
            );
        }

        row.checks = ChecksState::Failing;
        row.unresolved_threads = 1;
        row.since_review = SinceReview::NewCommits(1);
        assert_eq!(
            row_meta(&row, false).badges,
            [
                "checks failing",
                "1 unresolved thread",
                "1 new commit since your review"
            ]
        );
        row.unresolved_threads = 2;
        row.since_review = SinceReview::NewCommits(3);
        assert_eq!(
            row_meta(&row, false).badges,
            [
                "checks failing",
                "2 unresolved threads",
                "3 new commits since your review"
            ]
        );
        row.since_review = SinceReview::ForcePushed;
        assert_eq!(
            row_meta(&row, false).badges[2],
            "force-pushed since your review"
        );
        row.since_review = SinceReview::UpToDate;
        assert_eq!(row_meta(&row, false).badges.len(), 2);

        assert_eq!(row_meta(&row, true).primary_action, "Continue review");
        row.last_review_commit = Some("a0".into());
        assert_eq!(row_meta(&row, false).primary_action, "Continue review");
    }

    #[test]
    fn section_labels_and_copy() {
        assert_eq!(
            section_label(InboxGroup::ReviewRequested, 3),
            "Review requested · 3"
        );
        assert_eq!(section_label(InboxGroup::Yours, 2), "Yours · 2");
        assert_eq!(section_label(InboxGroup::Mentioned, 0), "Mentioned · 0");
        assert_eq!(EMPTY_INBOX_TEXT, "No open pull requests.");
    }
}
