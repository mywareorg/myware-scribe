//! Design tokens and colour palettes.
//!
//! Every widget style in the kit resolves through [`Scheme`], so switching
//! between the light and dark palettes does not require touching individual
//! widgets. The palettes are IBM Carbon-based: one neutral ramp, one blue
//! accent and semantic support colours, which keeps panels, fields and accents
//! from competing with each other.

use iced::theme::Palette;
use iced::{Color, Theme};

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

/// Padding of a compact icon button, in logical pixels. Icon buttons are sized
/// by this padding rather than by a fixed height: `Button::height(Fixed(h))`
/// fixes the box *inside* the padding, so 24 px of height with 4 px of vertical
/// padding leaves 16 px for a glyph whose line box is 18.2 px (a 14 px icon at
/// iced's 1.3 line height) and the glyph gets cut.
pub const CONTROL_PADDING: [u16; 2] = [3, 5];

/// Type scale.
pub const TEXT_XS: f32 = 12.0;
pub const TEXT_SM: f32 = 13.0;
pub const TEXT_MD: f32 = 14.0;
pub const TEXT_LG: f32 = 20.0;
pub const TEXT_XL: f32 = 24.0;

/// Height every standard control takes: a 16 px label (20.8 px line box) plus the
/// 5 px of vertical padding every text button uses. Icon-only controls take the
/// same height so a row that mixes the two is one control height rather than as
/// tall as its tallest child.
pub const CONTROL_HEIGHT: f32 = 30.8;

/// Diameter of a circular icon button: a standard control height, so the round
/// controls line up with the text buttons around them.
pub const PANEL_ICON_DIAMETER: f32 = CONTROL_HEIGHT;

/// Glyph size for a circular icon control.
pub const ICON_ONLY_SIZE: f32 = 15.0;

/// Glyph size for an icon that sits in a compact icon box.
pub const ICON_SIZE: f32 = 14.0;

/// Padding inside a modal card.
pub const MODAL_PADDING: f32 = 16.0;

/// Fully rounded ends for pills.
pub const PILL: f32 = 1000.0;

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

fn channel(value: f32) -> f32 {
    if value <= 0.039_28 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn relative_luminance(color: Color) -> f32 {
    0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}

/// Whether a resolved [`Scheme`] is the dark one. Uses WCAG relative luminance
/// on the window background.
pub fn is_dark_scheme(scheme: &Scheme) -> bool {
    relative_luminance(scheme.bg) < 0.5
}

/// Whether the palette a widget is actually rendering with reads as dark.
///
/// Resolved from the theme's own background, so a "follow the system" mode
/// follows whatever appearance iced was given rather than a stored preference.
pub fn is_dark(theme: &Theme) -> bool {
    relative_luminance(theme.palette().background) < 0.5
}

/// The user-selectable appearance mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppTheme {
    Light,
    Dark,
    /// Follows the OS appearance.
    #[default]
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

    /// Stable string for a settings file.
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
            AppTheme::Light => "ui-kit Light",
            AppTheme::Dark => "ui-kit Dark",
            AppTheme::System => {
                if Self::system_is_dark() {
                    "ui-kit Dark"
                } else {
                    "ui-kit Light"
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

impl core::fmt::Display for AppTheme {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
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
    /// Recessed surface drawn inside a panel.
    pub timeline_bg: Color,
    /// Editable field well: a surface that reads as an input against the panel
    /// in both palettes, not as plain white.
    pub field_bg: Color,
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
            bg: Color::from_rgb(0.937, 0.941, 0.953), // Gray 10 #EFF0F3
            panel_bg: Color::from_rgb(0.988, 0.988, 0.996), // raised surface
            dialog_bg: Color::WHITE,
            track: Color::from_rgb(0.886, 0.894, 0.918),
            bar_track: Color::from_rgb(0.886, 0.894, 0.918),
            progress: Color::from_rgb(0.357, 0.808, 0.980),
            timeline_bg: Color::from_rgb(0.878, 0.886, 0.910),
            field_bg: Color::from_rgb(0.918, 0.925, 0.945), // input well
            border: Color::from_rgb(0.827, 0.839, 0.878),
            blue: Color::from_rgb(0.357, 0.808, 0.980),
            pink: Color::from_rgb(0.961, 0.663, 0.722),
            green: Color::from_rgb(0.30, 0.72, 0.42),
            danger: Color::from_rgb(0.85, 0.28, 0.38),
            ink: Color::from_rgb(0.13, 0.13, 0.17),
            text: Color::from_rgb(0.13, 0.13, 0.17),
            muted: Color::from_rgb(0.42, 0.42, 0.50),
            on_accent: Color::from_rgb(0.13, 0.13, 0.17),
            speaker_colors: [
                Color::from_rgb(96.0 / 255.0, 165.0 / 255.0, 250.0 / 255.0), // blue 400
                Color::from_rgb(251.0 / 255.0, 113.0 / 255.0, 133.0 / 255.0), // rose 400
                Color::from_rgb(251.0 / 255.0, 146.0 / 255.0, 60.0 / 255.0), // orange 400
                Color::from_rgb(251.0 / 255.0, 191.0 / 255.0, 36.0 / 255.0), // amber 400
                Color::from_rgb(163.0 / 255.0, 230.0 / 255.0, 53.0 / 255.0), // lime 400
                Color::from_rgb(52.0 / 255.0, 211.0 / 255.0, 153.0 / 255.0), // emerald 400
                Color::from_rgb(45.0 / 255.0, 212.0 / 255.0, 191.0 / 255.0), // teal 400
                Color::from_rgb(34.0 / 255.0, 211.0 / 255.0, 238.0 / 255.0), // cyan 400
                Color::from_rgb(167.0 / 255.0, 139.0 / 255.0, 250.0 / 255.0), // violet 400
                Color::from_rgb(232.0 / 255.0, 121.0 / 255.0, 249.0 / 255.0), // fuchsia 400
            ],
            sms0: Color::from_rgb(96.0 / 255.0, 165.0 / 255.0, 250.0 / 255.0),
            sms1: Color::from_rgb(251.0 / 255.0, 113.0 / 255.0, 133.0 / 255.0),
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
            bg: Color::from_rgb(0.086, 0.086, 0.086), // Gray 100 #161616
            panel_bg: Color::from_rgb(0.149, 0.149, 0.149), // Gray 90  #262626
            dialog_bg: Color::from_rgb(0.224, 0.224, 0.224), // Gray 80  #393939
            track: Color::from_rgb(0.224, 0.224, 0.224), // Gray 80
            bar_track: Color::from_rgb(0.224, 0.224, 0.224), // Gray 80
            progress: Color::from_rgb(0.471, 0.663, 1.0), // Blue 40  #78a9ff
            timeline_bg: Color::from_rgb(0.149, 0.149, 0.149), // Gray 90
            field_bg: Color::from_rgb(0.224, 0.224, 0.224), // Gray 80
            border: Color::from_rgb(0.322, 0.322, 0.322), // Gray 70  #525252
            blue: Color::from_rgb(0.471, 0.663, 1.0), // Blue 40
            pink: Color::from_rgb(1.0, 0.494, 0.714), // Magenta 40
            green: Color::from_rgb(0.259, 0.745, 0.396), // Green 40 #42be65
            danger: Color::from_rgb(0.980, 0.302, 0.337), // Red 50   #fa4d56
            ink: Color::from_rgb(0.957, 0.957, 0.957), // Gray 10  #f4f4f4
            text: Color::from_rgb(0.957, 0.957, 0.957), // Gray 10
            muted: Color::from_rgb(0.659, 0.659, 0.659), // Gray 40  #a8a8a8
            on_accent: Color::from_rgb(0.086, 0.086, 0.086), // Gray 100
            speaker_colors: [
                Color::from_rgb(29.0 / 255.0, 78.0 / 255.0, 216.0 / 255.0), // blue 700
                Color::from_rgb(190.0 / 255.0, 18.0 / 255.0, 60.0 / 255.0), // rose 700
                Color::from_rgb(194.0 / 255.0, 65.0 / 255.0, 12.0 / 255.0), // orange 700
                Color::from_rgb(146.0 / 255.0, 64.0 / 255.0, 14.0 / 255.0), // amber 800
                Color::from_rgb(63.0 / 255.0, 98.0 / 255.0, 18.0 / 255.0),  // lime 800
                Color::from_rgb(4.0 / 255.0, 120.0 / 255.0, 87.0 / 255.0),  // emerald 700
                Color::from_rgb(15.0 / 255.0, 118.0 / 255.0, 110.0 / 255.0), // teal 700
                Color::from_rgb(14.0 / 255.0, 116.0 / 255.0, 144.0 / 255.0), // cyan 700
                Color::from_rgb(109.0 / 255.0, 40.0 / 255.0, 217.0 / 255.0), // violet 700
                Color::from_rgb(162.0 / 255.0, 28.0 / 255.0, 175.0 / 255.0), // fuchsia 700
            ],
            sms0: Color::from_rgb(29.0 / 255.0, 78.0 / 255.0, 216.0 / 255.0),
            sms1: Color::from_rgb(190.0 / 255.0, 18.0 / 255.0, 60.0 / 255.0),
            warn_tint: Color::from_rgba(0.471, 0.663, 1.0, 0.18),
            search_hl: Color::from_rgba(0.945, 0.761, 0.106, 0.35),
            search_active: Color::from_rgba(0.945, 0.761, 0.106, 0.65),
            tip_bg: Color::from_rgb(0.224, 0.224, 0.224), // Gray 80
            tip_text: Color::from_rgb(0.957, 0.957, 0.957),
            toast_bg: Color::from_rgb(0.098, 0.502, 0.220), // Green 50
            toast_text: Color::from_rgb(0.957, 0.957, 0.957),
            scrim: Color::from_rgba(0.0, 0.0, 0.0, 0.64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_are_distinct_and_luminance_agrees() {
        assert!(is_dark_scheme(&Scheme::dark()));
        assert!(!is_dark_scheme(&Scheme::light()));
        assert!(is_dark(&AppTheme::Dark.iced()));
        assert!(!is_dark(&AppTheme::Light.iced()));
    }

    #[test]
    fn the_appearance_modes_round_trip_through_their_stable_names() {
        for mode in AppTheme::ALL {
            assert_eq!(AppTheme::from_arg(mode.arg()), Some(mode));
        }
    }
}
