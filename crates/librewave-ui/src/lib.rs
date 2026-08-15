#![doc = "The optional `LibreWave` desktop presentation shell."]

#[cfg(feature = "gpui")]
mod app;

#[cfg(feature = "gpui")]
pub use app::run;
