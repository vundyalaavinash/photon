use photon_brand::{ANSI_BRIGHT, ANSI_NORMAL, BHUMI, FOREGROUND, MAHA, contrast_ratio};
use serde_json::Value;

const ANSI_NAMES: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

fn photon_black() -> Value {
    let family: Value =
        serde_json_lenient::from_str(include_str!("../../../assets/themes/photon/photon.json"))
            .expect("photon.json parses");
    family["themes"]
        .as_array()
        .expect("themes array")
        .iter()
        .find(|theme| theme["name"] == "Photon Black")
        .expect("Photon Black theme")
        .clone()
}

/// `#rrggbb` or `#rrggbbaa` -> `0xrrggbb`, ignoring alpha.
fn hex(value: &Value) -> u32 {
    let text = value
        .as_str()
        .expect("color string")
        .trim_start_matches('#');
    u32::from_str_radix(&text[..6], 16).expect("hex color")
}

#[test]
fn photon_black_matches_brand() {
    let style = &photon_black()["style"];
    assert_eq!(hex(&style["background"]), BHUMI);
    assert_eq!(hex(&style["editor.background"]), BHUMI);
    assert_eq!(hex(&style["terminal.background"]), BHUMI);
    assert_eq!(hex(&style["text.accent"]), MAHA);
    assert_eq!(hex(&style["text"]), FOREGROUND);
    for (index, name) in ANSI_NAMES.iter().enumerate() {
        assert_eq!(
            hex(&style[format!("terminal.ansi.{name}")]),
            ANSI_NORMAL[index],
            "{name}"
        );
        assert_eq!(
            hex(&style[format!("terminal.ansi.bright_{name}")]),
            ANSI_BRIGHT[index],
            "bright_{name}"
        );
    }
}

#[test]
fn syntax_and_text_pass_aa() {
    let style = &photon_black()["style"];
    for (name, entry) in style["syntax"].as_object().expect("syntax map") {
        if let Some(color) = entry["color"].as_str() {
            let color = hex(&Value::from(color));
            assert!(
                contrast_ratio(color, BHUMI) >= 4.5,
                "syntax.{name} {color:06X}"
            );
        }
    }
    for name in ["text", "text.muted", "text.placeholder", "text.accent"] {
        let color = hex(&style[name]);
        assert!(contrast_ratio(color, BHUMI) >= 4.5, "{name} {color:06X}");
    }
}
