//! The native file picker for the avatar row.
//!
//! One thread per pick: it waits on the platform dialog, then reads, fits,
//! and decodes the chosen file, so neither the dialog nor a 12 megapixel
//! JPEG ever sits on the paint thread. The UI polls once a frame.
//!
//! The dialog is created on the calling thread rather than inside the
//! worker, because on macOS the panel belongs to the main thread: rfd builds
//! it there and the future carries only its completion. The shared executor
//! in [`crate::exec`] is deliberately not used, since it runs one job at a
//! time and a dialog can stay open for minutes.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use crate::avatar::{self, Picture};

/// What came back from one pick.
#[derive(Debug)]
pub enum Picked {
    /// The dialog closed with nothing chosen.
    Cancelled,
    Loaded(Box<Picture>),
    /// The file was chosen but is not usable, in the words of
    /// [`avatar::AvatarError`].
    Failed(String),
}

/// A dialog waiting for the musician.
pub struct Pick {
    rx: Receiver<Picked>,
}

impl Pick {
    /// Opens the picture dialog. Call this from the UI thread.
    pub fn picture() -> Pick {
        let dialog = rfd::AsyncFileDialog::new()
            .set_title("Choose a picture")
            .add_filter("Images", &["png", "jpg", "jpeg"])
            .pick_file();
        Pick::spawn(async move { dialog.await.map(|file| file.path().to_path_buf()) })
    }

    fn spawn<F>(dialog: F) -> Pick
    where
        F: Future<Output = Option<PathBuf>> + Send + 'static,
    {
        let (tx, rx) = channel();
        let worker = std::thread::Builder::new()
            .name("jamstream-picker".into())
            .spawn(move || {
                // A current-thread runtime with no drivers: the dialog's
                // future is woken by the platform, and nothing here needs a
                // timer or a socket.
                let picked = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("picker runtime")
                    .block_on(dialog);
                let outcome = match picked {
                    Some(path) => match avatar::load(&path) {
                        Ok(picture) => Picked::Loaded(Box::new(picture)),
                        Err(err) => Picked::Failed(err),
                    },
                    None => Picked::Cancelled,
                };
                let _ = tx.send(outcome);
            });
        if let Err(err) = worker {
            tracing::warn!(%err, "the file picker thread did not start");
        }
        Pick { rx }
    }

    /// Nonblocking, and yields at most once. A thread that died without
    /// sending reads as a cancelled dialog, so the row never waits forever.
    pub fn poll(&mut self) -> Option<Picked> {
        match self.rx.try_recv() {
            Ok(picked) => Some(picked),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Picked::Cancelled),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait(pick: &mut Pick) -> Picked {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(picked) = pick.poll() {
                return picked;
            }
            assert!(Instant::now() < deadline, "the pick never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The plumbing around the dialog, with the dialog itself replaced by a
    /// future that parks: a chosen file comes back loaded, and it was loaded
    /// somewhere other than the caller's thread.
    #[test]
    fn a_chosen_file_comes_back_loaded_off_the_calling_thread() {
        let dir = std::env::temp_dir().join(format!("jamstream-pick-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("rehearsal.png");
        std::fs::write(&path, avatar::tests::png(40, 40)).expect("write the picture");

        let (tx, rx) = std::sync::mpsc::channel();
        let mut pick = Pick::spawn(async move { rx.recv().expect("the dialog's answer") });
        assert!(pick.poll().is_none(), "nothing until the dialog closes");
        tx.send(Some(path.clone())).expect("close the dialog");
        match wait(&mut pick) {
            Picked::Loaded(picture) => {
                assert_eq!(picture.file, "rehearsal.png");
                assert_eq!(picture.fitted, (40, 40));
            }
            other => panic!("expected the picture, got {other:?}"),
        }

        std::fs::remove_file(&path).ok();
    }

    /// Closing the dialog with nothing chosen leaves the avatar alone.
    #[test]
    fn a_cancelled_dialog_reports_itself() {
        let mut pick = Pick::spawn(async move { None });
        assert!(matches!(wait(&mut pick), Picked::Cancelled));
    }

    /// A file that is not a picture names its own reason.
    #[test]
    fn a_file_that_is_not_a_picture_fails_with_the_reason() {
        let dir = std::env::temp_dir().join(format!("jamstream-pick-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("set-list.txt");
        std::fs::write(&path, b"three chords").expect("write the file");
        let mut pick = Pick::spawn(async move { Some(path) });
        match wait(&mut pick) {
            Picked::Failed(err) => {
                assert_eq!(err, "set-list.txt: only PNG and JPEG images are supported")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}
