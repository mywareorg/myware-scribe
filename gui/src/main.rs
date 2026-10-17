#![windows_subsystem = "windows"]

mod app;
mod backend;

use app::App;

/// Stable identity for the Windows taskbar (pinning/grouping under our icon).
#[cfg(windows)]
fn set_app_user_model_id() {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "shell32")]
    extern "system" {
        fn SetCurrentProcessExplicitAppUserModelID(app_id: *const u16) -> i32;
    }

    let id: Vec<u16> = OsStr::new("transiel").encode_wide().chain(Some(0)).collect();
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(id.as_ptr());
    }
}

fn main() -> iced::Result {
    #[cfg(windows)]
    set_app_user_model_id();

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
