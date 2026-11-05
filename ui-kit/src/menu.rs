//! Menus: rows, sections, the card they are drawn in, and the pick-list chrome.
//!
//! A menu is a stack layer rather than a widget of its own: the host places
//! [`menu_card`] in a layer over its content and gives it an offset. Everything in
//! this module is about the card's contents and look, not about where it hangs.

use iced::widget::{column, container, horizontal_space, mouse_area, pick_list};
use iced::{border, mouse, Element, Font, Length};

use crate::button::{Btn, ICON_FAMILY};
use crate::styles;
use crate::theme::{self, Scheme};

const ICON_CARET_DOWN: char = '\u{f229}';
const ICON_CARET_UP: char = '\u{f235}';

/// One row of a menu.
pub struct MenuRow<'a, M> {
    pub label: &'a str,
    /// The key that toggles the row, shown at the far right.
    pub key: Option<&'a str>,
    pub checked: bool,
    pub enabled: bool,
    pub action: Option<M>,
}

impl<'a, M> MenuRow<'a, M> {
    pub fn new(label: &'a str) -> Self {
        Self {
            label,
            key: None,
            checked: false,
            enabled: true,
            action: None,
        }
    }

    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn key(mut self, key: &'a str) -> Self {
        self.key = Some(key);
        self
    }

    pub fn on_press(mut self, message: M) -> Self {
        self.action = Some(message);
        self
    }

    pub fn on_press_maybe(mut self, message: Option<M>) -> Self {
        self.action = message;
        self
    }
}

/// A group of rows, optionally followed by custom controls (a zoom pair, for
/// instance). Groups are separated by a hairline.
pub struct MenuSection<'a, M> {
    pub rows: Vec<MenuRow<'a, M>>,
    pub footer: Option<Element<'a, M>>,
}

impl<'a, M> MenuSection<'a, M> {
    pub fn rows(rows: impl IntoIterator<Item = MenuRow<'a, M>>) -> Self {
        Self {
            rows: rows.into_iter().collect(),
            footer: None,
        }
    }

    pub fn footer(mut self, footer: impl Into<Element<'a, M>>) -> Self {
        self.footer = Some(footer.into());
        self
    }
}

/// Width a menu needs: the widest of its rows, each being the check slot and its
/// gap, the label, the minimum gap before a trailing key and the key, plus the
/// row's and the card's padding.
///
/// The rows are the same values the body is built from, so the card is as wide as
/// its widest label and no wider. Rows keep their `Fill` width inside it, so every
/// row — and its hover tint — spans the card and the trailing keys line up.
pub fn menu_width<'r, 'a: 'r, M: 'r>(
    rows: impl IntoIterator<Item = &'r MenuRow<'a, M>>,
) -> f32 {
    let row_width = |label: &str, key: Option<&str>| {
        theme::SPACE_MD * 2.0                       // card padding, both sides
            + theme::SPACE_SM * 2.0                 // row padding `[5, 6]`, both sides
            + styles::MENU_CHECK_WIDTH
            + theme::SPACE_SM                       // check slot to label
            + styles::estimated_text_width(label, 16.0)
            + key.map_or(0.0, |key| {
                theme::SPACE_2XL + styles::estimated_text_width(key, theme::TEXT_XS)
            })
    };
    rows.into_iter()
        .map(|row| row_width(row.label, row.key))
        .fold(0.0, f32::max)
}

/// A hairline between two menu groups.
pub fn menu_divider<M: Clone + 'static>(scheme: Scheme) -> Element<'static, M> {
    container(
        container(horizontal_space())
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_theme| container::Style {
                background: Some(scheme.border.into()),
                ..container::Style::default()
            }),
    )
    .padding([4, 0])
    .into()
}

/// The card a menu is drawn in: raised, hairline-outlined and sized to its rows.
///
/// Clicks inside it, including its padding, are consumed so only a press outside
/// dismisses the menu. `noop` is the message a disabled row answers with: iced
/// only reports `Hovered` for a button that has a press handler, and every row of
/// a menu should wash under the pointer, including the ones that do nothing.
pub fn menu_card<'a, M: Clone + 'static>(
    sections: Vec<MenuSection<'a, M>>,
    scheme: Scheme,
    noop: M,
) -> Element<'a, M> {
    let width = menu_width(sections.iter().flat_map(|section| section.rows.iter()));
    let mut body = column![].spacing(theme::SPACE_SM).width(Length::Fill);
    for (index, section) in sections.into_iter().enumerate() {
        if index > 0 {
            body = body.push(menu_divider(scheme));
        }
        let mut rows = column![].spacing(2).width(Length::Fill);
        for row in section.rows {
            let action = if row.enabled {
                row.action.unwrap_or_else(|| noop.clone())
            } else {
                noop.clone()
            };
            rows = rows.push(
                Btn::menu_row(scheme, row.label, row.checked, row.key)
                    .enabled(row.enabled)
                    .on_press(action)
                    .build(),
            );
        }
        body = body.push(rows);
        if let Some(footer) = section.footer {
            body = body.push(footer);
        }
    }

    let card = container(body)
        .width(Length::Fixed(width))
        .padding(theme::SPACE_MD)
        .style(move |_theme| container::Style {
            background: Some(scheme.dialog_bg.into()),
            border: border::Border {
                color: scheme.border,
                width: 1.0,
                radius: theme::RADIUS_MD.into(),
            },
            shadow: styles::soft_shadow(),
            ..container::Style::default()
        });
    mouse_area(card)
        .interaction(mouse::Interaction::Idle)
        .into()
}

/// The caret every pick list draws in its handle: a downward caret when the list
/// is closed and an upward one when it is open.
///
/// The flip is a state cue, not a direction: Material and Fluent/WinUI both rotate
/// the chevron 180 degrees on expand, and neither points it at the side the list
/// appears on. iced chooses whether the menu opens above or below and never
/// reports it, so a caret that flips can only express state.
pub fn caret_handle() -> pick_list::Handle<Font> {
    let icon = |code: char| pick_list::Icon {
        font: Font::with_name(ICON_FAMILY),
        code_point: code,
        size: Some(iced::Pixels(12.0)),
        line_height: iced::advanced::text::LineHeight::default(),
        shaping: iced::advanced::text::Shaping::Basic,
    };
    pick_list::Handle::Dynamic {
        closed: icon(ICON_CARET_DOWN),
        open: icon(ICON_CARET_UP),
    }
}
