use command_palette_hooks::CommandPaletteFilter;
use gpui::App;

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
    fn hack_font_is_bundled() {
        use gpui::AssetSource as _;
        let fonts = assets::Assets.list("fonts").expect("list fonts");
        for style in ["Regular", "Bold", "Italic", "BoldItalic"] {
            let path = format!("fonts/hack/Hack-{style}.ttf");
            assert!(fonts.iter().any(|font| font.as_ref() == path), "{path}");
        }
    }
}
