pub const DEDICATION: &str = "Dedicated to Maha & Bhumi.";

/// Base background (Warp Dark). Bhumi: the ground everything rests on.
pub const BHUMI: u32 = 0x050505;
/// Accent (Warp Dark). Maha: the color that stands out.
pub const MAHA: u32 = 0x19AAD8;
pub const FOREGROUND: u32 = 0xFFFFFF;
/// Brighter than Warp's ANSI black, which fails WCAG AA as text on `BHUMI`.
pub const COMMENT: u32 = 0x8E8E8E;

/// Black, red, green, yellow, blue, magenta, cyan, white.
pub const ANSI_NORMAL: [u32; 8] = [
    0x616161, 0xFF8272, 0xB4FA72, 0xFEFDC2, 0xA5D5FE, 0xFF8FFD, 0xD0D1FE, 0xF1F1F1,
];
pub const ANSI_BRIGHT: [u32; 8] = [
    0x8E8E8E, 0xFFC4BD, 0xD6FCB9, 0xFEFDD5, 0xC1E3FE, 0xFFB1FE, 0xE5E6FE, 0xFEFFFF,
];

/// WCAG 2.x contrast ratio between two opaque `0xRRGGBB` colors.
pub fn contrast_ratio(foreground: u32, background: u32) -> f32 {
    let (lighter, darker) = {
        let first = relative_luminance(foreground);
        let second = relative_luminance(background);
        (first.max(second), first.min(second))
    };
    (lighter + 0.05) / (darker + 0.05)
}

fn relative_luminance(color: u32) -> f32 {
    let channel = |shift: u32| {
        let value = ((color >> shift) & 0xFF) as f32 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_matches_wcag_reference() {
        assert!((contrast_ratio(0xFFFFFF, 0x000000) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(0x616161, BHUMI) - 3.3).abs() < 0.05);
        assert!((contrast_ratio(MAHA, BHUMI) - 7.6).abs() < 0.05);
    }

    #[test]
    fn text_colors_pass_aa_on_bhumi() {
        // ANSI_NORMAL[0] is ANSI black: a background color, never text.
        for color in [FOREGROUND, MAHA, COMMENT]
            .iter()
            .chain(&ANSI_NORMAL[1..])
            .chain(&ANSI_BRIGHT)
        {
            assert!(contrast_ratio(*color, BHUMI) >= 4.5, "{color:06X}");
        }
    }
}
