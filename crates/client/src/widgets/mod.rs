//! Custom controls drawn with the painter. A fader is a line and a handle;
//! meters are the one colorful place because there color is information.

mod fader;
mod lamp;
mod meter;
mod pan;
mod status_dot;

pub use fader::{FADER_DEFAULT_DB, FADER_MAX_DB, FADER_MIN_DB, fader};
pub use lamp::on_air;
pub use meter::{Meter, meter};
pub use pan::pan_slider;
pub use status_dot::status_dot;
