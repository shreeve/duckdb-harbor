//! DuckTable's Harbor client.
//!
//! The schema, paths, and state vocabulary come from `harbor-common`, and
//! the transport, sessions and summon from `harbor-http`, the client half
//! harbor's own CLI speaks through, so this crate cannot drift from what
//! harbor means by a name, a socket, a state, or an answer. What lives here
//! is what a GUI wants on top: results read whole, the catalog, and the
//! fleet — every database a live socket or the config knows, discovered the
//! same way bare `harbor` discovers them.

pub mod catalog;
pub mod fleet;
pub use harbor_http as http;
pub mod query;

pub use catalog::{catalog, catalog_lite, Catalog, Table};
pub use fleet::{connect_file, connect_remote, info, Conn};
pub use query::{
    exec, exec_checked, exec_within, query, session_end, session_new, session_open,
    session_release, session_renew, Ended, Failure, QueryResult, Session,
};
pub use harbor_common::{paths, Level, State};
