//
// This source file is part of the Pylon open source project.
//
// Copyright (c) 2026 Jaldis B.V.
//
// Licensed under the MIT OR Apache-2.0 license (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://opensource.org/licenses/MIT
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

//! `pylon-server` — the standalone binary. Discovers `pylon.toml` by
//! walking up from the current directory (same as the `pylon` CLI's own
//! project discovery), unless `--config` points at one explicitly, then
//! runs the server to completion (blocks until Ctrl+C). No Python involved
//! anywhere in this path.

use std::path::PathBuf;
use std::process::ExitCode;

use pylon_server::WorkerToggles;

const USAGE: &str = "\
Usage: pylon-server [OPTIONS]

Run from within a Pylon project (or a subdirectory of one) containing
pylon.toml, or pass --config to point at one explicitly.

Options:
  --config PATH           Path to pylon.toml (skips the cwd-upward search)
  --host HOST             Override [webserver].host
  --port PORT             Override [webserver].port
  --ui / --no-ui           Override [ui].enabled
  --static-dir PATH        Serve the frontend build from this directory instead
                           of the one embedded into the binary at compile time
  --vector-worker /        Force the vector-index worker on/off, regardless
  --disable-vector-worker  of whether the schema declares vector indexes
  --search-worker /        Force Meilisearch/OpenSearch index workers on/off,
  --disable-search-worker  regardless of [search] config
  --cache-worker /         Force the cache-invalidation worker on/off,
  --disable-cache-worker   regardless of [cache].enabled (forcing it on has
                           no effect if [cache].enabled = false, since no
                           cache handle was ever opened to attach it to)
  --http / --no-http       Bind a port and serve at all, or don't — with
                           --no-http, just run background workers and block
                           until Ctrl+C (a pure worker container, no API/UI)
  -h, --help               Show this help and exit
  -v, --version            Show the pylon-server version and exit

The --disable-*-worker flags are for deployments that run these workers in
their own process (`pylon worker start`) instead of in-process here; the
positive form opts back in if schema/config would otherwise skip a worker.
--no-http is the reverse split: a pure worker container with no API/UI.
Passing both forms of the same flag pair is an error.
";

struct Args {
    config: Option<PathBuf>,
    host: Option<String>,
    port: Option<u16>,
    ui_enabled: Option<bool>,
    static_dir: Option<PathBuf>,
    worker_toggles: WorkerToggles,
    http_enabled: Option<bool>,
}

/// Sets a `--flag`/`--opposite-flag` boolean pair, erroring if the
/// opposite form was already given.
fn set_flag(current: &mut Option<bool>, value: bool, flag: &str, opposite: &str) -> Result<(), String> {
    if *current == Some(!value) {
        return Err(format!("{flag} conflicts with {opposite} (both given)"));
    }
    *current = Some(value);
    Ok(())
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        config: None,
        host: None,
        port: None,
        ui_enabled: None,
        static_dir: None,
        worker_toggles: WorkerToggles::default(),
        http_enabled: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-v" | "--version" => {
                println!("pylon-server {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--config" => args.config = Some(PathBuf::from(it.next().ok_or("--config requires a value")?)),
            "--host" => args.host = Some(it.next().ok_or("--host requires a value")?),
            "--port" => {
                let raw = it.next().ok_or("--port requires a value")?;
                args.port = Some(raw.parse::<u16>().map_err(|_| format!("--port: invalid port {raw:?}"))?);
            }
            "--ui" => set_flag(&mut args.ui_enabled, true, "--ui", "--no-ui")?,
            "--no-ui" => set_flag(&mut args.ui_enabled, false, "--no-ui", "--ui")?,
            "--static-dir" => args.static_dir = Some(PathBuf::from(it.next().ok_or("--static-dir requires a value")?)),
            "--vector-worker" => {
                set_flag(&mut args.worker_toggles.vector, true, "--vector-worker", "--disable-vector-worker")?
            }
            "--disable-vector-worker" => {
                set_flag(&mut args.worker_toggles.vector, false, "--disable-vector-worker", "--vector-worker")?
            }
            "--search-worker" => {
                set_flag(&mut args.worker_toggles.search, true, "--search-worker", "--disable-search-worker")?
            }
            "--disable-search-worker" => {
                set_flag(&mut args.worker_toggles.search, false, "--disable-search-worker", "--search-worker")?
            }
            "--cache-worker" => {
                set_flag(&mut args.worker_toggles.cache, true, "--cache-worker", "--disable-cache-worker")?
            }
            "--disable-cache-worker" => {
                set_flag(&mut args.worker_toggles.cache, false, "--disable-cache-worker", "--cache-worker")?
            }
            "--http" => set_flag(&mut args.http_enabled, true, "--http", "--no-http")?,
            "--no-http" => set_flag(&mut args.http_enabled, false, "--no-http", "--http")?,
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("pylon-server: error: {e}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let config_result = match &args.config {
        Some(path) => pylon_server::load_config_at(path),
        None => pylon_server::load_config(None),
    };
    let mut config = match config_result {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pylon-server: error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(host) = args.host {
        config.webserver.host = host;
    }
    if let Some(port) = args.port {
        config.webserver.port = port;
    }
    if let Some(ui_enabled) = args.ui_enabled {
        config.ui.enabled = ui_enabled;
    }

    let no_http = args.http_enabled == Some(false);
    if let Err(e) = pylon_server::run(config, args.static_dir, args.worker_toggles, no_http) {
        eprintln!("pylon-server: error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
