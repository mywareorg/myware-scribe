//! Widget styles, ported from the app so both share one set of surfaces.
//!
//! Everything here is message-agnostic: a style is a function from the resolved
//! iced theme and a widget status to a style value.

use iced::widget::{button, pick_list, scrollable, text_input};
use iced::{border, Color, Shadow, Theme, Vector};

use crate::theme::{self, Scheme, PILL};

/// Outline shared by every card ("island"): the same hairline in the dark
/// palette, a borderless rounded edge in the light one, where the shadow alone
/// separates the card from the window.
pub fn island_border(scheme: &Scheme) -> border::Border {
    if theme::is_dark_scheme(scheme) {
        border::Border {
            color: scheme.border,
            width: 1.0,
            radius: 16.0.into(),
        }
    } else {
        border::rounded(16.0)
    }
}

/// A subtle drop shadow that gives surfaces a soft, raised look.
pub fn soft_shadow() -> Shadow {
    Shadow {
        color: Color::from_rgba(0.0, 0.0, 0.0, 0.22),
        offset: Vector::new(0.0, 3.0),
        blur_radius: 9.0,
    }
}

/// A larger shadow for floating dialogs.
pub fn dialog_shadow() -> Shadow {
    Shadow {
        color: Color::from_rgba(0.0, 0.0, 0.0, 0.35),
        offset: Vector::new(0.0, 6.0),
        blur_radius: 24.0,
    }
}

/// A top-to-bottom gradient background.
pub fn linear_gradient(top: Color, bottom: Color) -> iced::Background {
    iced::Background::Gradient(iced::Gradient::Linear(
        iced::gradient::Linear::new(iced::Degrees(180.0))
            .add_stop(0.0, top)
            .add_stop(1.0, bottom),
    ))
}

/// The background colour of a style, when it is a plain colour.
pub fn style_background_color(style: &button::Style) -> Option<Color> {
    match &style.background {
        Some(iced::Background::Color(color)) => Some(*color),
        _ => None,
    }
}

/// Label colour for neutral (unfilled) buttons.
///
/// Dark mode gets pure white so the label reads at full strength against the
/// dark panels; light mode keeps the theme's primary text, since white on a
/// near-white panel would vanish. Filled accent buttons do not use this — they
/// keep `on_accent`, which is chosen for contrast against their own fill.
pub fn button_label(theme: &Theme) -> Color {
    if theme::is_dark(theme) {
        Color::WHITE
    } else {
        theme.palette().text
    }
}

/// The ordinary chrome button: a soft vertical gradient, a hairline rim and a
/// shadow that lifts on hover.
pub fn pill_button_secondary(theme: &Theme, status: button::Status) -> button::Style {
    let base = button::secondary(theme, status);
    let color = style_background_color(&base).unwrap_or(theme.palette().background);
    let (top, bottom, shadow) = match status {
        button::Status::Hovered => (
            theme::lighten(color, 0.16),
            theme::lighten(color, 0.02),
            soft_shadow(),
        ),
        button::Status::Pressed => (
            theme::darken(color, 0.12),
            theme::darken(color, 0.02),
            Shadow::default(),
        ),
        button::Status::Disabled => (color, color, Shadow::default()),
        button::Status::Active => (
            theme::lighten(color, 0.07),
            theme::darken(color, 0.05),
            soft_shadow(),
        ),
    };
    // Disabled keeps iced's dimmed label; only the live states go white in
    // dark mode, so a dead button never looks live.
    let text_color = match status {
        button::Status::Disabled => base.text_color,
        _ => button_label(theme),
    };
    button::Style {
        background: Some(linear_gradient(top, bottom)),
        text_color,
        border: border::Border {
            color: theme::lighten(color, 0.28),
            width: 1.0,
            radius: PILL.into(),
        },
        shadow,
    }
}

/// Selected toggle: a translucent accent fill, so it reads as a chosen state
/// rather than a second primary action.
pub fn pill_button_active(theme: &Theme, status: button::Status) -> button::Style {
    let primary = theme.palette().primary;
    let (alpha, border_alpha, shadow): (f32, f32, Shadow) = match status {
        button::Status::Hovered => (0.30, 0.65, soft_shadow()),
        button::Status::Pressed => (0.38, 0.70, Shadow::default()),
        button::Status::Disabled => (0.12, 0.30, Shadow::default()),
        button::Status::Active => (0.22, 0.55, soft_shadow()),
    };
    button::Style {
        background: Some(linear_gradient(
            Color {
                a: (alpha + 0.10).min(0.95),
                ..primary
            },
            Color {
                a: alpha,
                ..primary
            },
        )),
        text_color: match status {
            // A disabled tinted pill keeps the label it had, so the state still
            // reads as unavailable rather than switching to full-strength white.
            button::Status::Disabled => theme.palette().text,
            _ => button_label(theme),
        },
        border: border::Border {
            color: Color {
                a: border_alpha,
                ..primary
            },
            width: 1.0,
            radius: PILL.into(),
        },
        shadow,
    }
}

/// Selected toggle drawn as an opaque fill, so it reads as a pressed state
/// rather than a translucent tint over whatever is behind it.
pub fn pill_button_toggled(theme: &Theme, status: button::Status) -> button::Style {
    let base = button::secondary(theme, status);
    let color = style_background_color(&base).unwrap_or(theme.palette().background);
    // Drives the label and the fill tint alike: in dark mode the grey fill is
    // mixed toward white, so the pressed state stays visible.
    let text = button_label(theme);
    let (amount, border_alpha, shadow): (f32, f32, Shadow) = match status {
        button::Status::Hovered => (0.24, 0.55, soft_shadow()),
        button::Status::Pressed => (0.30, 0.65, Shadow::default()),
        button::Status::Disabled => (0.10, 0.25, Shadow::default()),
        button::Status::Active => (0.17, 0.45, soft_shadow()),
    };
    button::Style {
        background: Some(linear_gradient(
            theme::mix(color, text, (amount + 0.05).min(1.0)),
            theme::mix(color, text, amount),
        )),
        text_color: text,
        border: border::Border {
            color: Color {
                a: border_alpha,
                ..text
            },
            width: 1.0,
            radius: PILL.into(),
        },
        shadow,
    }
}

/// One half of a segmented toggle.
///
/// The selected half is a solid accent thumb with an `on_accent` label — the
/// same fill/label pairing as the filled primary buttons — plus a rim border and
/// [`soft_shadow`], so the current choice is unmistakably the accent one. The
/// other half stays a live-looking label (text at reduced alpha, not the muted
/// grey) so the control reads as two options. `muted` (locked / unavailable) is
/// never blue, so it cannot be mistaken for the selection.
pub fn segment_button(
    scheme: Scheme,
    selected: bool,
    muted: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let text = button_label(theme);
        // Unselected but selectable: full-strength text at reduced alpha. This
        // is the colour of a choice you can still make, as opposed to the muted
        // grey of one you cannot.
        let idle = Color { a: 0.75, ..text };

        let (background, color, border, shadow) = if muted {
            // Locked or unavailable: no accent at all. When it is nonetheless
            // the saved setting it keeps a faint neutral pill, so the state
            // still reads without looking like an active selection.
            let (fill, color) = if selected {
                (
                    Color { a: 0.08, ..text },
                    Color {
                        a: 0.60,
                        ..scheme.muted
                    },
                )
            } else {
                (Color::TRANSPARENT, scheme.muted)
            };
            (fill.into(), color, Color::TRANSPARENT, Shadow::default())
        } else if selected {
            // Solid accent fill. Hover and press only move the lightness, and
            // the label is `on_accent`: dark on the light-theme accent, light on
            // the dark-theme one, in both cases well above 7:1.
            let base = match status {
                button::Status::Hovered => theme::lighten(scheme.blue, 0.12),
                button::Status::Pressed => theme::darken(scheme.blue, 0.12),
                _ => scheme.blue,
            };
            let shadow = match status {
                button::Status::Pressed => Shadow::default(),
                _ => soft_shadow(),
            };
            (
                linear_gradient(theme::lighten(base, 0.08), theme::darken(base, 0.08)),
                scheme.on_accent,
                // Rim blended toward the label colour: a deeper edge on the
                // solid fill in both themes.
                theme::mix(scheme.blue, scheme.on_accent, 0.25),
                shadow,
            )
        } else {
            // Hover and press still read on the unselected side. `Disabled` here
            // only means "not clickable right now", not locked, so it dims but
            // stays a visible label.
            let (fill, color) = match status {
                button::Status::Hovered => (0.12, text),
                button::Status::Pressed => (0.18, text),
                button::Status::Disabled => (0.0, Color { a: 0.45, ..scheme.text }),
                button::Status::Active => (0.0, idle),
            };
            (
                Color { a: fill, ..text }.into(),
                color,
                Color::TRANSPARENT,
                Shadow::default(),
            )
        };
        button::Style {
            background: Some(background),
            text_color: color,
            border: border::Border {
                color: border,
                width: if selected { 1.0 } else { 0.0 },
                radius: PILL.into(),
            },
            shadow,
        }
    }
}

/// Verb-phrase action used beside a toggle, not as a third segment.
pub fn text_action_button(scheme: Scheme) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let label = button_label(theme);
        let (background, color) = match status {
            button::Status::Hovered => (Color { a: 0.08, ..label }, label),
            button::Status::Pressed => (Color { a: 0.16, ..label }, label),
            button::Status::Disabled => (
                Color::TRANSPARENT,
                Color {
                    a: 0.30,
                    ..scheme.text
                },
            ),
            _ => (Color::TRANSPARENT, label),
        };
        button::Style {
            background: Some(background.into()),
            text_color: color,
            border: border::rounded(6.0),
            shadow: Shadow::default(),
        }
    }
}

/// A quiet, borderless icon button for row-level actions.
pub fn subtle_icon_button(theme: &Theme, status: button::Status) -> button::Style {
    let text = theme.palette().text;
    let (background, text_color) = match status {
        button::Status::Hovered => (Color { a: 0.10, ..text }, text),
        button::Status::Pressed => (Color { a: 0.16, ..text }, text),
        button::Status::Disabled => (Color::TRANSPARENT, Color { a: 0.30, ..text }),
        button::Status::Active => (Color::TRANSPARENT, Color { a: 0.65, ..text }),
    };
    button::Style {
        background: Some(background.into()),
        text_color,
        border: border::rounded(6.0),
        shadow: Shadow::default(),
    }
}

/// Greyed, but still pressable: a locked feature that answers with a message
/// rather than doing nothing.
pub fn greyed_button(scheme: Scheme) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut style = button::secondary(theme, status);
        style.background = Some(
            Color {
                a: 0.12,
                ..scheme.muted
            }
            .into(),
        );
        style.text_color = scheme.muted;
        style.border.radius = PILL.into();
        style
    }
}

/// A circular icon control: the live pill, or the greyed one when the action
/// would refuse. One style function for both states keeps a row's buttons a
/// single type, and the radius is pinned to half the diameter so hover, pressed
/// and disabled all draw a circle rather than a pill whose ends depend on the
/// box's height.
pub fn panel_icon_button(
    scheme: Scheme,
    enabled: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut style = if enabled {
            pill_button_secondary(theme, status)
        } else {
            greyed_button(scheme)(theme, status)
        };
        style.border.radius = (theme::PANEL_ICON_DIAMETER / 2.0).into();
        style
    }
}

/// The accent fill with the card hairline around it.
///
/// [`color_button`] outlines a filled button with a lighter tint of its own hue,
/// which on a dark window leaves filled actions looking flush with the panel. In
/// dark mode this wraps them in the same border every card uses.
pub fn color_button_outlined(
    bg: Color,
    ink: Color,
    scheme: Scheme,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut style = color_button(bg, ink)(theme, status);
        if theme::is_dark_scheme(&scheme) {
            style.border.color = scheme.border;
        }
        style
    }
}

/// A filled accent button: gradient fill, a rim in a lighter tint of the same
/// hue, and a stronger shadow on hover.
pub fn color_button(bg: Color, ink: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let (top, bottom, text, border, border_width, shadow) = match status {
            button::Status::Hovered => (
                theme::lighten(bg, 0.30),
                theme::lighten(bg, 0.10),
                ink,
                Color {
                    a: 0.95,
                    ..theme::lighten(bg, 0.78)
                },
                2.0,
                Shadow {
                    color: Color::from_rgba(0.0, 0.0, 0.0, 0.30),
                    offset: Vector::new(0.0, 4.0),
                    blur_radius: 12.0,
                },
            ),
            button::Status::Pressed => (
                theme::darken(bg, 0.18),
                theme::darken(bg, 0.06),
                ink,
                Color {
                    a: 0.80,
                    ..theme::lighten(bg, 0.65)
                },
                2.0,
                Shadow::default(),
            ),
            // Filled buttons keep their hue when disabled, but they must read as
            // unavailable: a dimmer fill and grey text rather than the full
            // strength label.
            button::Status::Disabled => {
                let faded = Color { a: 0.26, ..bg };
                (
                    faded,
                    faded,
                    Color {
                        a: 0.55,
                        ..theme.palette().text
                    },
                    Color { a: 0.14, ..bg },
                    1.0,
                    Shadow::default(),
                )
            }
            _ => (
                theme::lighten(bg, 0.12),
                theme::darken(bg, 0.04),
                ink,
                Color {
                    a: 0.45,
                    ..theme::lighten(bg, 0.55)
                },
                1.0,
                soft_shadow(),
            ),
        };
        button::Style {
            background: Some(linear_gradient(top, bottom)),
            text_color: text,
            border: border::Border {
                color: border,
                width: border_width,
                radius: PILL.into(),
            },
            shadow,
        }
    }
}

/// A pill-shaped text input, matching the pill buttons beside it.
pub fn pill_input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let mut style = text_input::default(theme, status);
    style.border.radius = PILL.into();
    style
}

/// The wash under the row the pointer is on, for every menu.
///
/// The band is the surface mixed away from itself rather than a fixed token:
/// `track` equals `dialog_bg` in the dark palette (both Gray 80), which makes a
/// `track`-based hover invisible there, and a recessed surface is only about
/// 1.3:1 against the same surface. These mixes give roughly 1.6:1 in light and
/// 1.75:1 in dark — a clear row under the pointer that is still a wash and not a
/// filled button.
pub fn hover_band(scheme: &Scheme) -> Color {
    if theme::is_dark_scheme(scheme) {
        theme::lighten(scheme.dialog_bg, 0.18)
    } else {
        theme::darken(scheme.dialog_bg, 0.20)
    }
}

/// The surface every pick-list menu is drawn on.
///
/// iced's default menu is a square popup filled with the window colour and edged
/// with a `background.strong` hairline, which floats with no elevation cue and
/// matches none of a panel-based app's surfaces. `menu::Style` has no shadow
/// field, so the fill and the hairline *are* the elevation: this puts it on the
/// raised dialog surface with the card radius, and replaces the row highlight.
///
/// iced has one index for the row under the pointer and for the row that is the
/// current value (`hovered_option`, seeded from the selection when the menu
/// opens), so a single `selected_background` paints both and they cannot be told
/// apart by colour. The band is therefore the same wash the header menus use, and
/// the chosen value stays legible in the closed control above the menu.
pub fn pick_menu_style(scheme: Scheme) -> impl Fn(&Theme) -> iced::overlay::menu::Style {
    move |_theme| iced::overlay::menu::Style {
        background: scheme.dialog_bg.into(),
        border: border::Border {
            color: scheme.border,
            width: 1.0,
            radius: theme::RADIUS_MD.into(),
        },
        text_color: scheme.text,
        selected_text_color: scheme.text,
        selected_background: hover_band(&scheme).into(),
    }
}

/// A pill-shaped pick list.
pub fn pill_pick(scheme: Scheme) -> impl Fn(&Theme, pick_list::Status) -> pick_list::Style {
    move |theme, status| {
        let mut style = pick_list::default(theme, status);
        style.border.radius = PILL.into();
        // Open: the handle takes the menu's surface, so the list reads as this
        // control opening rather than as a popup landing under a pill. iced's own
        // open state only tints the border with the accent, which would draw a
        // blue edge above the menu's grey hairline.
        if matches!(status, pick_list::Status::Opened) {
            style.background = scheme.dialog_bg.into();
            style.border.color = scheme.border;
        }
        style
    }
}

/// Borderless pick list whose colours mirror the secondary button.
pub fn speed_pick(scheme: Scheme) -> impl Fn(&Theme, pick_list::Status) -> pick_list::Style {
    move |theme, status| {
        let button_status = match status {
            pick_list::Status::Hovered | pick_list::Status::Opened => button::Status::Hovered,
            pick_list::Status::Active => button::Status::Active,
        };
        let base = button::secondary(theme, button_status);
        let text = button_label(theme);
        pick_list::Style {
            text_color: text,
            placeholder_color: text,
            handle_color: text,
            // Open: the same surface as its list, like the pill handles. The
            // border stays transparent — this idiom has no chrome when closed, and
            // growing one only while open would be a new shape rather than the
            // same control opening.
            background: if matches!(status, pick_list::Status::Opened) {
                scheme.dialog_bg.into()
            } else {
                base.background
                    .unwrap_or_else(|| Color::TRANSPARENT.into())
            },
            border: border::Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: PILL.into(),
            },
        }
    }
}

/// A pick list that reads as unavailable without losing its shape.
pub fn greyed_pick(scheme: Scheme) -> impl Fn(&Theme, pick_list::Status) -> pick_list::Style {
    move |theme, status| {
        let mut style = pick_list::default(theme, status);
        style.text_color = scheme.muted;
        style.handle_color = scheme.muted;
        style.background = scheme.track.into();
        style.border = border::Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: PILL.into(),
        };
        style
    }
}

/// Scrollbar that is transparent until the pointer is on it.
pub fn scrollbar_style(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = scrollable::default(theme, status);
    let visible = matches!(
        status,
        scrollable::Status::Hovered { .. } | scrollable::Status::Dragged { .. }
    );
    let ink = theme.palette().text;
    style.vertical_rail.scroller.color = if visible {
        Color { a: 0.5, ..ink }
    } else {
        Color::TRANSPARENT
    };
    style
}

/// Always-visible scrollbar, for lists and any dialog that scrolls.
///
/// A dialog's scroller is its only cue that more rows exist below the fold, and
/// a rail that only appears on hover reads as an empty gutter until then.
pub fn visible_scrollbar_style(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = scrollable::default(theme, status);
    let active = matches!(
        status,
        scrollable::Status::Hovered { .. } | scrollable::Status::Dragged { .. }
    );
    let ink = theme.palette().text;
    style.vertical_rail.scroller.color = if active {
        Color { a: 0.55, ..ink }
    } else {
        Color { a: 0.28, ..ink }
    };
    style
}

/// The line box of an icon glyph, tightened to its own em size.
///
/// This icon font is 300/300 em: ascent exactly one em, descent zero, and its
/// glyph inks are symmetric about the em box's centre. A line box the size of the
/// glyph therefore centres the ink, which is what lets a circle centre it by
/// layout.
pub fn icon_line_height(size: f32) -> iced::advanced::text::LineHeight {
    iced::advanced::text::LineHeight::Absolute(iced::Pixels(size))
}

/// Rough advance width of a run of text at `size`: 10 px per character at iced's
/// default 16 px UI size, scaled for a smaller size. A menu or button is built
/// before any glyph is measured and its labels never wrap, so the estimate is
/// deliberately generous.
pub fn estimated_text_width(text: &str, size: f32) -> f32 {
    text.chars().count() as f32 * size * 0.625
}

/// Fixed width for a text button so its label can never wrap. The default 16 px
/// UI font averages close to 10 px per character for mixed-case Latin text, and
/// the button adds 10 px of padding on each side.
pub fn action_width(label: &str) -> f32 {
    (label.chars().count() as f32 * 10.0 + 28.0).max(44.0)
}

/// UI chrome never wraps; labels are shortened or ellipsized instead.
pub const NO_WRAP: iced::widget::text::Wrapping = iced::widget::text::Wrapping::None;

/// Width of a menu row's leading check slot. Fixed, so every label in a menu
/// starts on the same left edge whether its row is checked or not.
pub const MENU_CHECK_WIDTH: f32 = 16.0;

/// Bootstrap Icons codepoint for a check mark.
pub const ICON_CHECK: char = '\u{f633}';

/// A menu row: no chrome until hovered. A disabled row still tints when the
/// pointer is over it — a menu should read as one live surface — but with a
/// weaker wash, because nothing about it is on offer; its label stays muted.
pub fn menu_item_style(
    scheme: Scheme,
    enabled: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hot = matches!(
            status,
            button::Status::Hovered | button::Status::Pressed
        );
        let tint = if enabled {
            hover_band(&scheme)
        } else {
            Color {
                a: 0.45,
                ..hover_band(&scheme)
            }
        };
        button::Style {
            background: hot.then(|| tint.into()),
            text_color: if enabled { scheme.ink } else { scheme.muted },
            border: border::Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: theme::RADIUS_SM.into(),
            },
            shadow: Shadow::default(),
        }
    }
}
