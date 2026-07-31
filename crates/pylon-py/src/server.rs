//! pyo3 binding for `pylon-server` — the native Rust HTTP server backing
//! `pylon serve`, replacing `uvicorn.run(create_app(config), ...)`.
//!
//! Deliberately blocking, not `future_into_py`: `pylon_server::run` builds
//! and owns its own multi-threaded Tokio runtime internally (a separate
//! one from the `pyo3_async_runtimes` runtime this module's sibling
//! bindings share — see this crate's own `_core` module init), and blocks
//! the calling thread until Ctrl-C. `py.detach` releases the GIL
//! for that whole duration so nothing else in the process needing it
//! (there is nothing else running here, `pylon serve` is this process's
//! only job, but the discipline matters generally) is blocked out.
//! Ctrl-C itself is handled by Tokio's own `signal::ctrl_c()` inside
//! `pylon_server::serve`, the same "last SIGINT handler registered wins"
//! dance `uvicorn` did before it — nothing new here.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

fn server_err(err: pylon_server::Error) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

/// Runs `pylon serve` to completion (blocks until Ctrl-C). `host`/`port`/
/// `ui_enabled` mirror `pylon serve`'s own CLI overrides
/// (`--host`/`--port`/`--ui`/`--no-ui`); everything else comes from
/// `pylon.toml` itself. `pylon.toml` discovery (walking up from the
/// current directory) now happens in Rust, identically to how Python's own
/// `requires_config` does it — both search from the process's actual
/// working directory, so this doesn't need a path passed in from Python.
///
/// `static_dir` is the built frontend's location — `pylon-server` has no
/// way to discover "where is the installed `pylon` package" on its own, so
/// `pylon/cli/commands/serve.py` computes it (`Path(pylon.__file__).parent
/// / "server" / "static"`, matching the old `asgi.py::STATIC_DIR`'s own
/// package-relative convention) and passes it straight through here.
#[pyfunction]
#[pyo3(signature = (host=None, port=None, ui_enabled=None, static_dir=None))]
fn run_server(
    py: Python<'_>,
    host: Option<String>,
    port: Option<u16>,
    ui_enabled: Option<bool>,
    static_dir: Option<String>,
) -> PyResult<()> {
    let mut config = pylon_server::load_config(None).map_err(server_err)?;
    if let Some(host) = host {
        config.webserver.host = host;
    }
    if let Some(port) = port {
        config.webserver.port = port;
    }
    if let Some(ui_enabled) = ui_enabled {
        config.ui.enabled = ui_enabled;
    }
    let static_dir = static_dir.map(std::path::PathBuf::from);
    py.detach(|| pylon_server::run(config, static_dir)).map_err(server_err)
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(run_server, m)?)?;
    Ok(())
}
