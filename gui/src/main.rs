#![windows_subsystem = "windows"]

mod app;
mod backend;

use app::App;

fn main() -> iced::Result {
    iced::application("transiel", App::update, App::view)
        .theme(App::theme)
        .subscription(App::subscription)
        .font(include_bytes!("../assets/bootstrap-icons.ttf").as_slice())
        .window(App::window_settings())
        .run()
}
