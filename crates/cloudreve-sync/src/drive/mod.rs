#[cfg(windows)]
pub mod callback;
#[cfg_attr(target_os = "macos", path = "macos/commands.rs")]
pub mod commands;
pub mod event_blocker;
pub mod ignore;
pub mod manager;
pub mod mounts;
#[cfg_attr(target_os = "macos", path = "macos/local_file.rs")]
pub mod placeholder;
#[cfg_attr(target_os = "macos", path = "macos/remote_events.rs")]
pub mod remote_events;
#[cfg_attr(target_os = "macos", path = "macos/sync.rs")]
pub mod sync;
pub mod utils;
