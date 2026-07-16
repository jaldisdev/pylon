use super::{f, B, E};
use super::{Array, FnDescriptor, Int64, Str, Tuple};

pub(super) fn build() -> Vec<FnDescriptor> {
    vec![
        f("sys", "get_current_database",
            vec![],
            Str,
            B("current_database")),

        f("sys", "get_version_as_str",
            vec![],
            Str,
            E(concat!("'", env!("CARGO_PKG_VERSION"), "'"))),

        f("sys", "get_version",
            vec![],
            Tuple(vec![Int64, Int64, Str, Int64, Array(Box::new(Str))]),
            E(env!("PYLON_VERSION_ROW"))),
    ]
}
