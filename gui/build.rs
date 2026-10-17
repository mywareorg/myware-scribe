fn main() {
    // Embed the .ico into the Windows executable (Explorer + pinned shortcut +
    // the taskbar icon before the window icon loads). No-op on other platforms.
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(error) = res.compile() {
            println!("cargo:warning=winresource failed to embed the exe icon: {error}");
        }
    }
}
