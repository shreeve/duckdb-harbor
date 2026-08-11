// harbor — HTTP /sql for DuckDB: SQL in, NDJSON out, no driver.
//
// This is the extension entry point. harbor registers a small SQL surface
// (five functions) and nothing else; every other capability lives on the
// HTTP side.
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

mod server;

/// One-shot table function scaffolding: bind declares the columns, init
/// tracks whether the single output row has been emitted yet.
struct OneShotInit {
    done: AtomicBool,
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
        bind.add_result_column("version", LogicalTypeHandle::from(LogicalTypeId::Varchar));
        Ok(())
    }

    fn init(_: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        Ok(OneShotInit {
            done: AtomicBool::new(false),
        })
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        if func.get_init_data().done.swap(true, Ordering::Relaxed) {
            output.set_len(0);
            return Ok(());
        }
        let vector = output.flat_vector(0);
        vector.insert(0, CString::new(env!("CARGO_PKG_VERSION"))?);
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

    // Still to come, in this order:
    //   harbor_serve(bind, port, token)  — start the HTTP listener
    //   harbor_stop()                    — stop it
    //   harbor_wait()                    — block; on SIGTERM drain + CHECKPOINT
    //   harbor_check_token(...)          — default authn callback
    //   harbor_nop_authorization(...)    — default authz callback

    Ok(())
}
