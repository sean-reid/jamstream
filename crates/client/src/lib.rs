//! JamStream desktop client. The UI talks to audio and networking only
//! through the [`runtime::Runtime`] trait; this pass ships the complete
//! interface against that contract with [`demo::DemoRuntime`] behind it.
//! The second pass plugs the real session core in without touching screens.

pub mod app;
pub mod avatar;
pub mod creds;
pub mod demo;
pub mod exec;
pub mod live;
pub mod logging;
pub mod picker;
pub mod prefs;
pub mod reveal;
pub mod runtime;
pub mod screens;
pub mod theme;
pub mod widgets;
