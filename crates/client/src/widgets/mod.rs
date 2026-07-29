//! Custom controls drawn with the painter. A fader is a line and a handle;
//! meters are the one colorful place because there color is information.

mod avatar;
mod db_drag;
mod fader;
mod lamp;
mod meter;
mod pan;
mod pick;
mod status_dot;

pub use avatar::{
    AVATAR_D_ROW, AVATAR_D_STRIP, avatar_disc, avatar_texture_count, sweep_avatar_textures,
};
pub use db_drag::db_drag;
pub use fader::{FADER_DEFAULT_DB, FADER_MAX_DB, FADER_MIN_DB, db_to_t, fader, t_to_db};
pub use lamp::{LampShape, lamp, lamp_toggle, state_lamp, state_lamp_width};
pub use meter::{Meter, meter};
pub use pan::pan_slider;
pub use pick::{PICK_DOT, PICK_INDENT, PICK_ROW_H, pick_row, row_cell};
pub use status_dot::{
    PRESENCE_AWAY, PRESENCE_HERE, PRESENCE_QUIET, presence_dot, present_ink, status_dot,
};
