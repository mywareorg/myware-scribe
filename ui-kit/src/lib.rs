//! Portable UI primitives for iced applications.
//!
//! This is a port of the Hippo Transcribe GUI's widget vocabulary so the same
//! surfaces, buttons, menus and dialogs can be used in another project. Nothing
//! here knows about transcripts, audio or licences: every component is generic
//! over the host application's message type, and the only shared state is a
//! [`theme::Scheme`] palette.
//!
//! Start with [`prelude`]. A host application builds a [`theme::Scheme`] once per
//! frame and hands it to the component it is building: the scheme is what makes
//! the light and dark palettes the same code path.
//!
//! The kit draws its glyphs from the [Bootstrap Icons] font, so the host must
//! load a font with the family name [`theme::ICON_FAMILY`] (the app does this with
//! `iced::application(..).font(include_bytes!("bootstrap-icons.ttf"))`).
//!
//! [Bootstrap Icons]: https://icons.getbootstrap.com/

pub mod button;
pub mod dialog;
pub mod menu;
pub mod prelude;
pub mod styles;
pub mod theme;
pub mod toast;

/// The iced release this crate is built against. A host project should depend on
/// the same minor version: the widget and style types are part of iced's API, not
/// a stable interface between 0.13.x and 0.14.
pub const ICED_VERSION: &str = "0.13";

#[cfg(test)]
mod tests {
    use crate::button::{Btn, BtnKind, BtnSize, LABEL_SIZE};
    use crate::theme::Scheme;

    /// Every kind at the control size is one control height, and every label is
    /// one type size: the invariant the button exists to hold.
    #[test]
    fn every_button_kind_is_one_height_and_one_label_size() {
        let scheme = Scheme::light();
        for kind in BtnKind::ALL {
            let button: Btn<'_, ()> = Btn::label(scheme, "A label").kind(kind);
            let size = button.resolved_size();
            assert_eq!(
                size.height(),
                crate::theme::CONTROL_HEIGHT,
                "{kind:?} is not a control height"
            );
            assert_eq!(button.label_size(), LABEL_SIZE, "{kind:?} has its own label size");
        }
        assert_eq!(
            BtnSize::Compact.height(),
            crate::button::COMPACT_HEIGHT,
            "the compact box drifted from the icon pill it was measured from"
        );
    }
}
