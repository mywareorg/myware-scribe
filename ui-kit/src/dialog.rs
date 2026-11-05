//! Modal dialogs: the card, the scrim and the stack that puts one over the page.

use iced::widget::{container, horizontal_space, mouse_area, stack};
use iced::{border, mouse, Element, Length};

use crate::styles;
use crate::theme::{self, Scheme, MODAL_PADDING};

/// The card a dialog is drawn in: fixed width, raised surface, rounded outline.
pub fn modal_card<'a, M: Clone + 'static>(
    content: impl Into<Element<'a, M>>,
    width: f32,
    scheme: Scheme,
) -> Element<'a, M> {
    container(content)
        .width(Length::Fixed(width))
        .padding(MODAL_PADDING)
        .style(move |_theme| container::Style {
            background: Some(scheme.dialog_bg.into()),
            border: if theme::is_dark_scheme(&scheme) {
                border::Border {
                    color: scheme.border,
                    width: 1.0,
                    radius: 16.0.into(),
                }
            } else {
                border::rounded(16.0)
            },
            shadow: styles::dialog_shadow(),
            ..container::Style::default()
        })
        .into()
}

/// Stack `dialog` over `base` behind a scrim.
///
/// The backdrop and the card both consume presses, scrolls and clicks with
/// `noop`, so nothing behind the dialog can be reached while it is up. `Idle`
/// keeps the arrow cursor over the scrim instead of inheriting the pointer or
/// text cursor of whatever sits underneath.
pub fn modal_overlay<'a, M: Clone + 'static>(
    base: impl Into<Element<'a, M>>,
    dialog: impl Into<Element<'a, M>>,
    scheme: Scheme,
    noop: M,
) -> Element<'a, M> {
    let base = base.into();
    let dialog = dialog.into();
    let noop_backdrop = noop.clone();
    let backdrop = mouse_area(
        container(horizontal_space())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(scheme.scrim.into()),
                ..container::Style::default()
            }),
    )
    .interaction(mouse::Interaction::Idle)
    .on_press(noop_backdrop.clone())
    .on_right_press(noop_backdrop.clone())
    .on_middle_press(noop_backdrop.clone())
    // The stack already withholds the wheel from layers under an area that
    // reports an interaction, but capturing it here says so outright: a
    // scrollable behind the scrim must never move.
    .on_scroll(move |_| noop_backdrop.clone());

    let noop_card = noop.clone();
    let dialog = mouse_area(dialog)
        .interaction(mouse::Interaction::Idle)
        .on_press(noop_card.clone())
        .on_right_press(noop_card.clone())
        .on_middle_press(noop_card.clone())
        .on_scroll(move |_| noop_card.clone());

    let centered = container(dialog)
        .center(Length::Fill)
        .width(Length::Fill)
        .height(Length::Fill);

    stack![base, backdrop, centered].into()
}
