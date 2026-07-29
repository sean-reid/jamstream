//! Showing a file in the platform's own file manager.
//!
//! Not opening it. [`egui::Context::open_url`], which the host wizard uses for
//! provider token pages, hands a path to the default handler, so on macOS it
//! would start playing a FLAC in Music instead of showing where it is. A take
//! is something you then drag into a DAW, so the folder with the file selected
//! is the thing worth asking for.
//!
//! Each platform is named the way that platform names it: our own coinage
//! reads alien everywhere.
//!
//! It must never be a dead button. A sandboxed Flatpak, an SSH session, and a
//! box with no file manager all fail here, and when they do the caller shows
//! the path as selectable text: the take exists either way, and what failed is
//! our ability to open a window rather than their ability to have the file.

use std::path::Path;
use std::process::Command;

/// What the button that calls [`show`] is called on this platform.
pub const LABEL: &str = if cfg!(target_os = "macos") {
    "Reveal in Finder"
} else if cfg!(target_os = "windows") {
    "Show in File Explorer"
} else {
    "Show in Files"
};

/// Opens the folder holding `path` with `path` selected.
///
/// The error is the platform's own, and the caller shows the path when there
/// is one.
pub fn show(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("{} is not there any more", path.display()));
    }
    reveal(path)
}

#[cfg(target_os = "macos")]
fn reveal(path: &Path) -> Result<(), String> {
    // -R reveals and selects in Finder rather than opening the file.
    run(Command::new("open").arg("-R").arg(path))
}

#[cfg(target_os = "windows")]
fn reveal(path: &Path) -> Result<(), String> {
    // No space after the comma: explorer takes the rest of the argument as the
    // path, and a space makes it open Documents instead.
    run(Command::new("explorer").arg(format!("/select,{}", path.display())))
}

/// GNOME Files and Dolphin both implement the freedesktop file manager
/// interface, which is the only call that selects the file rather than just
/// opening its folder. `gdbus` ships with glib, so nothing new is needed to
/// make it.
///
/// Where nothing answers, the folder opens with nothing highlighted. That is
/// the honest ceiling on Linux, and it is still better than a refusal.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn reveal(path: &Path) -> Result<(), String> {
    let uri = format!("file://{}", path.display());
    let show_items = run(Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.FileManager1",
            "--object-path",
            "/org/freedesktop/FileManager1",
            "--method",
            "org.freedesktop.FileManager1.ShowItems",
        ])
        .arg(format!("['{uri}']"))
        .arg(""));
    if show_items.is_ok() {
        return Ok(());
    }
    let parent = path.parent().unwrap_or(path);
    run(Command::new("xdg-open").arg(parent)).map_err(|err| {
        format!("{err}. No file manager answered on the session bus either, so the folder could not be opened")
    })
}

/// Runs the command and reports what the platform said. Waits for it: a file
/// manager that refuses does so immediately, and a caller that showed nothing
/// would be the dead button this module exists to avoid.
fn run(command: &mut Command) -> Result<(), String> {
    let name = command.get_program().to_string_lossy().into_owned();
    let output = command
        .output()
        .map_err(|err| format!("{name} could not be started: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let reason = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if reason.is_empty() {
        Err(format!("{name} failed"))
    } else {
        Err(format!("{name}: {reason}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The label is the platform's own word for this, and never ours.
    #[test]
    fn the_button_is_named_the_way_this_platform_names_it() {
        assert!(
            ["Reveal in Finder", "Show in File Explorer", "Show in Files"].contains(&LABEL),
            "{LABEL}"
        );
    }

    /// A take that is gone says so rather than handing a missing path to the
    /// platform, which answers with nothing on macOS and a dialog elsewhere.
    #[test]
    fn a_path_that_is_not_there_is_refused_before_the_platform_sees_it() {
        let missing = std::env::temp_dir().join("jamstream-nothing-here-at-all.flac");
        let err = show(&missing).expect_err("a missing take cannot be revealed");
        assert!(err.contains("not there any more"), "{err}");
    }
}
