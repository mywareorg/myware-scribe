//! Design tokens and colour palettes for the Hippo Transcribe GUI.
//!
//! Every widget style resolves through [`Scheme`], so switching between the
//! light and dark palettes does not require touching individual widgets.

#![allow(dead_code)]

use iced::theme::Palette;
use iced::{Color, Theme};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Spacing scale (logical pixels).
pub const SPACE_2XS: f32 = 2.0;
pub const SPACE_XS: f32 = 4.0;
pub const SPACE_SM: f32 = 6.0;
pub const SPACE_MD: f32 = 8.0;
pub const SPACE_LG: f32 = 12.0;
pub const SPACE_XL: f32 = 16.0;
pub const SPACE_2XL: f32 = 24.0;

/// Corner radii.
pub const RADIUS_SM: f32 = 6.0;
pub const RADIUS_MD: f32 = 10.0;
pub const RADIUS_LG: f32 = 16.0;

/// Type scale.
pub const TEXT_XS: f32 = 12.0;
pub const TEXT_SM: f32 = 13.0;
pub const TEXT_MD: f32 = 14.0;
pub const TEXT_LG: f32 = 20.0;
pub const TEXT_XL: f32 = 24.0;

/// Blend a colour toward white.
pub fn lighten(color: Color, amount: f32) -> Color {
    let t = amount.clamp(0.0, 1.0);
    Color {
        r: color.r + (1.0 - color.r) * t,
        g: color.g + (1.0 - color.g) * t,
        b: color.b + (1.0 - color.b) * t,
        a: color.a,
    }
}

/// Blend a colour toward black.
pub fn darken(color: Color, amount: f32) -> Color {
    let t = amount.clamp(0.0, 1.0);
    Color {
        r: color.r * (1.0 - t),
        g: color.g * (1.0 - t),
        b: color.b * (1.0 - t),
        a: color.a,
    }
}

/// Linear blend between two colours.
pub fn mix(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color {
        r: from.r + (to.r - from.r) * t,
        g: from.g + (to.g - from.g) * t,
        b: from.b + (to.b - from.b) * t,
        a: from.a + (to.a - from.a) * t,
    }
}

/// The user-selectable appearance mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AppTheme {
    #[default]
    Light,
    Dark,
    System,
}

impl AppTheme {
    pub const ALL: [AppTheme; 3] = [AppTheme::Light, AppTheme::Dark, AppTheme::System];

    pub fn label(self) -> &'static str {
        match self {
            AppTheme::Light => "Light",
            AppTheme::Dark => "Dark",
            AppTheme::System => "System",
        }
    }

    /// Stable string used in `settings.json`.
    pub fn arg(self) -> &'static str {
        match self {
            AppTheme::Light => "light",
            AppTheme::Dark => "dark",
            AppTheme::System => "system",
        }
    }

    pub fn from_arg(value: &str) -> Option<Self> {
        match value {
            "light" => Some(AppTheme::Light),
            "dark" => Some(AppTheme::Dark),
            "system" => Some(AppTheme::System),
            _ => None,
        }
    }

    /// Cycle Light -> Dark -> System -> Light.
    pub fn toggled(self) -> Self {
        match self {
            AppTheme::Light => AppTheme::Dark,
            AppTheme::Dark => AppTheme::System,
            AppTheme::System => AppTheme::Light,
        }
    }

    fn system_is_dark() -> bool {
        matches!(Theme::default(), Theme::Dark)
    }

    pub fn scheme(self) -> Scheme {
        match self {
            AppTheme::Light => Scheme::light(),
            AppTheme::Dark => Scheme::dark(),
            AppTheme::System => {
                if Self::system_is_dark() {
                    Scheme::dark()
                } else {
                    Scheme::light()
                }
            }
        }
    }

    /// The iced theme handed to widgets that still use built-in styling.
    pub fn iced(self) -> Theme {
        let scheme = self.scheme();
        let name = match self {
            AppTheme::Light => "Hippo Transcribe Light",
            AppTheme::Dark => "Hippo Transcribe Dark",
            AppTheme::System => {
                if Self::system_is_dark() {
                    "Hippo Transcribe Dark"
                } else {
                    "Hippo Transcribe Light"
                }
            }
        };
        Theme::custom(
            name.to_string(),
            Palette {
                background: scheme.bg,
                text: scheme.text,
                primary: scheme.blue,
                success: scheme.green,
                danger: scheme.danger,
            },
        )
    }
}

impl fmt::Display for AppTheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Resolved colours for one appearance mode.
#[derive(Debug, Clone, Copy)]
pub struct Scheme {
    /// Window background.
    pub bg: Color,
    /// Raised content panels.
    pub panel_bg: Color,
    /// Modal / dialog surface.
    pub dialog_bg: Color,
    /// Inactive tracks (progress bars, dividers).
    pub track: Color,
    pub bar_track: Color,
    /// Filled portion of progress bars.
    pub progress: Color,
    pub timeline_bg: Color,
    /// Hairline borders and separators.
    pub border: Color,
    /// Trans-flag accents.
    pub blue: Color,
    pub pink: Color,
    /// Semantic colours (not flag accents).
    pub green: Color,
    pub danger: Color,
    /// Primary text.
    pub ink: Color,
    pub text: Color,
    /// Secondary text, captions, disabled controls.
    pub muted: Color,
    /// Text drawn on a filled accent button.
    pub on_accent: Color,
    /// Speaker palette (first four are the flag/neutral accents).
    pub speaker_colors: [Color; 10],
    pub sms0: Color,
    pub sms1: Color,
    /// Tints.
    pub warn_tint: Color,
    pub search_hl: Color,
    pub search_active: Color,
    pub tip_bg: Color,
    pub tip_text: Color,
    pub toast_bg: Color,
    pub toast_text: Color,
    /// Modal backdrop.
    pub scrim: Color,
}

impl Scheme {
    pub fn light() -> Scheme {
        Scheme {
            bg: Color::WHITE,
            panel_bg: Color::from_rgb(0.96, 0.97, 0.995),
            dialog_bg: Color::from_rgb(0.99, 0.99, 1.0),
            track: Color::from_rgb(0.88, 0.88, 0.93),
            bar_track: Color::from_rgb(0.88, 0.88, 0.93),
            progress: Color::from_rgb(0.357, 0.808, 0.980),
            timeline_bg: Color::from_rgb(0.88, 0.88, 0.93),
            border: Color::from_rgb(0.84, 0.85, 0.91),
            blue: Color::from_rgb(0.357, 0.808, 0.980),
            pink: Color::from_rgb(0.961, 0.663, 0.722),
            green: Color::from_rgb(0.30, 0.72, 0.42),
            danger: Color::from_rgb(0.85, 0.28, 0.38),
            ink: Color::from_rgb(0.13, 0.13, 0.17),
            text: Color::from_rgb(0.13, 0.13, 0.17),
            muted: Color::from_rgb(0.42, 0.42, 0.50),
            on_accent: Color::from_rgb(0.13, 0.13, 0.17),
            speaker_colors: [
                Color::from_rgb(0.357, 0.808, 0.980), // blue
                Color::from_rgb(0.961, 0.663, 0.722), // pink
                Color::from_rgb(0.66, 0.62, 0.98),    // violet
                Color::from_rgb(0.45, 0.82, 0.62),    // green
                Color::from_rgb(0.98, 0.78, 0.42),    // amber
                Color::from_rgb(0.55, 0.85, 0.88),    // teal
                Color::from_rgb(0.96, 0.58, 0.42),    // coral
                Color::from_rgb(0.58, 0.66, 0.98),    // periwinkle
                Color::from_rgb(0.78, 0.88, 0.48),    // lime
                Color::from_rgb(0.90, 0.70, 0.97),    // lavender
            ],
            sms0: Color::from_rgb(0.357, 0.808, 0.980),
            sms1: Color::from_rgb(0.961, 0.663, 0.722),
            warn_tint: Color::from_rgba(0.529, 0.867, 1.0, 0.35),
            search_hl: Color::from_rgba(1.0, 0.85, 0.3, 0.5),
            search_active: Color::from_rgba(1.0, 0.72, 0.2, 0.8),
            tip_bg: Color::from_rgb(0.99, 0.99, 1.0),
            tip_text: Color::from_rgb(0.13, 0.13, 0.17),
            toast_bg: Color::from_rgb(0.30, 0.72, 0.42),
            toast_text: Color::from_rgb(0.13, 0.13, 0.17),
            scrim: Color::from_rgba(0.0, 0.0, 0.0, 0.40),
        }
    }

    pub fn dark() -> Scheme {
        Scheme {
            bg: Color::from_rgb(0.043, 0.047, 0.055),
            panel_bg: Color::from_rgb(0.082, 0.090, 0.106),
            dialog_bg: Color::from_rgb(0.114, 0.124, 0.145),
            track: Color::from_rgb(0.165, 0.176, 0.200),
            bar_track: Color::from_rgb(0.165, 0.176, 0.200),
            progress: Color::from_rgb(0.286, 0.416, 0.529),
            timeline_bg: Color::from_rgb(0.106, 0.114, 0.129),
            border: Color::from_rgb(0.243, 0.255, 0.282),
            blue: Color::from_rgb(0.431, 0.686, 0.855),
            pink: Color::from_rgb(0.784, 0.573, 0.651),
            green: Color::from_rgb(0.302, 0.741, 0.475),
            danger: Color::from_rgb(0.898, 0.353, 0.396),
            ink: Color::from_rgb(0.890, 0.902, 0.925),
            text: Color::from_rgb(0.890, 0.902, 0.925),
            muted: Color::from_rgb(0.576, 0.600, 0.647),
            on_accent: Color::from_rgb(0.059, 0.071, 0.090),
            speaker_colors: [
                Color::from_rgb(0.184, 0.392, 0.478), // blue
                Color::from_rgb(0.463, 0.286, 0.357), // pink
                Color::from_rgb(0.345, 0.318, 0.541), // violet
                Color::from_rgb(0.263, 0.431, 0.333), // green
                Color::from_rgb(0.494, 0.412, 0.239), // amber
                Color::from_rgb(0.208, 0.416, 0.424), // teal
                Color::from_rgb(0.490, 0.322, 0.243), // coral
                Color::from_rgb(0.306, 0.365, 0.541), // periwinkle
                Color::from_rgb(0.384, 0.439, 0.251), // lime
                Color::from_rgb(0.427, 0.318, 0.478), // lavender
            ],
            sms0: Color::from_rgb(0.184, 0.392, 0.478),
            sms1: Color::from_rgb(0.463, 0.286, 0.357),
            warn_tint: Color::from_rgba(0.431, 0.686, 0.855, 0.18),
            search_hl: Color::from_rgba(0.890, 0.710, 0.353, 0.38),
            search_active: Color::from_rgba(0.890, 0.710, 0.353, 0.70),
            tip_bg: Color::from_rgb(0.114, 0.124, 0.145),
            tip_text: Color::from_rgb(0.890, 0.902, 0.925),
            toast_bg: Color::from_rgb(0.302, 0.741, 0.475),
            toast_text: Color::from_rgb(0.059, 0.071, 0.090),
            scrim: Color::from_rgba(0.0, 0.0, 0.0, 0.64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_theme_is_selectable_and_cycles() {
        assert_eq!(AppTheme::ALL.len(), 3);
        assert_eq!(AppTheme::from_arg("system"), Some(AppTheme::System));
        assert_eq!(AppTheme::System.label(), "System");
        assert_eq!(AppTheme::System.arg(), "system");

        assert_eq!(AppTheme::Light.toggled(), AppTheme::Dark);
        assert_eq!(AppTheme::Dark.toggled(), AppTheme::System);
        assert_eq!(AppTheme::System.toggled(), AppTheme::Light);
    }
}
