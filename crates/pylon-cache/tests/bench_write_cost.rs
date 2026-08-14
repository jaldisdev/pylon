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
//! What a cache write actually costs, split into its parts.
//!
//! `cache_put` measured ~2.9 ms for a 49-row result — several times the
//! query that produced it — and the question is how much of that is the
//! rkyv encode, how much is LMDB's page work, and how much is the fsync on
//! commit. Only the last one is avoidable without changing what is stored.
//!
//! Not a correctness test; run explicitly with:
//!
//! ```text
//! cargo test --release -p pylon-cache --test bench_write_cost -- --ignored --nocapture
//! ```

use std::time::Instant;

use pylon_cache::Cache;
use pylon_value::DecodedValue;

/// A row shaped like the benchmark schema's `Article`: 15 scalars plus a
/// nested object and a small array.
fn row(i: usize) -> DecodedValue {
    DecodedValue::Composite(vec![
        DecodedValue::Str("m::Article".to_string()),
        DecodedValue::Str(format!("Title {i}")),
        DecodedValue::Str(format!("slug-{i}")),
        DecodedValue::Str("body text ".repeat(20)),
        DecodedValue::Str("summary".into()),
        DecodedValue::I64(i as i64 * 10),
        DecodedValue::F64(4.5),
        DecodedValue::Bool(true),
        DecodedValue::Timestamptz(1_000_000_000),
        DecodedValue::Timestamptz(1_000_000_001),
        DecodedValue::Str("en".into()),
        DecodedValue::Str("seed".into()),
        DecodedValue::Str("abc".into()),
        DecodedValue::I64(500),
        DecodedValue::I64(3),
        DecodedValue::Uuid([7; 16]),
        DecodedValue::Composite(vec![
            DecodedValue::Str("m::Author".into()),
            DecodedValue::Str(format!("Author {i}")),
            DecodedValue::Str(format!("a{i}@example.com")),
        ]),
        DecodedValue::Array(vec![DecodedValue::Composite(vec![
            DecodedValue::Str("m::Tag".into()),
            DecodedValue::Str("red".into()),
        ])]),
    ])
}

fn rows() -> Vec<DecodedValue> {
    (0..49).map(row).collect()
}

fn time(label: &str, iterations: u32, mut f: impl FnMut(u32)) -> f64 {
    for i in 0..iterations.min(20) {
        f(i);
    }
    let mut best = f64::INFINITY;
    for round in 0..5 {
        let start = Instant::now();
        for i in 0..iterations {
            f(round * iterations + i);
        }
        best = best.min(start.elapsed().as_secs_f64() / iterations as f64 * 1e6);
    }
    println!("  {label:<54} {best:9.2} µs");
    best
}

#[test]
#[ignore = "benchmark, not a correctness test"]
fn write_cost_breakdown() {
    let data = rows();
    let tags = vec!["public.article".to_string()];

    println!("\nSERIALIZATION ONLY");
    let entry = pylon_value::CachedEntry {
        rows: data.clone(),
        tags: tags.clone(),
    };
    let encoded = rkyv::to_bytes::<rkyv::rancor::Error>(&entry).unwrap();
    println!("  (entry encodes to {} bytes)", encoded.len());
    time("rkyv::to_bytes", 2_000, |_| {
        std::hint::black_box(rkyv::to_bytes::<rkyv::rancor::Error>(&entry).unwrap());
    });

    println!("\nFULL PUT, DURABLE COMMIT");
    let durable = tempfile::tempdir().unwrap();
    let cache = Cache::open_durable(durable.path(), 64).unwrap();
    let t_durable = time("Cache::put", 200, |i| {
        cache.put(&format!("k{i}"), data.clone(), tags.clone()).unwrap();
    });

    println!("\nFULL PUT, DEFERRED SYNC (as shipped)");
    let deferred = tempfile::tempdir().unwrap();
    let cache = Cache::open(deferred.path(), 64).unwrap();
    let t_deferred = time("Cache::put", 200, |i| {
        cache.put(&format!("k{i}"), data.clone(), tags.clone()).unwrap();
    });

    println!("\n  Deferring the sync saves {:.0} µs per put ({:.1}x).", t_durable - t_deferred, t_durable / t_deferred);

    println!("\nEXPLICIT FLUSH");
    time("Cache::flush", 20, |_| {
        cache.flush().unwrap();
    });
}
