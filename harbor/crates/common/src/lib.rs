//! What harbor and ducktable both need.
//!
//! Before this crate each binary had its own copy of "where does config
//! live", "what is a legal berth name", and "may I trust this file" — and the
//! copies disagreed: harbor refused to start without `$HOME` while the client
//! quietly resolved to `./.config/harbor` and looked for sockets in a
//! relative directory. One definition, imported everywhere, is the point.
//!
//! # Front ends
//!
//! Everything here is presentation-free except [`ui`], which is the terminal
//! renderer and is behind the default `term` feature. A GUI takes
//! `default-features = false` and gets the semantics without the ANSI:
//! [`state::State`] answers *what is this berth doing* and
//! [`state::Level`] answers *how alarming is that*, leaving each front end to
//! map a level onto its own palette — a `Tone` in a terminal, a token in a
//! stylesheet. Nothing in this crate decides that a running berth is
//! `#22c55e`.

#[cfg(feature = "config")]
pub mod config;
pub mod lifetime;
pub mod paths;
pub mod perms;
pub mod state;
#[cfg(feature = "term")]
pub mod ui;

pub use paths::{
    config_file, config_root, expand, history_file, hold_file, lock_file, log_file, looks_like_path,
    normalize, runtime_dir, sidecar_file, sock_file, socket_for, state_root, token_file,
};
pub use lifetime::{Lifetime, Summoner};
pub use state::{Level, State};
pub use perms::{chmod, create_dir_private, exposed, write_private};
