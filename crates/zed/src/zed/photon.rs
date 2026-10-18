use command_palette_hooks::CommandPaletteFilter;
use gpui::{App, Context, Window};
use terminal_view::TerminalView;
use workspace::{NewCenterTerminal, Workspace};

/// Hides actions for services Photon keeps switched off (Zed account, collaboration, auto-update).
pub fn hide_offline_actions(cx: &mut App) {
    CommandPaletteFilter::update_global(cx, |filter, _| {
        for namespace in [
            "client",
            "collab",
            "collab_panel",
            "channel_modal",
            "auto_update",
        ] {
            filter.hide_namespace(namespace);
        }
    });
}

/// Photon is terminal-first: a workspace with nothing open starts with a terminal in the center.
pub fn ensure_center_terminal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if should_open_center_terminal(workspace, cx) {
        TerminalView::deploy(workspace, &center_terminal_action(), window, cx);
    }
}

fn should_open_center_terminal(workspace: &Workspace, cx: &App) -> bool {
    workspace.items(cx).next().is_none()
}

fn center_terminal_action() -> NewCenterTerminal {
    NewCenterTerminal { local: false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn offline_actions_hidden(cx: &mut TestAppContext) {
        cx.update(|cx| {
            command_palette_hooks::init(cx);
            hide_offline_actions(cx);
            let filter = CommandPaletteFilter::try_global(cx).expect("filter initialized");
            assert!(filter.is_hidden(&client::SignIn));
            assert!(filter.is_hidden(&auto_update::Check));
            assert!(!filter.is_hidden(&workspace::NewCenterTerminal::default()));
        });
    }

    #[gpui::test]
    fn user_settings_override_photon_defaults(cx: &mut TestAppContext) {
        use gpui::UpdateGlobal as _;
        use settings::{Settings as _, SettingsStore};
        cx.update(|cx| {
            // The real defaults; `SettingsStore::test` swaps in a test theme.
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            let dark_theme = |cx: &gpui::App| {
                theme_settings::ThemeSettings::get_global(cx)
                    .theme
                    .name(gpui::WindowAppearance::Dark.into())
                    .0
            };
            assert_eq!(dark_theme(cx).as_ref(), "Photon Black");
            assert!(!client::TelemetrySettings::get_global(cx).metrics);

            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(
                        r#"{"theme": {"mode": "dark", "light": "One Light", "dark": "One Dark"},
                            "telemetry": {"metrics": true}}"#,
                        cx,
                    )
                    .unwrap();
            });
            assert_eq!(dark_theme(cx).as_ref(), "One Dark");
            assert!(client::TelemetrySettings::get_global(cx).metrics);
        });
    }

    #[test]
    fn center_terminal_follows_the_project_host() {
        // Remote projects must get a shell on the remote host, not on this machine.
        assert!(!center_terminal_action().local);
    }

    #[gpui::test]
    async fn center_terminal_only_for_empty_workspace(cx: &mut TestAppContext) {
        use project::Project;
        use serde_json::json;
        use util::path;
        use workspace::{MultiWorkspace, WorkspaceHandle as _};

        let app_state = super::super::tests::init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/root"), json!({ "notes.txt": "hello" }))
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();

        cx.read(|cx| assert!(should_open_center_terminal(workspace.read(cx), cx)));

        let file = cx.read(|cx| workspace.file_project_paths(cx))[0].clone();
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.open_path(file, None, true, window, cx)
                })
            })
            .unwrap()
            .await
            .unwrap();

        // A restored or opened file means the user already has content; don't add a terminal.
        cx.read(|cx| assert!(!should_open_center_terminal(workspace.read(cx), cx)));
    }

    #[test]
    fn hack_font_is_bundled() {
        use gpui::AssetSource as _;
        let fonts = assets::Assets.list("fonts").expect("list fonts");
        for style in ["Regular", "Bold", "Italic", "BoldItalic"] {
            let path = format!("fonts/hack/Hack-{style}.ttf");
            assert!(fonts.iter().any(|font| font.as_ref() == path), "{path}");
        }
    }
}
