//! Manual smoke check for the Phase 3 hyper skeleton — not part of `cargo test`.
//! Run with: `cargo run -p pylon-server --example smoke`, then in another
//! shell: `curl http://127.0.0.1:5999/metrics`.

fn main() {
    let config = pylon_server::config::Config {
        database: pylon_server::config::DatabaseConfig {
            dsn: Some("postgresql://postgres:postgres@localhost:5418/app".into()),
            host: None,
            port: None,
            name: None,
            user: None,
            password: None,
            pool_min_size: 2,
            pool_max_size: 10,
        },
        project: pylon_server::config::ProjectConfig {
            schema_dir: std::path::PathBuf::from("."),
            name: Some("smoke".into()),
            pyql: None,
        },
        search: Default::default(),
        models: Default::default(),
        connections: Default::default(),
        webserver: pylon_server::config::WebserverConfig { host: "127.0.0.1".into(), port: 5999 },
        ui: Default::default(),
        metrics: pylon_server::config::MetricsConfig { enabled: true },
        cache: Default::default(),
        toml_path: std::path::PathBuf::from("pylon.toml"),
    };
    pylon_server::run(config).unwrap();
}
