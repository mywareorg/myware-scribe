//! Design tokens and colour palettes for the Transiel GUI.
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
            AppTheme::Light => "Transiel Light",
            AppTheme::Dark => "Transiel Dark",
            AppTheme::System => {
                if Self::system_is_dark() {
                    "Transiel Dark"
                } else {
                    "Transiel Light"
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
            toast_bg: Color::from_rgb(0.357, 0.808, 0.980),
            toast_text: Color::from_rgb(0.13, 0.13, 0.17),
            scrim: Color::from_rgba(0.0, 0.0, 0.0, 0.40),
        }
    }

    pub fn dark() -> Scheme {
        Scheme {
            bg: Color::from_rgb(0.055, 0.063, 0.086),
            panel_bg: Color::from_rgb(0.090, 0.101, 0.141),
            dialog_bg: Color::from_rgb(0.114, 0.129, 0.180),
            track: Color::from_rgb(0.169, 0.188, 0.247),
            bar_track: Color::from_rgb(0.169, 0.188, 0.247),
            timeline_bg: Color::from_rgb(0.141, 0.161, 0.212),
            border: Color::from_rgb(0.220, 0.243, 0.310),
            blue: Color::from_rgb(0.357, 0.808, 0.980),
            pink: Color::from_rgb(0.961, 0.663, 0.722),
            green: Color::from_rgb(0.361, 0.780, 0.471),
            danger: Color::from_rgb(1.0, 0.451, 0.522),
            ink: Color::from_rgb(0.910, 0.922, 0.961),
            text: Color::from_rgb(0.910, 0.922, 0.961),
            muted: Color::from_rgb(0.600, 0.620, 0.700),
            on_accent: Color::from_rgb(0.078, 0.086, 0.118),
            speaker_colors: [
                Color::from_rgb(0.184, 0.435, 0.525), // blue
                Color::from_rgb(0.478, 0.267, 0.341), // pink
                Color::from_rgb(0.357, 0.310, 0.620), // violet
                Color::from_rgb(0.247, 0.478, 0.333), // green
                Color::from_rgb(0.541, 0.416, 0.184), // amber
                Color::from_rgb(0.184, 0.447, 0.478), // teal
                Color::from_rgb(0.541, 0.290, 0.200), // coral
                Color::from_rgb(0.290, 0.353, 0.620), // periwinkle
                Color::from_rgb(0.384, 0.459, 0.184), // lime
                Color::from_rgb(0.455, 0.290, 0.541), // lavender
            ],
            sms0: Color::from_rgb(0.184, 0.435, 0.525),
            sms1: Color::from_rgb(0.478, 0.267, 0.341),
            warn_tint: Color::from_rgba(0.357, 0.808, 0.980, 0.22),
            search_hl: Color::from_rgba(1.0, 0.78, 0.25, 0.35),
            search_active: Color::from_rgba(1.0, 0.72, 0.20, 0.65),
            tip_bg: Color::from_rgb(0.114, 0.129, 0.180),
            tip_text: Color::from_rgb(0.910, 0.922, 0.961),
            toast_bg: Color::from_rgb(0.125, 0.196, 0.239),
            toast_text: Color::from_rgb(0.910, 0.922, 0.961),
            scrim: Color::from_rgba(0.0, 0.0, 0.0, 0.62),
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
