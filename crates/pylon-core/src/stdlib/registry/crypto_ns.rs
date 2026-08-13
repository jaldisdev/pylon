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

use super::B;
use super::{Bytes, FnDescriptor, Int64, Str};
use super::{E, f, p};
use crate::stdlib::FnVolatility::Volatile;

/// Straight passthrough to PostgreSQL's `pgcrypto` extension (`CREATE
/// EXTENSION IF NOT EXISTS pgcrypto;` must be run on the target database —
/// same expectation as `pgvector`'s `vector` extension, neither of which
/// Pylon auto-provisions).
pub(super) fn build() -> Vec<FnDescriptor> {
    vec![
        f(
            "crypto",
            "digest",
            vec![p("data", Str), p("type", Str)],
            Bytes,
            B("digest"),
        ),
        f(
            "crypto",
            "digest",
            vec![p("data", Bytes), p("type", Str)],
            Bytes,
            B("digest"),
        ),
        f(
            "crypto",
            "hmac",
            vec![p("data", Str), p("key", Str), p("type", Str)],
            Bytes,
            B("hmac"),
        ),
        f(
            "crypto",
            "hmac",
            vec![p("data", Bytes), p("key", Bytes), p("type", Str)],
            Bytes,
            B("hmac"),
        ),
        // Zero-arg form defaults to blowfish ("bf").
        f("crypto", "gen_salt", vec![], Str, E("gen_salt('bf')")).vol(Volatile),
        f("crypto", "gen_salt", vec![p("type", Str)], Str, B("gen_salt")).vol(Volatile),
        // pgcrypto's gen_salt(type, iter_count) takes iter_count as int4; Pylon's
        // int64 needs an explicit narrowing cast (PG has no implicit int8 -> int4).
        f(
            "crypto",
            "gen_salt",
            vec![p("type", Str), p("iter_count", Int64)],
            Str,
            E("gen_salt($1, $2::int4)"),
        )
        .vol(Volatile),
        f(
            "crypto",
            "crypt",
            vec![p("password", Str), p("salt", Str)],
            Str,
            B("crypt"),
        ),
    ]
}
