//! Embeds the app icon and a VERSIONINFO block into jamstream-app.exe,
//! so Explorer, the taskbar, Alt-Tab, Task Manager, and the Properties
//! Details tab all say what the binary is. Windows targets only; a
//! no-op everywhere else. FileVersion, ProductVersion, and the numeric
//! version block all derive from CARGO_PKG_VERSION inside winresource.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/icon/jamstream.ico")
            .set("ProductName", "JamStream")
            .set("FileDescription", "JamStream desktop app")
            .compile()
            .expect("compiling the Windows exe resources failed");
    }
}
