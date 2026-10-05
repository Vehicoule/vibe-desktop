fn main() {
    // Windows only: embed packaging/icon.ico into the exe so Explorer,
    // taskbar and the NSIS installer shortcut show the app icon.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("../../packaging/vibe-desktop.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
