use super::{f, p, E};
use super::{Bytes, FnDescriptor, Int64, Str};
use super::B;

/// Straight passthrough to PostgreSQL's `pgcrypto` extension (`CREATE
/// EXTENSION IF NOT EXISTS pgcrypto;` must be run on the target database —
/// same expectation as `pgvector`'s `vector` extension, neither of which
/// Pylon auto-provisions).
pub(super) fn build() -> Vec<FnDescriptor> {
    vec![
        f("crypto", "digest", vec![p("data", Str),   p("type", Str)], Bytes, B("digest")),
        f("crypto", "digest", vec![p("data", Bytes), p("type", Str)], Bytes, B("digest")),
        f("crypto", "hmac",   vec![p("data", Str),   p("key", Str),   p("type", Str)], Bytes, B("hmac")),
        f("crypto", "hmac",   vec![p("data", Bytes), p("key", Bytes), p("type", Str)], Bytes, B("hmac")),
        // Zero-arg form defaults to blowfish ("bf").
        f("crypto", "gen_salt", vec![],                                          Str, E("gen_salt('bf')")),
        f("crypto", "gen_salt", vec![p("type", Str)],                            Str, B("gen_salt")),
        // pgcrypto's gen_salt(type, iter_count) takes iter_count as int4; Pylon's
        // int64 needs an explicit narrowing cast (PG has no implicit int8 -> int4).
        f("crypto", "gen_salt", vec![p("type", Str), p("iter_count", Int64)],    Str, E("gen_salt($1, $2::int4)")),
        f("crypto", "crypt",  vec![p("password", Str), p("salt", Str)],         Str, B("crypt")),
    ]
}
