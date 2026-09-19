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
//! Timings for the per-execution work on the query hot path that a
//! Python-side benchmark cannot resolve, because a database round trip's
//! noise floor (hundreds of µs) is orders of magnitude above the signal.
//!
//! Not a correctness test and not part of the normal suite — `#[ignore]`d,
//! run explicitly with:
//!
//! ```text
//! cargo test -p pylon-core --test bench_hot_path -- --ignored --nocapture
//! ```

mod common;

use std::time::Instant;

use common::*;
use pylon_core::query;
use pylon_core::schema::{SchemaDescriptor, TypeDescriptor};

/// Mirrors `bench_boundary.py`'s `Article`: 15 properties, one link, one
/// multilink — a realistic wide-ish result shape, so the numbers below are
/// comparable to the Python-side ones measured against the same shape.
fn bench_schema(module: &str) -> SchemaDescriptor {
    let article_props = [
        "title",
        "slug",
        "body",
        "summary",
        "views",
        "rating",
        "published",
        "created_at",
        "updated_at",
        "locale",
        "source",
        "checksum",
        "word_count",
        "read_minutes",
        "external_id",
    ];
    let mut properties = vec![id_prop()];
    properties.extend(article_props.iter().map(|n| text_prop(n)));

    SchemaDescriptor {
        types: vec![
            TypeDescriptor {
                name: "Tag".into(),
                module: module.into(),
                table: "Tag".into(),
                properties: vec![id_prop(), text_prop("label")],
                materialized: true,
                ..empty_type()
            },
            TypeDescriptor {
                name: "Author".into(),
                module: module.into(),
                table: "Author".into(),
                properties: vec![id_prop(), text_prop("name"), text_prop("email")],
                materialized: true,
                ..empty_type()
            },
            TypeDescriptor {
                name: "Article".into(),
                module: module.into(),
                table: "Article".into(),
                properties,
                links: vec![link("author", &format!("{module}::Author"))],
                multilinks: vec![multilink("tags", &format!("{module}::Tag"))],
                materialized: true,
                ..empty_type()
            },
        ],
        ..Default::default()
    }
}

fn empty_type() -> TypeDescriptor {
    TypeDescriptor {
        name: String::new(),
        module: String::new(),
        table: String::new(),
        abstract_: false,
        materialized: false,
        description: None,
        parents: vec![],
        interfaces: vec![],
        properties: vec![],
        links: vec![],
        multilinks: vec![],
        computed: vec![],
        constraints: vec![],
        indexes: vec![],
        partition: None,
        vector_indexes: vec![],
        search_indexes: vec![],
        triggers: vec![],
        junction: false,
        signals: vec![],
    }
}

/// Minimum ns per iteration over several samples — the floor, for the same
/// reason `bench_boundary.py` reports one.
fn time_ns(label: &str, iterations: u32, mut f: impl FnMut()) -> f64 {
    for _ in 0..iterations.min(100) {
        f();
    }
    let mut best = f64::INFINITY;
    for _ in 0..7 {
        let start = Instant::now();
        for _ in 0..iterations {
            f();
        }
        let per = start.elapsed().as_nanos() as f64 / iterations as f64;
        best = best.min(per);
    }
    println!("  {label:<52} {:9.2} µs", best / 1000.0);
    best
}

#[test]
#[ignore = "benchmark, not a correctness test"]
fn hot_path_costs() {
    let module = unique_module("bench");
    let schema = bench_schema(&module);
    let pyql = format!(
        "select {module}::Article {{ \
           id, title, slug, body, summary, views, rating, published, \
           created_at, updated_at, locale, source, checksum, \
           word_count, read_minutes, external_id, \
           author: {{ id, name, email }}, \
           tags: {{ id, label }}, \
         }} filter .views > $min_views"
    );

    let compiled = query::compile(&pyql, &schema).expect("benchmark query must compile");
    println!(
        "\n  (SQL: {} bytes, {} params)",
        compiled.sql.len(),
        compiled.param_names.len()
    );

    println!("\nPER-EXECUTION WORK");
    let t_shape_id = time_ns("CompiledQuery::shape_id() — per execution", 20_000, || {
        std::hint::black_box(compiled.shape_id());
    });
    let t_derive = time_ns("  what deriving it costs, if it were not cached", 2_000, || {
        std::hint::black_box(query::derive_shape_id(&compiled.sql, &compiled.shape));
    });

    println!("\nCOMPILE PATH");
    let t_cached = time_ns("compile() — cache hit (hash, shard lock, Arc clone)", 20_000, || {
        std::hint::black_box(query::compile(&pyql, &schema).unwrap());
    });
    let t_clone = time_ns("  ...of which: Arc<CompiledQuery>::clone()", 20_000, || {
        std::hint::black_box(compiled.clone());
    });

    println!(
        "\n  Precomputing shape_id saves {:.2} µs per execution.",
        (t_derive - t_shape_id) / 1000.0
    );
    println!(
        "  A cache hit costs {:.2} µs, of which {:.2} µs is the Arc bump.\n",
        t_cached / 1000.0,
        t_clone / 1000.0
    );
}
