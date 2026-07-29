//! Deterministic RGBA frames for the broadcast a live session's viewers
//! watch. Audio is the star; this is the stage: near-black surface, member
//! names in the interface type, avatars in tight geometry, LED level meters
//! as the only living element. Frames are pure functions of (config,
//! members, listener_count, frame_index): no clocks, no randomness beyond a
//! stable hash of member names.

mod avatar;
/// The stage palette and the initials-disc hue rule. Public so the desktop
/// client can assert its own copy still agrees: a member must look the same
/// in the app and on the stream card.
pub mod palette;
mod render;
mod text;

pub use avatar::{AvatarError, AvatarImage};
pub use render::{MAX_CARDS, Renderer, initials};

/// Fixed per-stream scene parameters. Everything sized off these is
/// preallocated in [`Renderer::new`].
#[derive(Clone, Debug)]
pub struct SceneConfig {
    pub width: u32,
    pub height: u32,
    /// Draw the small jamstream lockup with the amber tuning dot, lower right.
    pub wordmark: bool,
}

impl Default for SceneConfig {
    /// 1280x720 with the wordmark on, the standard broadcast frame.
    fn default() -> Self {
        SceneConfig {
            width: 1280,
            height: 720,
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
