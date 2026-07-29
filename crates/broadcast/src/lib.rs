//! Deterministic RGBA frames for the broadcast a live session's viewers
//! watch. Audio is the star; this is the stage: near-black surface, member
//! names in the interface type, avatars in tight geometry, LED level meters
//! as the only living element. Frames are pure functions of (config,
//! members, listener_count, frame_index): no clocks, no randomness beyond a
//! stable hash of member names.

mod avatar;
/// The meter law, in seconds and decibels. Public for the same reason
/// [`palette`] is: the app draws a meter for the same signal, so there is one
/// copy of where green ends and how long a peak holds.
pub mod meter;
/// The stage palette and the initials-disc hue rule. Public so the desktop
/// client can assert its own copy still agrees: a member must look the same
/// in the app and on the stream card.
pub mod palette;
mod render;
mod text;

pub use avatar::{AvatarError, AvatarImage, MAX_BYTES, MAX_DIM};
pub use render::{MAX_CARDS, Renderer, initials};

/// Fixed per-stream scene parameters. Everything sized off these is
/// preallocated in [`Renderer::new`].
#[derive(Clone, Debug)]
pub struct SceneConfig {
    pub width: u32,
    pub height: u32,
    /// Frames per second the stream is encoded at, from the platform catalog.
    /// The renderer needs it because the meter's peak-hold is a duration:
    /// see [`meter`] for what a wrong one does to a viewer.
    pub fps: u32,
    /// Draw the small jamstream lockup with the amber tuning dot, lower right.
    pub wordmark: bool,
}

impl Default for SceneConfig {
    /// 1280x720 at 30 fps with the wordmark on, the standard broadcast frame.
    fn default() -> Self {
        SceneConfig {
            width: 1280,
            height: 720,
            fps: 30,
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
