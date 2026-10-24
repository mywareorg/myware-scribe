#![windows_subsystem = "windows"]

mod app;
mod backend;
mod documents;
mod files;
mod format;
mod options;
mod theme;

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

    let id: Vec<u16> = OsStr::new("HippoTranscribe")
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(id.as_ptr());
    }
}

/// GUI-subsystem binaries have no console. Attach to the parent console for the
/// `--cli` path so download progress is visible from a terminal or deploy.bat.
#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    unsafe {
        // The GUI re-invokes itself with `--cli` and pipes stdout/stderr when
        // running a batch. Those handles must stay attached to the pipes, not
        // be redirected to the parent console.
        let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
        if !stdout.is_null() && stdout != INVALID_HANDLE_VALUE {
            return;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        let handle = CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if handle != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_OUTPUT_HANDLE, handle);
            SetStdHandle(STD_ERROR_HANDLE, handle);
        }
    }
}

fn main() -> iced::Result {
    #[cfg(windows)]
    set_app_user_model_id();

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--cli") {
        #[cfg(windows)]
        attach_console();
        std::process::exit(hippo_transcribe_core::cli::run(&args[2..]));
    }

    iced::application(
        "Hippo Transcribe \u{2014} Private, offline transcription",
        App::update,
        App::view,
    )
    .theme(App::theme)
    .subscription(App::subscription)
    .font(include_bytes!("../assets/bootstrap-icons.ttf").as_slice())
    .window(App::window_settings())
    .run()
}
