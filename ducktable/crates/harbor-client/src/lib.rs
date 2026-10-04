//! DuckTable's Harbor client.
//!
//! The schema, paths, and state vocabulary come from `harbor-common`, so
//! this crate cannot drift from what harbor means by a name, a socket, or
//! a state. What lives here is the client half harbor also keeps to
//! itself: the blocking HTTP transport, and the fleet view a GUI needs —
//! every database a live socket or the config knows, discovered the same
//! way bare `harbor` discovers them.

pub mod catalog;
pub mod fleet;
pub mod http;
pub mod query;

pub use catalog::{catalog, catalog_lite, Catalog, Table};
pub use fleet::{connect_file, connect_remote, info, Conn};
pub use query::{
    exec, exec_checked, exec_within, query, session_end, session_new, session_open,
    session_release, Ended, Failure, QueryResult, Session,
};
pub use harbor_common::{paths, Level, State};
