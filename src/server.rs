// The HTTP side of harbor.
//
// Shape (deliberately small):
//
//   POST /sql     run one statement, stream the NDJSON envelope back
//   GET  /health  liveness, no auth
//
// The envelope is the one thing that must not drift from the C++ harbor,
// because it is the contract every client already speaks:
//
//   {"type":"schema","columns":[{"name":"id","duckdbType":"BIGINT","lossless":true}]}
//   {"type":"row","values":[0,"row0"]}
//   {"type":"end","rowCount":3,"timeMs":2}
//
// Three properties of that envelope are load-bearing and easy to lose in a
// rewrite:
//
//   1. It streams. Rows go out as chunks arrive; a large result is never
//      materialised in memory first.
//   2. Types are carried per column (`duckdbType`, plus `decimal` width/scale
//      and nested `child`/`fields`), so a client can reconstruct exactly what
//      DuckDB had.
//   3. Values that JSON cannot hold losslessly are quoted, not emitted as bare
//      numbers. HUGEINT and large BIGINT go out as strings — a bare
//      123456789012345678901234567890 silently becomes 1.2345678901234568e+29
//      in any JavaScript client.
//
// One statement per request, on purpose: it makes SQL injection through
// string concatenation structurally impossible, and it keeps HTTP status
// codes meaningful. Multi-statement work belongs on a session.
//
// Concurrency: accept many connections, execute few queries. DuckDB
// parallelises a single query across all cores, so running hundreds
// concurrently buys thrashing, not throughput. A semaphore bounds in-flight
// statements and the queue returns 503 rather than growing without limit.

#![allow(dead_code)]

/// Bounded number of statements executing at once. Connections may greatly
/// exceed this; queries should not.
pub const DEFAULT_MAX_INFLIGHT: usize = 6;

/// Server-side default for how long a single statement may run before
/// `duckdb_interrupt` is used to cancel it. 0 disables the limit.
pub const DEFAULT_QUERY_TIMEOUT_S: u64 = 0;
