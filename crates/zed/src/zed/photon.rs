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
}
