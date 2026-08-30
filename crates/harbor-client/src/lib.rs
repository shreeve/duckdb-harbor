//! DuckTable's Harbor client.
//!
//! The schema, paths, and berth-state vocabulary come from `harbor-common`,
//! so this crate cannot drift from what harbor and pilot mean by a name, a
//! socket, or a state. What lives here is the client half pilot also keeps
//! to itself: the blocking HTTP transport, token resolution (which shells
//! out for `token-cmd` and therefore never belongs in a shared crate the
//! server links), and the fleet view a GUI needs — every berth the config
//! or the runtime dir knows, with a probed [`harbor_common::State`].

pub mod catalog;
pub mod fleet;
pub mod http;
pub mod query;
pub mod tokens;

pub use catalog::{catalog, catalog_lite, Catalog, Table};
pub use fleet::{connect, info, keepalive, Conn};
pub use query::{query, QueryResult};
pub use harbor_common::{paths, Level, State};
