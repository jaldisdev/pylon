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

//! Migration file parsing, ID computation, and chain validation.
//!
//! Every migration file has a two-line header:
//!   -- migration: m2<38-hex>
//!   -- onto: m2<38-hex> | initial
//!
//! The migration ID is `m2` + 19 bytes (38 hex chars) of a SHA-256 over the
//! body *and* the migration's position in the chain (`onto`, plus any
//! squashed IDs). The short ID used in filenames is the same digest cut to 6
//! bytes (12 hex chars). `m1` — a body-only hash with a 3-byte short form —
//! is still accepted for files already on disk; see `verify_integrity`.

use sha2::{Digest, Sha256};

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("migration file has no header: {0}")]
    MissingHeader(String),
    #[error("migration file has malformed header line: {0:?}")]
    MalformedHeader(String),
    #[error("migration ID mismatch in {filename}: header says {header_id}, body hashes to {computed_id}")]
    IdMismatch {
        filename: String,
        header_id: String,
        computed_id: String,
    },
    #[error("migration chain conflict: {0}")]
    ChainConflict(String),
    #[error("migration chain has a fork: both {a} and {b} claim onto {parent}")]
    Fork { parent: String, a: String, b: String },
    #[error("no migrations found")]
    Empty,
    #[error("unknown onto reference {onto} in migration {id}")]
    UnknownOnto { id: String, onto: String },
}

// ── Data types ────────────────────────────────────────────────────────────────

/// A parsed migration file.
#[derive(Debug, Clone)]
pub struct MigrationFile {
    /// Full migration ID: a version prefix (`m2`, or `m1` for a file
    /// written before the format change) + 38 hex chars.
    pub id: String,
    /// Parent's full ID, or the literal string `"initial"`.
    pub onto: String,
    /// IDs this migration squashes (§12); empty for normal migrations.
    pub squashed: Vec<String>,
    /// Original filename (stem only, e.g. `00001_m2a3f9bc12de4`).
    pub filename: String,
    /// Everything after the two header lines.
    pub body: String,
}

impl MigrationFile {
    /// Short ID used in filenames — the leading prefix of `id`, whose length
    /// depends on which format wrote the file: `m1` carried 6 hex chars,
    /// `m2` carries 12 (see `compute_short_id`).
    pub fn short_id(&self) -> &str {
        if self.id.starts_with("m1") {
            &self.id[..8]
        } else {
            &self.id[..14]
        }
    }

    /// Whether this is the "first" migration (onto == "initial").
    pub fn is_first(&self) -> bool {
        self.onto == "initial"
    }
}

// ── ID computation ────────────────────────────────────────────────────────────

/// Current ID format. `m1` hashed the body alone, which meant two migrations
/// with identical bodies at different points in the chain collided on one ID
/// — and `_pylon."Migrations"` keys on that ID, so the second apply
/// overwrote the first's tracking row. `m2` folds `onto` and the squash list
/// in, making the ID identify a migration's position in the chain and not
/// just its text.
const ID_VERSION: &str = "m2";

/// The digest every ID is derived from: the body, plus the chain position
/// that body sits at.
///
/// Field-separated with a byte that can't occur in any of the inputs, so
/// `(onto = "a", body = "bc")` and `(onto = "ab", body = "c")` can't hash
/// alike.
fn id_digest(body: &str, onto: &str, squashed: &[String]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    hasher.update([0x1f]);
    hasher.update(onto.as_bytes());
    for id in squashed {
        hasher.update([0x1f]);
        hasher.update(id.as_bytes());
    }
    hasher.finalize().into()
}

/// Compute the full migration ID (§5): `m2` + 38 hex chars of the digest.
pub fn compute_id(body: &str, onto: &str, squashed: &[String]) -> String {
    let digest = id_digest(body, onto, squashed);
    let hex = hex::encode(&digest[..19]); // 19 bytes = 38 hex chars
    format!("{ID_VERSION}{hex}")
}

/// Compute the short migration ID used as the filename component.
///
/// 12 hex chars (48 bits), not the 6 (24 bits) the `m1` format used: at 24
/// bits two migrations share a filename stem with ~50% probability by the
/// ~4,800th migration, and a few percent within the first few hundred.
pub fn compute_short_id(body: &str, onto: &str, squashed: &[String]) -> String {
    let digest = id_digest(body, onto, squashed);
    let hex = hex::encode(&digest[..6]); // 6 bytes = 12 hex chars
    format!("{ID_VERSION}{hex}")
}

/// The `m1` full ID for `body` — body-only hash, kept solely so migration
/// files written before the `m2` format still verify.
fn legacy_compute_id(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    format!("m1{}", hex::encode(&digest[..19]))
}

// ── Parsing ───────────────────────────────────────────────────────────────────

/// Parse a migration file's content. `filename` is used for error messages.
pub fn parse(content: &str, filename: &str) -> Result<MigrationFile, MigrationError> {
    let mut lines = content.splitn(3, '\n');

    let migration_line = lines
        .next()
        .ok_or_else(|| MigrationError::MissingHeader(filename.to_string()))?;
    let onto_line = lines
        .next()
        .ok_or_else(|| MigrationError::MissingHeader(filename.to_string()))?;
    let rest = lines.next().unwrap_or(""); // body (may be empty)

    let id = parse_header_line(migration_line, "migration")
        .ok_or_else(|| MigrationError::MalformedHeader(migration_line.to_string()))?;
    let onto =
        parse_header_line(onto_line, "onto").ok_or_else(|| MigrationError::MalformedHeader(onto_line.to_string()))?;

    // The body starts after the second newline.
    // Strip exactly one leading newline that separates headers from body.
    let body = rest.to_string();

    // Parse optional squashed: lines (§12)
    let squashed = parse_squashed_lines(content);

    Ok(MigrationFile {
        id: id.to_string(),
        onto: onto.to_string(),
        squashed,
        filename: filename.to_string(),
        body,
    })
}

/// Parse `-- key: value` and return `value`.
fn parse_header_line<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("-- {}:", key);
    let stripped = line.trim().strip_prefix(prefix.as_str())?;
    Some(stripped.trim())
}

/// Extract `-- squashed: id1, id2, ...` from the header block.
fn parse_squashed_lines(content: &str) -> Vec<String> {
    for line in content.lines() {
        if !line.starts_with("-- ") {
            break; // end of header block
        }
        if let Some(rest) = line.trim().strip_prefix("-- squashed:") {
            return rest
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    vec![]
}

// ── Integrity verification ─────────────────────────────────────────────────────

/// Re-hash a migration and verify it matches the ID in the header (§9.2).
///
/// Which hash to check against comes from the ID's own version prefix, so a
/// project with `m1` files on disk keeps verifying against the body-only
/// hash those files were written with. Only newly created migrations get
/// `m2` — there is no rewrite step and no flag day.
pub fn verify_integrity(m: &MigrationFile) -> Result<(), MigrationError> {
    let computed = if m.id.starts_with("m1") {
        legacy_compute_id(&m.body)
    } else {
        compute_id(&m.body, &m.onto, &m.squashed)
    };
    if computed != m.id {
        return Err(MigrationError::IdMismatch {
            filename: m.filename.clone(),
            header_id: m.id.clone(),
            computed_id: computed,
        });
    }
    Ok(())
}

// ── Chain validation ──────────────────────────────────────────────────────────

/// Validate that `migrations` form a single unbroken linear chain (§6).
///
/// Returns the migrations in chain order, from oldest (onto=initial) to newest (tip).
/// Errors on forks, cycles, broken references, or an empty input.
pub fn validate_chain(migrations: &[MigrationFile]) -> Result<Vec<&MigrationFile>, MigrationError> {
    if migrations.is_empty() {
        return Ok(vec![]);
    }

    use std::collections::HashMap;

    // Build a map from ID → migration.
    let by_id: HashMap<&str, &MigrationFile> = migrations.iter().map(|m| (m.id.as_str(), m)).collect();

    // Build a map from onto → child. Detect forks.
    let mut children: HashMap<&str, &MigrationFile> = HashMap::new();
    for m in migrations {
        if let Some(existing) = children.insert(m.onto.as_str(), m) {
            return Err(MigrationError::Fork {
                parent: m.onto.clone(),
                a: existing.id.clone(),
                b: m.id.clone(),
            });
        }
    }

    // Find the root (onto == "initial").
    let roots: Vec<_> = migrations.iter().filter(|m| m.onto == "initial").collect();
    match roots.len() {
        0 => return Err(MigrationError::ChainConflict("no migration with onto=initial".into())),
        2.. => {
            return Err(MigrationError::ChainConflict(format!(
                "multiple migrations claim onto=initial: {}",
                roots.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join(", ")
            )));
        }
        _ => {}
    }

    // Walk the chain from root to tip.
    let mut chain: Vec<&MigrationFile> = vec![];
    let mut current = roots[0];
    loop {
        // Verify onto reference exists (unless initial).
        if current.onto != "initial" && !by_id.contains_key(current.onto.as_str()) {
            return Err(MigrationError::UnknownOnto {
                id: current.id.clone(),
                onto: current.onto.clone(),
            });
        }
        chain.push(current);
        match children.get(current.id.as_str()) {
            None => break, // reached the tip
            Some(next) => current = next,
        }
    }

    // Sanity: chain length should match input length (no orphans/cycles).
    if chain.len() != migrations.len() {
        return Err(MigrationError::ChainConflict(format!(
            "chain has {} entries but {} files exist — possible cycle or orphan",
            chain.len(),
            migrations.len()
        )));
    }

    Ok(chain)
}

// ── Tip resolution ────────────────────────────────────────────────────────────

/// Return the tip migration ID from an ordered chain, or `"initial"` if empty.
pub fn chain_tip<'a>(chain: &[&'a MigrationFile]) -> &'a str {
    chain.last().map(|m| m.id.as_str()).unwrap_or("initial")
}

// ── Blank migration body ──────────────────────────────────────────────────────

/// Generate the stub body for a blank migration (§8.5).
/// Includes the leading blank line that separates the header from SQL content.
pub fn blank_body() -> &'static str {
    "\n-- TODO: write this migration's SQL by hand\n"
}

/// Build the full file content for a migration given `onto` and `body`.
///
/// `body` must include the leading blank line (the separator between header and SQL),
/// so that `parse` recovers the same bytes that `compute_id` hashed.
pub fn render_file(onto: &str, body: &str, squashed: &[String]) -> String {
    let id = compute_id(body, onto, squashed);
    let mut out = format!("-- migration: {}\n-- onto: {}\n", id, onto);
    if !squashed.is_empty() {
        out.push_str(&format!("-- squashed: {}\n", squashed.join(", ")));
    }
    out.push_str(body);
    out
}

// ── Step splitting ────────────────────────────────────────────────────────────

/// Split a migration body on `-- pylon:step` markers into `(transactional,
/// sql)` pairs, in order. A step is transactional unless its *preceding*
/// marker was `-- pylon:step non-transactional` (that marker applies to the
/// step it introduces, not the one it ends — matching the Python
/// implementation this replaces exactly, including the leading segment
/// before any marker always being transactional).
pub fn parse_steps(body: &str) -> Vec<(bool, String)> {
    let mut steps = Vec::new();
    let mut current_transactional = true;
    let mut current = String::new();

    for line in body.split_inclusive('\n') {
        match line.trim() {
            "-- pylon:step" => {
                steps.push((current_transactional, std::mem::take(&mut current)));
                current_transactional = true;
            }
            "-- pylon:step non-transactional" => {
                steps.push((current_transactional, std::mem::take(&mut current)));
                current_transactional = false;
            }
            _ => current.push_str(line),
        }
    }
    steps.push((current_transactional, current));
    steps
}

// ── Statement splitting ───────────────────────────────────────────────────────

/// Split one step's SQL into individual statements on top-level `;`.
///
/// Needed because `apply --dev-mode` has to be able to skip a *single*
/// already-applied statement rather than discarding the whole step (see
/// `migrate::apply_one`). A naive `split(';')` would corrupt every step that
/// contains a dollar-quoted function body — which is most of them, since
/// trigger and constraint DDL is emitted as `... AS $$ ... ; ... $$`.
///
/// Recognises the four places a `;` can appear without ending a statement:
/// single-quoted strings (`''` escapes), quoted identifiers (`""` escapes),
/// dollar-quoted bodies (`$$` or `$tag$`), and comments (`--` to end of line,
/// `/* */` which nest in PostgreSQL). Empty statements are dropped, so a
/// trailing `;` or a stray blank line never produces one.
pub fn split_statements(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        // A doubled quote is an escaped quote, not the end.
                        if bytes.get(i + 1) == Some(&quote) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 1usize;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'$' => match dollar_tag(bytes, i) {
                Some(tag) => {
                    i += tag.len();
                    // Scan to the matching closing tag; an unterminated body
                    // runs to end of input rather than looping forever.
                    match find_subslice(&bytes[i..], tag) {
                        Some(offset) => i += offset + tag.len(),
                        None => i = bytes.len(),
                    }
                }
                None => i += 1,
            },
            b';' => {
                let stmt = sql[start..i].trim();
                if !stmt.is_empty() {
                    out.push(stmt.to_string());
                }
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }

    let tail = sql[start..].trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

/// If a dollar-quote tag opens at `at`, return it (including both `$`s).
/// `$$` and `$tag$` open one; `$1` (a parameter) and a bare `$` do not.
fn dollar_tag(bytes: &[u8], at: usize) -> Option<&[u8]> {
    let mut j = at + 1;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        // A tag can't start with a digit — that's a `$1` placeholder.
        if j == at + 1 && bytes[j].is_ascii_digit() {
            return None;
        }
        j += 1;
    }
    if bytes.get(j) == Some(&b'$') {
        Some(&bytes[at..=j])
    } else {
        None
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_body(tag: &str) -> String {
        // Body includes the leading blank line (header-to-SQL separator).
        format!("\nCREATE TABLE \"{}\" ();\n", tag)
    }

    fn make_file(onto: &str, body: &str) -> MigrationFile {
        let content = render_file(onto, body, &[]);
        parse(&content, "test").unwrap()
    }

    #[test]
    fn test_compute_id_prefix() {
        let id = compute_id("hello", "initial", &[]);
        assert!(id.starts_with("m2"));
        assert_eq!(id.len(), 40); // "m2" + 38 hex
    }

    #[test]
    fn test_compute_short_id() {
        let sid = compute_short_id("hello", "initial", &[]);
        assert!(sid.starts_with("m2"));
        assert_eq!(sid.len(), 14); // "m2" + 12 hex
    }

    #[test]
    fn test_id_is_prefix_of_short_id() {
        let full = compute_id("SELECT 1;", "initial", &[]);
        let short = compute_short_id("SELECT 1;", "initial", &[]);
        assert!(full.starts_with(&short));
    }

    #[test]
    fn identical_bodies_at_different_chain_positions_get_different_ids() {
        // The `m1` collision: same DDL added, reverted, then re-added later
        // hashed to one ID, and `_pylon."Migrations"` keys on that ID.
        let body = "ALTER TABLE t ADD COLUMN c int8;";
        let first = compute_id(body, "initial", &[]);
        let second = compute_id(body, "m2aaaaaaaaaaaa", &[]);
        assert_ne!(first, second);
        assert_ne!(
            compute_short_id(body, "initial", &[]),
            compute_short_id(body, "m2aaaaaaaaaaaa", &[])
        );
    }

    #[test]
    fn the_squash_list_is_part_of_the_id() {
        let body = "SELECT 1;";
        assert_ne!(
            compute_id(body, "initial", &[]),
            compute_id(body, "initial", &["m1abc".to_string()])
        );
    }

    #[test]
    fn the_field_separator_stops_boundary_ambiguity() {
        // Without a separator these two would hash the same bytes.
        assert_ne!(compute_id("bc", "a", &[]), compute_id("c", "ab", &[]));
    }

    #[test]
    fn a_legacy_m1_file_still_verifies_against_the_body_only_hash() {
        let body = "\nSELECT 1;\n";
        let legacy_id = legacy_compute_id(body);
        let content = format!("-- migration: {legacy_id}\n-- onto: initial\n{body}");
        let m = parse(&content, "00001_legacy").unwrap();
        assert!(
            verify_integrity(&m).is_ok(),
            "an m1 file written before the format change must keep verifying"
        );
        assert_eq!(m.short_id().len(), 8, "m1 short IDs stay 6 hex chars wide");
    }

    #[test]
    fn test_parse_and_verify() {
        let body = make_body("Person");
        let content = render_file("initial", &body, &[]);
        let m = parse(&content, "00001_m1abc123").unwrap();
        assert_eq!(m.onto, "initial");
        assert!(m.id.starts_with("m2"));
        verify_integrity(&m).unwrap();
    }

    #[test]
    fn test_verify_detects_tampering() {
        let body = make_body("Person");
        let content = render_file("initial", &body, &[]);
        let mut m = parse(&content, "test").unwrap();
        m.body = "TAMPERED;\n".to_string();
        assert!(verify_integrity(&m).is_err());
    }

    #[test]
    fn test_chain_single() {
        let m1 = make_file("initial", &make_body("A"));
        let files = [m1];
        let chain = validate_chain(&files).unwrap();
        assert_eq!(chain.len(), 1);
        assert!(chain[0].is_first());
    }

    #[test]
    fn test_chain_ordered() {
        let m1 = make_file("initial", &make_body("A"));
        let m2 = make_file(&m1.id, &make_body("B"));
        let m3 = make_file(&m2.id, &make_body("C"));
        // Pass in reverse order — validate_chain should still sort correctly.
        let files = [m3.clone(), m1.clone(), m2.clone()];
        let chain = validate_chain(&files).unwrap();
        assert_eq!(chain[0].id, m1.id);
        assert_eq!(chain[1].id, m2.id);
        assert_eq!(chain[2].id, m3.id);
    }

    #[test]
    fn test_chain_fork_detected() {
        let m1 = make_file("initial", &make_body("A"));
        let m2a = make_file(&m1.id, &make_body("B"));
        let m2b = make_file(&m1.id, &make_body("C"));
        let files = [m1, m2a, m2b];
        assert!(validate_chain(&files).is_err());
    }

    #[test]
    fn test_empty_chain() {
        assert!(validate_chain(&[]).unwrap().is_empty());
    }

    #[test]
    fn test_squashed_header() {
        let ids = vec!["m1aaa".to_string(), "m1bbb".to_string()];
        let body = make_body("Squashed");
        let content = render_file("initial", &body, &ids);
        let m = parse(&content, "test").unwrap();
        assert_eq!(m.squashed, ids);
    }

    #[test]
    fn test_blank_body_stable() {
        // Blank body must be stable so its hash is consistent.
        assert_eq!(blank_body(), "\n-- TODO: write this migration's SQL by hand\n");
    }

    #[test]
    fn test_parse_steps_no_markers_is_one_transactional_step() {
        let steps = parse_steps("CREATE TABLE foo ();\n");
        assert_eq!(steps, vec![(true, "CREATE TABLE foo ();\n".to_string())]);
    }

    #[test]
    fn test_parse_steps_splits_on_transactional_marker() {
        let steps = parse_steps("CREATE TABLE a ();\n-- pylon:step\nCREATE TABLE b ();\n");
        assert_eq!(
            steps,
            vec![
                (true, "CREATE TABLE a ();\n".to_string()),
                (true, "CREATE TABLE b ();\n".to_string()),
            ]
        );
    }

    #[test]
    fn test_parse_steps_non_transactional_marker_applies_to_the_next_step() {
        let steps = parse_steps(
            "CREATE TABLE a ();\n-- pylon:step non-transactional\nCREATE INDEX CONCURRENTLY idx ON a (x);\n",
        );
        assert_eq!(
            steps,
            vec![
                (true, "CREATE TABLE a ();\n".to_string()),
                (false, "CREATE INDEX CONCURRENTLY idx ON a (x);\n".to_string()),
            ]
        );
    }

    #[test]
    fn test_parse_steps_reverts_to_transactional_after_a_plain_marker() {
        let steps = parse_steps(
            "-- pylon:step non-transactional\nCREATE INDEX CONCURRENTLY idx ON a (x);\n\
             -- pylon:step\nCREATE TABLE b ();\n",
        );
        assert_eq!(steps.len(), 3);
        assert!(steps[0].0); // leading empty segment, transactional
        assert!(!steps[1].0);
        assert!(steps[2].0);
    }

    #[test]
    fn test_parse_steps_empty_body() {
        assert_eq!(parse_steps(""), vec![(true, String::new())]);
    }

    // ── split_statements ──────────────────────────────────────────────────

    #[test]
    fn split_statements_splits_on_plain_semicolons() {
        assert_eq!(
            split_statements("CREATE TABLE a (id int8); CREATE TABLE b (id int8);"),
            vec!["CREATE TABLE a (id int8)", "CREATE TABLE b (id int8)"]
        );
    }

    #[test]
    fn split_statements_drops_empty_and_trailing_statements() {
        assert_eq!(split_statements("SELECT 1;;\n\n;"), vec!["SELECT 1"]);
        assert_eq!(split_statements(""), Vec::<String>::new());
        assert_eq!(split_statements("   \n  "), Vec::<String>::new());
    }

    #[test]
    fn split_statements_keeps_a_statement_without_a_trailing_semicolon() {
        assert_eq!(split_statements("SELECT 1"), vec!["SELECT 1"]);
    }

    #[test]
    fn split_statements_ignores_semicolons_in_string_literals() {
        assert_eq!(
            split_statements("INSERT INTO t VALUES ('a;b'); SELECT 1;"),
            vec!["INSERT INTO t VALUES ('a;b')", "SELECT 1"]
        );
    }

    #[test]
    fn split_statements_handles_doubled_quote_escapes() {
        assert_eq!(
            split_statements("SELECT 'it''s; fine'; SELECT 2;"),
            vec!["SELECT 'it''s; fine'", "SELECT 2"]
        );
    }

    #[test]
    fn split_statements_ignores_semicolons_in_quoted_identifiers() {
        assert_eq!(
            split_statements("CREATE TABLE \"weird;name\" (id int8); SELECT 1;"),
            vec!["CREATE TABLE \"weird;name\" (id int8)", "SELECT 1"]
        );
    }

    #[test]
    fn split_statements_ignores_semicolons_in_line_comments() {
        assert_eq!(
            split_statements("SELECT 1; -- trailing; comment\nSELECT 2;"),
            vec!["SELECT 1", "-- trailing; comment\nSELECT 2"]
        );
    }

    #[test]
    fn split_statements_ignores_semicolons_in_block_comments() {
        assert_eq!(
            split_statements("SELECT 1 /* a; b */; SELECT 2;"),
            vec!["SELECT 1 /* a; b */", "SELECT 2"]
        );
    }

    #[test]
    fn split_statements_handles_nested_block_comments() {
        assert_eq!(
            split_statements("SELECT 1 /* a /* b; */ c; */; SELECT 2;"),
            vec!["SELECT 1 /* a /* b; */ c; */", "SELECT 2"]
        );
    }

    #[test]
    fn split_statements_keeps_a_dollar_quoted_body_intact() {
        // This is the case a naive split(';') corrupts: the function body has
        // two internal semicolons that must not end the CREATE FUNCTION.
        let sql = "CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$\n\
                   BEGIN\n  PERFORM 1;\n  RETURN NEW;\nEND;\n$$;\nSELECT 1;";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 2, "got {stmts:#?}");
        assert!(stmts[0].starts_with("CREATE FUNCTION f()"));
        assert!(stmts[0].ends_with("$$"));
        assert_eq!(stmts[1], "SELECT 1");
    }

    #[test]
    fn split_statements_keeps_a_tagged_dollar_quoted_body_intact() {
        let sql = "CREATE FUNCTION f() RETURNS int8 AS $body$ SELECT 1; $body$ LANGUAGE sql; SELECT 2;";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 2, "got {stmts:#?}");
        assert!(stmts[0].contains("$body$ SELECT 1; $body$"));
        assert_eq!(stmts[1], "SELECT 2");
    }

    #[test]
    fn split_statements_handles_the_do_block_enum_form() {
        // Exactly what `export::emit_enum` produces.
        let sql = "DO $$ BEGIN CREATE TYPE s.t AS ENUM ('a'); EXCEPTION WHEN duplicate_object THEN NULL; END $$;";
        assert_eq!(split_statements(sql).len(), 1);
    }

    #[test]
    fn split_statements_treats_dollar_digit_as_a_placeholder_not_a_tag() {
        assert_eq!(
            split_statements("SELECT $1; SELECT $2;"),
            vec!["SELECT $1", "SELECT $2"]
        );
    }

    #[test]
    fn split_statements_does_not_hang_on_an_unterminated_dollar_body() {
        let stmts = split_statements("CREATE FUNCTION f() AS $$ SELECT 1;");
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn split_statements_does_not_hang_on_an_unterminated_string() {
        let stmts = split_statements("SELECT 'oops;");
        assert_eq!(stmts.len(), 1);
    }
}
