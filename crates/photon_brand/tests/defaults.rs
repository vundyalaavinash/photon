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

#[test]
fn warp_typography_defaults() {
    let settings = default_settings();
    assert_eq!(settings["theme"]["mode"], "dark");
    assert_eq!(settings["theme"]["dark"], "Photon Black");
    assert_eq!(settings["buffer_font_family"], "Hack");
    assert_eq!(settings["buffer_font_size"], 13);
    assert_eq!(settings["buffer_line_height"]["custom"], 1.2);
    assert_eq!(settings["cursor_shape"], "bar");
    assert_eq!(settings["ui_font_size"], 13);
    let terminal = &settings["terminal"];
    assert_eq!(terminal["font_family"], "Hack");
    assert_eq!(terminal["font_size"], 13);
    assert_eq!(terminal["line_height"]["custom"], 1.2);
    assert_eq!(terminal["cursor_shape"], "bar");
    assert_eq!(terminal["blinking"], "on");
}
