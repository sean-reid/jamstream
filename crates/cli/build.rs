//! Embeds a VERSIONINFO block into jamstream.exe so Task Manager and
//! the Properties Details tab say what the binary is. Windows targets
//! only; a no-op everywhere else. No icon: this is a console tool, not
//! something a user launches from Explorer.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set("ProductName", "JamStream")
            .set("FileDescription", "JamStream command line interface")
            .compile()
            .expect("compiling the Windows exe resources failed");
    }
}
