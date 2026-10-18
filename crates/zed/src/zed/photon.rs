use command_palette_hooks::CommandPaletteFilter;
use editor::Editor;
use gpui::{App, Context, Focusable as _, Window};
use terminal_view::TerminalView;
use util::ResultExt as _;
use workspace::{ItemHandle, NewCenterTerminal, Workspace};

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
        filter.hide_action_types(&[std::any::TypeId::of::<zed_actions::OpenAccountSettings>()]);
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

/// [`ensure_center_terminal`] for a window opened asynchronously (e.g. a remote/SSH project).
pub fn ensure_center_terminal_in(
    window: gpui::WindowHandle<workspace::MultiWorkspace>,
    cx: &mut gpui::AsyncApp,
) {
    window
        .update(cx, |multi_workspace, window, cx| {
            multi_workspace.workspace().update(cx, |workspace, cx| {
                ensure_center_terminal(workspace, window, cx)
            })
        })
        .log_err();
}

fn should_open_center_terminal(workspace: &Workspace, cx: &App) -> bool {
    workspace.items(cx).next().is_none()
}

fn center_terminal_action() -> NewCenterTerminal {
    NewCenterTerminal { local: false }
}

gpui::actions!(
    photon,
    [
        /// Moves focus between the most recently used center terminal and editor.
        ToggleTerminalEditorFocus
    ]
);

pub fn toggle_terminal_editor_focus(
    workspace: &mut Workspace,
    _: &ToggleTerminalEditorFocus,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let active_pane = workspace.active_pane().clone();
    if !active_pane.focus_handle(cx).contains_focused(window, cx) {
        return;
    }
    let Some(active_item) = active_pane.read(cx).active_item() else {
        return;
    };
    let target = if active_item.downcast::<TerminalView>().is_some() {
        most_recently_active_item::<Editor>(workspace, cx)
    } else if active_item.downcast::<Editor>().is_some() {
        most_recently_active_item::<TerminalView>(workspace, cx)
    } else {
        None
    };
    if let Some(target) = target {
        workspace.activate_item(target.as_ref(), true, true, window, cx);
    }
}

/// Pane activation timestamps come from one counter shared by the workspace, so they order items across panes.
fn most_recently_active_item<T: 'static>(
    workspace: &Workspace,
    cx: &App,
) -> Option<Box<dyn ItemHandle>> {
    workspace
        .panes()
        .iter()
        .flat_map(|pane| {
            let pane = pane.read(cx);
            pane.activation_history().iter().filter_map(move |entry| {
                let item = pane
                    .items()
                    .find(|item| item.item_id() == entry.entity_id)?;
                item.downcast::<T>()?;
                Some((entry.timestamp, item.boxed_clone()))
            })
        })
        .max_by_key(|(timestamp, _)| *timestamp)
        .map(|(_, item)| item)
}

/// The note shown under the version when the About icon is clicked. Kept quiet on purpose.
pub fn about_icon_click_note(modifiers: &gpui::Modifiers) -> Option<&'static str> {
    (modifiers.alt && !modifiers.control && !modifiers.platform && !modifiers.shift)
        .then_some(photon_brand::DEDICATION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};

    #[gpui::test]
    fn offline_actions_hidden(cx: &mut TestAppContext) {
        cx.update(|cx| {
            command_palette_hooks::init(cx);
            hide_offline_actions(cx);
            let filter = CommandPaletteFilter::try_global(cx).expect("filter initialized");
            assert!(filter.is_hidden(&client::SignIn));
            assert!(filter.is_hidden(&auto_update::Check));
            assert!(filter.is_hidden(&zed_actions::OpenAccountSettings));
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

    #[gpui::test]
    async fn toggle_moves_focus_between_terminal_and_editor(cx: &mut TestAppContext) {
        use editor::Editor;
        use project::Project;
        use serde_json::json;
        use terminal::{
            TerminalBuilder,
            terminal_settings::{AlternateScroll, CursorShape},
        };
        use util::{path, paths::PathStyle};
        use workspace::{MultiWorkspace, SplitDirection, WorkspaceHandle as _};

        let app_state = super::super::tests::init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/root"), json!({ "notes.txt": "hello" }))
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);

        let toggle = |cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                toggle_terminal_editor_focus(workspace, &ToggleTerminalEditorFocus, window, cx)
            })
        };
        let focused_is = |cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                let pane = workspace.active_pane().clone();
                assert!(pane.focus_handle(cx).contains_focused(window, cx));
                let item = pane.read(cx).active_item().expect("active item");
                (
                    item.downcast::<TerminalView>().is_some(),
                    item.downcast::<Editor>().is_some(),
                )
            })
        };

        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        workspace.update_in(cx, |workspace, window, cx| {
            let terminal_view = cx.new(|cx| {
                TerminalView::new(
                    terminal,
                    workspace.weak_handle(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(terminal_view), None, true, window, cx);
        });

        // No editor yet: nothing happens and nothing is opened.
        toggle(cx);
        assert_eq!(focused_is(cx), (true, false));
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.panes().len()),
            1
        );

        let file = cx.read(|cx| workspace.file_project_paths(cx))[0].clone();
        let editor_pane = workspace.update_in(cx, |workspace, window, cx| {
            let terminal_pane = workspace.active_pane().clone();
            let editor_pane =
                workspace.split_pane(terminal_pane, SplitDirection::Right, window, cx);
            let open = workspace.open_path(file, Some(editor_pane.downgrade()), true, window, cx);
            (editor_pane, open)
        });
        editor_pane.1.await.unwrap();
        assert_eq!(focused_is(cx), (false, true));

        toggle(cx);
        assert_eq!(focused_is(cx), (true, false));
        toggle(cx);
        assert_eq!(focused_is(cx), (false, true));
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.panes().len()),
            2
        );
    }

    #[test]
    fn about_note_only_on_option_click() {
        assert_eq!(about_icon_click_note(&gpui::Modifiers::default()), None);
        assert_eq!(about_icon_click_note(&gpui::Modifiers::command()), None);
        assert_eq!(
            about_icon_click_note(&gpui::Modifiers::alt()),
            Some(photon_brand::DEDICATION)
        );
    }

    #[test]
    fn hack_font_is_bundled() {
        use gpui::AssetSource as _;
        let fonts = assets::Assets.list("fonts").expect("list fonts");
        for style in ["Regular", "Bold", "Italic", "BoldItalic"] {
            let path = format!("fonts/hack/Hack-{style}.ttf");
            assert!(fonts.iter().any(|font| font.as_ref() == path), "{path}");
        }
        // Prompt icons (starship, powerlevel10k) that Hack lacks; the terminal falls back to it.
        let symbols = "fonts/symbols-nerd-font/SymbolsNerdFontMono-Regular.ttf";
        assert!(
            fonts.iter().any(|font| font.as_ref() == symbols),
            "{symbols}"
        );
    }
}
