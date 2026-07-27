//! Deterministic RGBA frames for the broadcast a live session's viewers
//! watch. Audio is the star; this is the stage: near-black surface, the
//! session name and members in the interface type, avatars in tight
//! geometry, LED level meters as the only living element. Frames are pure
//! functions of (config, members, listener_count, frame_index): no clocks,
//! no randomness beyond a stable hash of member names.

mod avatar;
/// The stage palette and the initials-disc hue rule. Public so the desktop
/// client can assert its own copy still agrees: a member must look the same
/// in the app and on the stream card.
pub mod palette;
mod render;
mod text;

pub use avatar::{AvatarError, AvatarImage};
pub use render::{Renderer, initials};

/// Fixed per-stream scene parameters. Everything sized off these is
/// preallocated in [`Renderer::new`].
#[derive(Clone, Debug)]
pub struct SceneConfig {
    pub width: u32,
    pub height: u32,
    pub session_name: String,
    /// Draw the small jamstream lockup with the amber tuning dot, lower right.
    pub wordmark: bool,
}

impl SceneConfig {
    /// 1280x720 with the wordmark on, the standard broadcast frame.
    pub fn new(session_name: impl Into<String>) -> Self {
        SceneConfig {
            width: 1280,
            height: 720,
            session_name: session_name.into(),
            wordmark: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Gets a card with an avatar and a live meter.
    Musician,
    /// Counted in the quiet footer line, never carded.
    Listener,
}

/// One member as seen this frame. Levels are linear 0..1 with ballistics
/// already applied by the caller; the renderer adds only the visual
/// peak-hold. Keep `avatar` buffers stable across frames: the renderer
/// caches card pixels keyed by the avatar's data pointer, and a fresh
/// clone every frame forces a full card repaint.
#[derive(Clone, Debug)]
pub struct MemberVisual {
    pub name: String,
    pub avatar: Option<AvatarImage>,
    pub level_peak: f32,
    pub level_rms: f32,
    pub connected: bool,
    pub role: Role,
}
