mod app;
mod backend;

use app::App;

fn main() -> iced::Result {
    iced::application("transiel", App::update, App::view)
        .subscription(App::subscription)
        .run()
}
