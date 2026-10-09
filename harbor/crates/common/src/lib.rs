//! What harbor and ducktable both need: one definition each of where config
//! and state live, what a legal berth name is, and whether a file may be
//! trusted, imported by both binaries so the two never disagree.
//!
//! # Front ends
//!
//! Everything here is presentation-free except [`ui`], which is the terminal
//! renderer and is behind the default `term` feature. A GUI takes
//! `default-features = false` and gets the semantics without the ANSI:
//! [`state::State`] answers *what is this berth doing* and
//! [`state::Level`] answers *how alarming is that*, leaving each front end to
//! map a level onto its own palette. Nothing in this crate decides that a
//! running berth is `#22c55e`.

#[cfg(feature = "config")]
pub mod config;
pub mod duration;
#[cfg(feature = "membership")]
pub mod autostart;
#[cfg(feature = "membership")]
pub mod membership;
pub mod paths;
pub mod perms;
pub mod state;
#[cfg(feature = "term")]
pub mod ui;

pub use paths::{
    config_file, config_root, expand, history_file, log_file, looks_like_path, normalize,
    runtime_dir, sock_file, socket_for, state_root,
};
pub use state::{Level, State};
pub use perms::{chmod, create_dir_private, exposed};
