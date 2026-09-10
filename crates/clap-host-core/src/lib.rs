//! CLAP host core — plugin loading, audio session, MIDI, presets, state, scan.

pub use clap_sys;

pub mod audio;
pub mod events;
pub mod host;
pub mod loader;
pub mod midi;
pub mod plugin_gui;
#[cfg(unix)]
pub mod posix_fd;
pub mod preset;
pub mod remote_controls;
pub mod scan;
pub mod state;
pub mod thread_pool;
#[cfg(windows)]
pub mod win32_embed;
