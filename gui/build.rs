fn main() {
    // Cargo does not track the icon as an input unless it is declared, so a
    // replaced .ico would otherwise leave the old icon in a cached binary.
    println!("cargo:rerun-if-changed=assets/icon.ico");

    // Embed the .ico into the Windows executable (Explorer + pinned shortcut +
    // the taskbar icon before the window icon loads). No-op on other platforms.
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Hippo Transcribe");
        res.set(
            "FileDescription",
            "Hippo Transcribe \u{2014} Private, offline transcription",
        );
        res.set("CompanyName", "Sean Wu");
        res.set("LegalCopyright", "Copyright (C) 2026 Sean Wu");
        res.set("OriginalFilename", "hippo-transcribe.exe");
        if let Err(error) = res.compile() {
            println!("cargo:warning=winresource failed to embed the exe icon: {error}");
        }
    }
}
