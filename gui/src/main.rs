mod app;
mod backend;

use app::App;

fn main() -> iced::Result {
    iced::application("transiel", App::update, App::view)
        .theme(App::theme)
        .subscription(App::subscription)
        .window(App::window_settings())
        .run()
}
