//! Registry of known Pylon session config options — Rust port of
//! `pylon/config_options.py`. Backs `GET /api/config-options`.

pub struct ConfigOptionSpec {
    pub name: &'static str,
    pub type_name: &'static str,
    pub default: bool,
}

/// The only session config option `pylon_core::ir::SessionConfig` knows
/// today. Adding a new one: add it here, thread it through
/// `pylon-client`'s param-binding/`SessionConfig`, and add it here.
pub const CONFIG_OPTIONS: &[ConfigOptionSpec] =
    &[ConfigOptionSpec { name: "allow_user_specified_id", type_name: "bool", default: false }];
