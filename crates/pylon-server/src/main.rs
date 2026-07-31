//! `pylon-server` — the standalone binary. Discovers `pylon.toml` by
//! walking up from the current directory (same as the `pylon` CLI's own
//! project discovery), unless `--config` points at one explicitly, then
//! runs the server to completion (blocks until Ctrl+C). No Python involved
//! anywhere in this path.

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: pylon-server [OPTIONS]

Run from within a Pylon project (or a subdirectory of one) containing
pylon.toml, or pass --config to point at one explicitly.

Options:
  --config PATH        Path to pylon.toml (skips the cwd-upward search)
  --host HOST          Override [webserver].host
  --port PORT          Override [webserver].port
  --ui / --no-ui        Override [ui].enabled
  --static-dir PATH     Serve the frontend build from this directory instead
                        of the one embedded into the binary at compile time
  -h, --help            Show this help and exit
";

struct Args {
    config: Option<PathBuf>,
    host: Option<String>,
    port: Option<u16>,
    ui_enabled: Option<bool>,
    static_dir: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { config: None, host: None, port: None, ui_enabled: None, static_dir: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--config" => args.config = Some(PathBuf::from(it.next().ok_or("--config requires a value")?)),
            "--host" => args.host = Some(it.next().ok_or("--host requires a value")?),
            "--port" => {
                let raw = it.next().ok_or("--port requires a value")?;
                args.port = Some(raw.parse::<u16>().map_err(|_| format!("--port: invalid port {raw:?}"))?);
            }
            "--ui" => args.ui_enabled = Some(true),
            "--no-ui" => args.ui_enabled = Some(false),
            "--static-dir" => args.static_dir = Some(PathBuf::from(it.next().ok_or("--static-dir requires a value")?)),
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

    if let Err(e) = pylon_server::run(config, args.static_dir) {
        eprintln!("pylon-server: error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
