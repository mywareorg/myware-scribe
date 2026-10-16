#![windows_subsystem = "windows"]

mod app;
mod backend;

use app::App;

fn main() -> iced::Result {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--cli") {
        std::process::exit(transiel_core::cli::run(&args[2..]));
    }

    iced::application("transiel", App::update, App::view)
        .theme(App::theme)
        .subscription(App::subscription)
        .font(include_bytes!("../assets/bootstrap-icons.ttf").as_slice())
        .window(App::window_settings())
        .run()
}
