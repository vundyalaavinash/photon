use serde_json::Value;

fn default_settings() -> Value {
    serde_json_lenient::from_str(include_str!("../../../assets/settings/default.json"))
        .expect("default.json parses")
}

#[test]
fn offline_defaults() {
    let settings = default_settings();
    assert_eq!(settings["telemetry"]["diagnostics"], false);
    assert_eq!(settings["telemetry"]["metrics"], false);
    assert_eq!(settings["auto_update"], false);
    assert_eq!(settings["title_bar"]["show_sign_in"], false);
    assert_eq!(settings["title_bar"]["show_user_menu"], false);
    assert_eq!(settings["title_bar"]["show_user_picture"], false);
    assert_eq!(settings["collaboration_panel"]["button"], false);
    assert_eq!(settings["edit_predictions"]["provider"], "none");
}
