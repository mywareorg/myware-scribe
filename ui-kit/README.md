# ui-kit

Portable UI primitives for [iced](https://iced.rs) applications: one palette, one
set of widget styles, one button, menus, dialogs and a toast. It is a port of the
Hippo Transcribe GUI's widget vocabulary, so a second project can have the same
surfaces without copying twenty thousand lines of transcript code.

Everything is generic over the host application's message type. Nothing here
knows about transcripts, audio or licences.

## What is in it

| Module | Contents |
|---|---|
| `theme` | `Scheme` (the light and dark palettes), `AppTheme`, the spacing / type / radius / control constants, `lighten` / `darken` / `mix`, `is_dark` / `is_dark_scheme` |
| `styles` | `island_border`, `soft_shadow`, `dialog_shadow`, `pill_button_secondary` / `_active` / `_toggled`, `text_action_button`, `subtle_icon_button`, `greyed_button`, `panel_icon_button`, `color_button`, `color_button_outlined`, `segment_button`, `menu_item_style`, `hover_band`, `pick_menu_style`, `pill_pick`, `speed_pick`, `greyed_pick`, `pill_input`, `scrollbar_style`, `visible_scrollbar_style`, `icon_line_height`, `estimated_text_width`, `action_width` |
| `button` | `Btn` with `BtnKind` (12 looks), `BtnSize` (`Control` 30.8 px, `Compact` 24.2 px), label / glyph / label+glyph / menu-row / custom constructors |
| `menu` | `MenuRow`, `MenuSection`, `menu_card`, `menu_width`, `menu_divider`, `caret_handle` |
| `dialog` | `modal_card`, `modal_overlay` |
| `toast` | `toast_card`, `with_toast`, `ToastTone`, `toast_width` |
| `prelude` | one import for all of the above |

## Wiring a new app

Build a `Scheme` once per frame and pass it down. That single value is what makes
the light and dark palettes one code path:

```rust
let scheme = AppTheme::System.scheme();   // Light, Dark, or follow the desktop
```

Give iced the matching theme so any widget still using built-in styling agrees
with the kit:

```rust
iced::application(..).theme(|state: &State| state.mode.iced())
```

The kit draws glyphs from **Bootstrap Icons** and expects a font with the family
name `ui_kit::ICON_FAMILY` (`"bootstrap-icons"`) to be loaded:

```rust
iced::application(..).font(include_bytes!("assets/bootstrap-icons.ttf"))
```

### A button

```rust
use ui_kit::prelude::*;

let save = Btn::label(scheme, "Save")
    .kind(BtnKind::Primary)
    .on_press(Message::Save);
let cancel = Btn::label(scheme, "Cancel").on_press(Message::Cancel);
let delete = Btn::glyph(scheme, '\u{f5dd}')
    .kind(BtnKind::Panel)
    .ink(scheme.danger)
    .on_press(Message::Delete);
let add = Btn::glyph_label(scheme, '\u{f4fe}', "Add").on_press(Message::Add);
```

`Btn` is an `Element` through `From`, so it drops straight into `row!` / `column!`.

The point of the component is what it does **not** expose: there is no padding
step, no label size and no glyph size. The kind picks the look, the size picks the
box, the constructor picks the type size, and the content is centred by layout —
a fixed-height container with `center_y(Fill)` — so a button cannot drift from its
neighbours.

### A menu

A menu is a stack layer; the host positions it.

```rust
let menu = menu_card(
    vec![MenuSection::rows(vec![
        MenuRow::new("Show timestamps").checked(state.timestamps).key("T")
            .on_press(Message::ToggleTimestamps),
        MenuRow::new("Rename").enabled(state.editable)
            .on_press_maybe(state.editable.then_some(Message::Rename)),
    ])],
    scheme,
    Message::Noop,
);

let layer: Element<_> = container(menu)
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(iced::Alignment::End)   // the card hugs the right edge ...
    .align_y(iced::Alignment::Start)
    .padding(iced::Padding { top: 48.0, right: 8.0, ..iced::Padding::ZERO })
    .into();
```

`noop` is a message a disabled row answers with: iced only reports `Hovered` for
a button that has a press handler, and every row of a menu should wash under the
pointer, including the ones that do nothing.

## Version pinning

The kit depends on `iced = "0.13"` with **only** `advanced` enabled (it needs
`advanced` for the glyph line height). It deliberately enables no renderer: the
host application chooses wgpu, tiny-skia or its own.

That is a hard pin in practice. The widget and style types this crate exposes —
`button::Style`, `pick_list::Style`, `overlay::menu::Style`, `scrollable::Style` —
are iced's API, not a stable interface between minor versions. A host should use
the same `0.13.x` the kit was built against; `ui_kit::ICED_VERSION` is exported so
a build script or a runtime check can assert it.

## Theme trait requirements

Every style in `styles` is a plain function `Fn(&Theme, Status) -> Style`, so they
work with any `Theme` value. The two places that ask for more are the widget
classes that take a *style function* rather than a status: `pick_menu_style`
returns `impl Fn(&Theme) -> overlay::menu::Style`, which is what
`pick_list(..).menu_style(..)` wants. `caret_handle` returns a
`pick_list::Handle<Font>` and needs the icon font loaded.

Nothing here implements iced's `Catalog` traits for you. If you want the kit's
looks available as a theme class rather than as explicit `.style(..)` calls, that
is a small adapter in your app:

```rust
impl container::Catalog for MyTheme { type Class<'a> = StyleFn<'a, Self>; .. }
```

## What was left behind as app-specific

Transcript rows and bubbles, the waveform and deck canvases, queue rows and their
drag-and-drop, the virtualised list, the line editor and its strip, and the
keyboard shortcuts. They all depend on the app's own state and would only
generalise into a framework, not a kit.

## Differences from the app's copies

The port is faithful except where the app's version cannot be generic:

* `Message` is a type parameter everywhere; `menu_card` and `modal_overlay` take a
  `noop` message for the press handlers they need to attach.
* `header_menu_item` became `MenuRow` + `menu_card`, and `header_menu_card`
  became `menu_card`; the row metrics are unchanged.
* The toast's dismiss message is the host's, and the layer's bottom inset is a
  parameter rather than a queue-dependent value.
* `AppTheme::iced()` names its generated themes "ui-kit Light" / "ui-kit Dark"
  rather than after the app.
