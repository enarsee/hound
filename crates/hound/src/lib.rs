//! hound frontend.

pub use hound_core;

pub mod herdr;
pub mod program;
pub mod text;
pub mod ui;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
