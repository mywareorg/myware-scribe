//! The one button.
//!
//! The kind picks the look, the size picks the box, and the constructor picks the
//! content. A call site cannot set padding, a label size or a glyph size: those
//! are the metrics that drifted between buttons when every site built its own
//! `button(..)`. The content is centred by layout — a fixed-height container with
//! `center_y(Fill)` — rather than by a padding that has to add up, which is what
//! makes "the text is not centred" impossible to reintroduce.

use iced::widget::{button, container, horizontal_space, row, text};
use iced::{Color, Element, Font, Length, Theme};

use crate::styles;
use crate::theme::{self, Scheme, CONTROL_HEIGHT, CONTROL_PADDING, ICON_ONLY_SIZE, ICON_SIZE};

/// Family name of the icon font. The host must load a font under this name.
pub const ICON_FAMILY: &str = "bootstrap-icons";

/// Type size of every button label, anywhere.
pub const LABEL_SIZE: f32 = 16.0;

/// The shorter icon box: the width the compact icon rows have always used —
/// a 14 px glyph's line box plus the icon padding.
pub const COMPACT_HEIGHT: f32 = ICON_SIZE * 1.3 + 2.0 * CONTROL_PADDING[0] as f32;

/// The look a button wears. Closed on purpose: a new look is a decision made
/// here, not a style closure at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtnKind {
    /// Filled accent: the one action a view is asking for.
    Primary,
    /// Filled success colour: start a run.
    Success,
    /// Filled danger colour: destroy something.
    Danger,
    /// Outlined success fill: a primary action that must not shout.
    Outlined,
    /// The ordinary chrome button.
    Secondary,
    /// A secondary button that is on (`On` / `Off`).
    Toggle,
    /// Greyed but still pressable: a locked feature that answers with a message.
    Locked,
    /// A row inside a menu: no chrome until hovered.
    MenuItem,
    /// An icon on a pill.
    Icon,
    /// A round icon control.
    Panel,
    /// A bare glyph that only shows a wash on hover: carets, row actions.
    Ghost,
    /// One half of a segmented control.
    Segment,
}

impl BtnKind {
    /// Every kind, for tests and for building a style picker.
    pub const ALL: [BtnKind; 12] = [
        BtnKind::Primary,
        BtnKind::Success,
        BtnKind::Danger,
        BtnKind::Outlined,
        BtnKind::Secondary,
        BtnKind::Toggle,
        BtnKind::Locked,
        BtnKind::MenuItem,
        BtnKind::Icon,
        BtnKind::Panel,
        BtnKind::Ghost,
        BtnKind::Segment,
    ];
}

/// The two boxes a button may occupy. Two, because an app has two: the standard
/// control and the smaller icon box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtnSize {
    /// The standard control row.
    Control,
    /// The shorter icon-only box.
    Compact,
}

impl BtnSize {
    /// Outer height of the finished button, padding included.
    pub fn height(self) -> f32 {
        match self {
            Self::Control => CONTROL_HEIGHT,
            Self::Compact => COMPACT_HEIGHT,
        }
    }

    /// Vertical padding, which the content box is sized against.
    pub fn pad_y(self) -> u16 {
        match self {
            Self::Control => 5,
            Self::Compact => CONTROL_PADDING[0],
        }
    }

    /// Height of the content box the padding surrounds.
    pub fn content_height(self) -> f32 {
        self.height() - 2.0 * self.pad_y() as f32
    }
}

/// What sits inside the box.
enum Content<'a, M> {
    Label(&'a str),
    Glyph(char),
    LabelGlyph { code: char, label: &'a str },
    MenuRow {
        label: &'a str,
        checked: bool,
        shortcut: Option<&'a str>,
    },
    Custom(Element<'a, M>),
}

/// A button.
pub struct Btn<'a, M> {
    content: Content<'a, M>,
    kind: BtnKind,
    size: BtnSize,
    scheme: Scheme,
    width: Option<Length>,
    ink: Option<Color>,
    enabled: bool,
    active: bool,
    message: Option<M>,
}

impl<'a, M: Clone + 'a> Btn<'a, M> {
    /// A text button, sized to its label so it can never wrap.
    pub fn label(scheme: Scheme, label: &'a str) -> Self {
        Self::new(scheme, Content::Label(label)).width(Length::Fixed(styles::action_width(label)))
    }

    /// An icon button.
    pub fn glyph(scheme: Scheme, code: char) -> Self {
        Self::new(scheme, Content::Glyph(code)).kind(BtnKind::Icon)
    }

    /// A glyph and a label.
    pub fn glyph_label(scheme: Scheme, code: char, label: &'a str) -> Self {
        Self::new(scheme, Content::LabelGlyph { code, label })
    }

    /// A menu row: a check slot, the label and an optional key at the right.
    pub fn menu_row(
        scheme: Scheme,
        label: &'a str,
        checked: bool,
        shortcut: Option<&'a str>,
    ) -> Self {
        Self::new(
            scheme,
            Content::MenuRow {
                label,
                checked,
                shortcut,
            },
        )
        .kind(BtnKind::MenuItem)
        .width(Length::Fill)
    }

    /// Bespoke content. Its box metrics still belong to the button.
    pub fn content(scheme: Scheme, content: impl Into<Element<'a, M>>) -> Self {
        Self::new(scheme, Content::Custom(content.into()))
    }

    fn new(scheme: Scheme, content: Content<'a, M>) -> Self {
        Self {
            content,
            kind: BtnKind::Secondary,
            size: BtnSize::Control,
            scheme,
            width: None,
            ink: None,
            enabled: true,
            active: false,
            message: None,
        }
    }

    pub fn kind(mut self, kind: BtnKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn size(mut self, size: BtnSize) -> Self {
        self.size = size;
        self
    }

    /// On/off state for [`BtnKind::Toggle`] and [`BtnKind::Segment`].
    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// Layout only: a fixed or filled width. Never a metric of the box.
    pub fn width(mut self, width: Length) -> Self {
        self.width = Some(width);
        self
    }

    /// Override the glyph's colour when the look carries meaning (a danger tint,
    /// a muted icon). Not a metric.
    pub fn ink(mut self, ink: Color) -> Self {
        self.ink = Some(ink);
        self
    }

    /// A disabled button keeps its box and drops its message, so iced reports
    /// `Disabled` to the style and nothing can press it.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        if !enabled {
            self.message = None;
        }
        self
    }

    pub fn on_press(mut self, message: M) -> Self {
        self.message = Some(message);
        self
    }

    pub fn on_press_maybe(mut self, message: Option<M>) -> Self {
        self.message = message;
        self
    }

    /// The size every kind resolves to. Public so a suite can hold the invariant.
    pub fn resolved_size(&self) -> BtnSize {
        self.size
    }

    /// The one label size, for the same reason.
    pub fn label_size(&self) -> f32 {
        LABEL_SIZE
    }

}

/// The style for a kind, in one place. Public so another component can reuse a
/// button's look without rebuilding it.
pub fn style_of(
    kind: BtnKind,
    scheme: Scheme,
    active: bool,
    enabled: bool,
    theme: &Theme,
    status: button::Status,
) -> button::Style {
        match kind {
            BtnKind::Primary => styles::color_button(scheme.blue, scheme.on_accent)(theme, status),
            BtnKind::Success => styles::color_button(scheme.green, scheme.on_accent)(theme, status),
            BtnKind::Danger => styles::color_button(scheme.danger, scheme.on_accent)(theme, status),
            BtnKind::Outlined => {
                let ink = if theme::is_dark_scheme(&scheme) {
                    Color::WHITE
                } else {
                    scheme.on_accent
                };
                styles::color_button_outlined(scheme.green, ink, scheme)(theme, status)
            }
            BtnKind::Secondary | BtnKind::Icon => styles::pill_button_secondary(theme, status),
            BtnKind::Toggle if active => styles::pill_button_active(theme, status),
            BtnKind::Toggle => styles::pill_button_secondary(theme, status),
            BtnKind::Locked => styles::greyed_button(scheme)(theme, status),
            BtnKind::MenuItem => styles::menu_item_style(scheme, enabled)(theme, status),
            BtnKind::Panel => styles::panel_icon_button(scheme, enabled)(theme, status),
            BtnKind::Ghost => styles::subtle_icon_button(theme, status),
            BtnKind::Segment => styles::segment_button(scheme, active, !enabled)(theme, status),
    }
}

impl<'a, M: Clone + 'a> Btn<'a, M> {
    /// Finish the widget.
    pub fn build(self) -> Element<'a, M> {
        let Self {
            content,
            kind,
            size,
            scheme,
            width,
            ink,
            enabled,
            active,
            message,
        } = self;
        let content_height = size.content_height();
        let glyph_size = match (kind, size) {
            (BtnKind::Panel, _) => ICON_ONLY_SIZE,
            (_, BtnSize::Control) => 16.0,
            (_, BtnSize::Compact) => ICON_SIZE,
        };
        let glyph_ink = ink.unwrap_or(match kind {
            BtnKind::Primary | BtnKind::Success | BtnKind::Danger => scheme.on_accent,
            BtnKind::Locked => scheme.muted,
            _ => scheme.ink,
        });

        let centred = |content: Element<'a, M>| -> Element<'a, M> {
            container(content)
                .height(Length::Fixed(content_height))
                .align_y(iced::Alignment::Center)
                .into()
        };

        let inner: Element<'a, M> = match content {
            Content::Label(label) => centred(
                text(label)
                    .size(LABEL_SIZE)
                    .wrapping(crate::styles::NO_WRAP)
                    .into(),
            ),
            Content::Glyph(code) => centred(glyph_element(code, glyph_size, glyph_ink)),
            Content::LabelGlyph { code, label } => centred(
                row![
                    glyph_element(code, glyph_size, glyph_ink),
                    text(label)
                        .size(LABEL_SIZE)
                        .wrapping(crate::styles::NO_WRAP),
                ]
                .spacing(theme::SPACE_XS)
                .align_y(iced::Alignment::Center)
                .into(),
            ),
            Content::MenuRow {
                label,
                checked,
                shortcut,
            } => {
                let check: Element<'a, M> = if checked && enabled {
                    glyph_element(styles::ICON_CHECK, theme::TEXT_XS, scheme.blue)
                } else {
                    horizontal_space().into()
                };
                let mut row_content = row![
                    container(check)
                        .width(Length::Fixed(styles::MENU_CHECK_WIDTH))
                        .center_x(Length::Fixed(styles::MENU_CHECK_WIDTH)),
                    text(label)
                        .size(LABEL_SIZE)
                        .wrapping(crate::styles::NO_WRAP),
                ]
                .spacing(theme::SPACE_SM)
                .align_y(iced::Alignment::Center)
                .width(Length::Fill);
                if let Some(key) = shortcut {
                    row_content = row_content.push(horizontal_space());
                    row_content = row_content.push(
                        text(key)
                            .size(theme::TEXT_XS)
                            .wrapping(crate::styles::NO_WRAP)
                            .color(scheme.muted),
                    );
                }
                centred(row_content.into())
            }
            Content::Custom(element) => centred(element),
        };

        let width = match (width, kind) {
            (Some(width), _) => width,
            (None, BtnKind::Panel) => Length::Fixed(size.height()),
            (None, _) => Length::Shrink,
        };
        let pad_x = match kind {
            BtnKind::MenuItem => 6,
            BtnKind::Panel | BtnKind::Segment => 0,
            BtnKind::Ghost => 4,
            _ => 10,
        };

        let mut widget = button(inner)
            .padding([0, pad_x])
            .width(width)
            .style(move |theme, status| {
                style_of(kind, scheme, active, enabled, theme, status)
            });
        if let Some(message) = message {
            widget = widget.on_press(message);
        }
        widget.into()
    }
}

impl<'a, M: Clone + 'a> From<Btn<'a, M>> for Element<'a, M> {
    fn from(button: Btn<'a, M>) -> Self {
        button.build()
    }
}

/// A bootstrap-icons glyph at an explicit size and colour.
pub fn glyph_element<'a, M: Clone + 'a>(code: char, size: f32, colour: Color) -> Element<'a, M> {
    text(code.to_string())
        .font(Font::with_name(ICON_FAMILY))
        .size(size)
        .style(move |_theme| iced::widget::text::Style { color: Some(colour) })
        .into()
}
