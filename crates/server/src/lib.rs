//! The jamstreamd session server: a UDP driver around the sans-io
//! `ServerCore` from jamstream-session, plus the boot config it reads.

pub mod config;
pub mod revocations;
pub mod runtime;
