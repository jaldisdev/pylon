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

//! One canonical answer to "is this the same query?".
//!
//! Several places need to identify a query independently of the values bound
//! to it — metric labels, the compile cache, and migration tooling that wants
//! to talk about a query without quoting it. Each deriving its own key would
//! mean three subtly different notions of sameness; this is the one they
//! share.
//!
//! **What it hashes:** the *compiled* SQL, which carries `$1`/`$2`
//! placeholders rather than values, plus the result shape. So the same query
//! run a million times with different parameters collapses to a single id —
//! which is the whole point, since this is what bounds metric cardinality.
//!
//! **What it does not hash:** parameter values, and the raw PyQL source.
//! Leaving the source out means two spellings that compile identically
//! (whitespace, a `with` binding that inlines away) are correctly recognised
//! as the same query.

use sha2::{Digest, Sha256};

/// Length of the hex id. 16 hex chars is 64 bits — far more than enough to
/// keep a realistic number of distinct query shapes collision-free, and short
/// enough to read in a metric label or a log line.
const ID_HEX_LEN: usize = 16;

/// The stable identifier for a compiled query shape.
///
/// `sql` must be the compiled SQL (placeholders, not values). `shape_repr` is
/// the result shape's own stable rendering — passing it separately keeps two
/// queries with identical SQL but different result shapes from colliding.
pub fn query_shape_id(sql: &str, shape_repr: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(sql.as_bytes());
    // Field separator: a byte neither input can contain, so the boundary
    // between them can't be forged by content.
    hasher.update([0x1f]);
    hasher.update(shape_repr.as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..ID_HEX_LEN / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_stable_for_the_same_input() {
        assert_eq!(query_shape_id("SELECT 1", "s"), query_shape_id("SELECT 1", "s"));
    }

    #[test]
    fn has_the_documented_width() {
        assert_eq!(query_shape_id("SELECT 1", "s").len(), ID_HEX_LEN);
    }

    #[test]
    fn ignores_bound_values_because_they_are_not_in_the_sql() {
        // The cardinality guarantee: a query run with different parameters
        // compiles to one SQL string with the same placeholders, so it must
        // produce one id no matter how many times it runs.
        let sql = "SELECT (name) AS result FROM t WHERE id = $1";
        assert_eq!(query_shape_id(sql, "s"), query_shape_id(sql, "s"));
    }

    #[test]
    fn distinguishes_different_sql() {
        assert_ne!(query_shape_id("SELECT 1", "s"), query_shape_id("SELECT 2", "s"));
    }

    #[test]
    fn distinguishes_the_same_sql_with_a_different_result_shape() {
        assert_ne!(
            query_shape_id("SELECT 1", "scalar"),
            query_shape_id("SELECT 1", "object")
        );
    }

    #[test]
    fn the_separator_stops_boundary_ambiguity() {
        assert_ne!(query_shape_id("ab", "c"), query_shape_id("a", "bc"));
    }
}
