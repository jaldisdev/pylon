//! Manual smoke check for pylon-server's ported endpoints — not part of
//! `cargo test`. Run with:
//!
//!     cargo run -p pylon-server --example smoke -- ~/Development/pylon-demo
//!
//! then in another shell:
//!     curl http://127.0.0.1:5656/api/connections
//!     curl http://127.0.0.1:5656/api/models
//!     curl http://127.0.0.1:5656/api/config-options
//!     curl http://127.0.0.1:5656/api/main/stats
//!     curl -X POST http://127.0.0.1:5656/api/main/query -d '{"pyql": "select 1 + 1"}'
//!     curl http://127.0.0.1:5656/metrics

fn main() {
    let dir = std::env::args().nth(1).expect("usage: smoke <dir containing pylon.toml>");
    let mut config = pylon_server::load_config(Some(std::path::Path::new(&dir))).expect("load pylon.toml");
    // Overridden away from pylon.toml's own [webserver] port to avoid
    // colliding with whatever else is already bound to it locally.
    config.webserver.port = 15656;
    println!(
        "loaded config for project {}, webserver {}:{}",
        config.project.name.as_deref().unwrap_or("<unnamed>"),
        config.webserver.host,
        config.webserver.port,
    );
    pylon_server::run(config).unwrap();
}
