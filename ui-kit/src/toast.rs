//! A bottom-right toast: a dot, one line of text, and a width that follows the
//! message without ever passing a cap.

use iced::widget::{container, horizontal_space, mouse_area, row, stack, text};
use iced::{border, Element, Color, Length};

use crate::styles;
use crate::theme::{self, Scheme};

/// How the toast reads at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastTone {
    Success,
    Info,
    Danger,
}

impl ToastTone {
    fn dot(self, scheme: &Scheme) -> Color {
        match self {
            Self::Success => scheme.green,
            Self::Info => scheme.blue,
            Self::Danger => scheme.danger,
        }
    }
}

/// Width the toast needs for `message`: an estimate of the text plus the dot and
/// the padding, clamped to a readable band.
pub fn toast_width(message: &str) -> f32 {
    ((message.chars().count() as f32 * 7.2 + 46.0).clamp(180.0, 320.0)).max(180.0)
}

/// The toast itself. Its click handler is the host's: this returns the card and
/// leaves the dismissal to the caller.
pub fn toast_card<'a, M: Clone + 'static>(
    message: &'a str,
    tone: ToastTone,
    scheme: Scheme,
) -> Element<'a, M> {
    let dot = container(horizontal_space())
        .width(Length::Fixed(6.0))
        .height(Length::Fixed(6.0))
        .style(move |_theme| container::Style {
            background: Some(tone.dot(&scheme).into()),
            border: border::rounded(3.0),
            ..container::Style::default()
        });
    container(
        row![
            dot,
            text(message).size(theme::TEXT_SM).wrapping(styles::NO_WRAP)
        ]
        .spacing(theme::SPACE_MD)
        .align_y(iced::Alignment::Center),
    )
    .width(Length::Fixed(toast_width(message)))
    .height(Length::Fixed(36.0))
    .align_y(iced::Alignment::Center)
    .padding([0, theme::SPACE_LG as u16])
    .style(move |_theme| container::Style {
        background: Some(scheme.dialog_bg.into()),
        text_color: Some(scheme.text),
        border: border::Border {
            color: scheme.border,
            width: 1.0,
            radius: 12.0.into(),
        },
        shadow: styles::soft_shadow(),
    })
    .into()
}

/// Stack a toast at the bottom right of `base`, or return the base untouched.
///
/// The caller passes the layer's bottom inset, which is what keeps the toast
/// clear of whatever the page keeps at the bottom.
pub fn with_toast<'a, M: Clone + 'static>(
    base: impl Into<Element<'a, M>>,
    toast: Option<Element<'a, M>>,
    scheme: Scheme,
    bottom: f32,
    on_dismiss: M,
) -> Element<'a, M> {
    let base = base.into();
    let layer: Element<'a, M> = match toast {
        Some(toast) => mouse_area(toast).on_press(on_dismiss).into(),
        // An empty, non-interactive filler so the stack shape — and therefore
        // every stateful widget under it — is identical either way.
        None => horizontal_space().into(),
    };
    let _ = scheme;
    let overlay = container(layer)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::Alignment::End)
        .align_y(iced::Alignment::End)
        .padding(iced::Padding {
            top: 0.0,
            right: theme::SPACE_LG,
            bottom,
            left: 0.0,
        });
    stack![base, overlay].into()
}

/// A toast as a one-liner, for hosts that just want the look.
pub fn simple_toast<'a, M: Clone + 'static>(
    message: &'a str,
    tone: ToastTone,
    scheme: Scheme,
) -> Element<'a, M> {
    toast_card(message, tone, scheme)
}
