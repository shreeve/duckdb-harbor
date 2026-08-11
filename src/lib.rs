// harbor — HTTP /sql for DuckDB: SQL in, NDJSON out, no driver.
//
// This is the extension entry point. harbor registers a small SQL surface
// and nothing else; every other capability lives on the HTTP side.
//
// What harbor deliberately does NOT do, and why:
//
//   - It does not serve the DuckDB UI. The upstream `ui` extension serves
//     itself in the same process: LOAD ui; CALL start_ui_server();
//   - It does not implement a client/server protocol for DuckDB clients.
//     The upstream `quack` extension does that: CALL quack_serve(...).
//   - It does not terminate TLS. Put Caddy (or any reverse proxy) in front.
//
// harbor's job is the audience neither of those serves: clients that do not
// embed DuckDB and just want to POST SQL and read JSON back.

use duckdb::{
    Connection, Result,
    core::{DataChunkHandle, Inserter, LogicalTypeHandle, LogicalTypeId},
    duckdb_entrypoint_c_api,
    vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab},
};
use std::{
    error::Error,
    ffi::CString,
    sync::atomic::{AtomicBool, Ordering},
};

mod keywords;
mod server;

use server::DEFAULT_MAX_INFLIGHT;

/// Every function harbor exposes answers with one row and one VARCHAR
/// column. `init` tracks whether that row has been emitted; DuckDB calls
/// `func` until it returns an empty chunk.
struct OneShotInit {
    done: AtomicBool,
}

fn one_shot_init(_: &InitInfo) -> Result<OneShotInit, Box<dyn Error>> {
    Ok(OneShotInit { done: AtomicBool::new(false) })
}

/// Write `text` as the single output row, or close the stream if the row has
/// already gone out.
fn one_shot_emit(
    done: &AtomicBool,
    output: &mut DataChunkHandle,
    text: impl FnOnce() -> String,
) -> Result<(), Box<dyn Error>> {
    if done.swap(true, Ordering::Relaxed) {
        output.set_len(0);
        return Ok(());
    }
    output.flat_vector(0).insert(0, CString::new(text())?);
    output.set_len(1);
    Ok(())
}

fn varchar() -> LogicalTypeHandle {
    LogicalTypeHandle::from(LogicalTypeId::Varchar)
}

fn bigint() -> LogicalTypeHandle {
    LogicalTypeHandle::from(LogicalTypeId::Bigint)
}

// ---------------------------------------------------------------------------
// harbor_version() -> VARCHAR
//
// The smoke-test surface. If this answers, the extension loaded and harbor's
// symbols are reachable.
// ---------------------------------------------------------------------------

struct HarborVersion;

impl VTab for HarborVersion {
    type InitData = OneShotInit;
    type BindData = ();

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        bind.add_result_column("version", varchar());
        Ok(())
    }

    fn init(init: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        one_shot_init(init)
    }

    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        one_shot_emit(&func.get_init_data().done, output, || env!("CARGO_PKG_VERSION").to_string())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

// ---------------------------------------------------------------------------
// harbor_serve(bind := ..., port := ..., token := ..., workers := ...)
//
// Starts the listener and returns immediately with the bound address, so the
// caller keeps a usable session. Loopback and a required token are the
// defaults because the alternative — a database exposed to the network with
// no credential — should never be one keystroke away.
// ---------------------------------------------------------------------------

struct HarborServe;

struct ServeConfig {
    bind: String,
    port: u16,
    token: Option<String>,
    workers: usize,
    /// Present only when harbor generated the token, so it can be shown once.
    generated: bool,
}

impl VTab for HarborServe {
    type InitData = OneShotInit;
    type BindData = ServeConfig;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        bind.add_result_column("address", varchar());

        let host = bind
            .get_named_parameter("bind")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "127.0.0.1".to_string());

        let port = bind.get_named_parameter("port").map(|v| v.to_int64()).unwrap_or(9495);
        if !(0..=65535).contains(&port) {
            return Err(format!("harbor_serve: port {port} is out of range").into());
        }

        // No token means harbor mints one and prints it, not that the
        // endpoint is open. `token := ''` is the explicit opt-out.
        let (token, generated) = match bind.get_named_parameter("token").map(|v| v.to_string()) {
            Some(t) if t.is_empty() => (None, false),
            Some(t) => (Some(t), false),
            None => (Some(server::random_token()), true),
        };

        let workers = bind
            .get_named_parameter("workers")
            .map(|v| v.to_int64())
            .unwrap_or(DEFAULT_MAX_INFLIGHT as i64);
        if workers < 1 {
            return Err("harbor_serve: workers must be at least 1".into());
        }

        Ok(ServeConfig { bind: host, port: port as u16, token, workers: workers as usize, generated })
    }

    fn init(init: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        one_shot_init(init)
    }

    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        let cfg = func.get_bind_data();
        if func.get_init_data().done.swap(true, Ordering::Relaxed) {
            output.set_len(0);
            return Ok(());
        }
        let address = server::start(&cfg.bind, cfg.port, cfg.token.clone(), cfg.workers)?;
        let mut text = format!("http://{address}");
        if cfg.generated {
            // The only chance to see a generated token is here; it is never
            // stored and never echoed again.
            text.push_str("  token=");
            text.push_str(cfg.token.as_deref().unwrap_or(""));
        }
        output.flat_vector(0).insert(0, CString::new(text)?);
        output.set_len(1);
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }

    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        Some(vec![
            ("bind".to_string(), varchar()),
            ("port".to_string(), bigint()),
            ("token".to_string(), varchar()),
            ("workers".to_string(), bigint()),
        ])
    }
}

// ---------------------------------------------------------------------------
// harbor_stop() — stop the listener, drain the workers, CHECKPOINT
// ---------------------------------------------------------------------------

struct HarborStop;

impl VTab for HarborStop {
    type InitData = OneShotInit;
    type BindData = ();

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        bind.add_result_column("address", varchar());
        Ok(())
    }

    fn init(init: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        one_shot_init(init)
    }

    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        if func.get_init_data().done.swap(true, Ordering::Relaxed) {
            output.set_len(0);
            return Ok(());
        }
        let address = server::stop()?;
        output.flat_vector(0).insert(0, CString::new(address)?);
        output.set_len(1);
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

// ---------------------------------------------------------------------------
// harbor_wait() — block until the server stops
//
// This is what turns a DuckDB CLI invocation into a daemon: run harbor_serve,
// then harbor_wait, and the process stays up serving until something calls
// harbor_stop.
// ---------------------------------------------------------------------------

struct HarborWait;

impl VTab for HarborWait {
    type InitData = OneShotInit;
    type BindData = ();

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        bind.add_result_column("address", varchar());
        Ok(())
    }

    fn init(init: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        one_shot_init(init)
    }

    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        if func.get_init_data().done.swap(true, Ordering::Relaxed) {
            output.set_len(0);
            return Ok(());
        }
        let address = server::wait()?;
        output.flat_vector(0).insert(0, CString::new(address)?);
        output.set_len(1);
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

#[duckdb_entrypoint_c_api]
pub unsafe fn extension_entrypoint(con: Connection) -> Result<(), Box<dyn Error>> {
    con.register_table_function::<HarborVersion>("harbor_version")?;
    con.register_table_function::<HarborServe>("harbor_serve")?;
    con.register_table_function::<HarborStop>("harbor_stop")?;
    con.register_table_function::<HarborWait>("harbor_wait")?;

    // Open the worker connections. This has to happen here, inside the load
    // callback: the database handle an extension is given does not outlive
    // it, so a connection opened later fails. See server.rs.
    server::open_pool(con)?;

    Ok(())
}
