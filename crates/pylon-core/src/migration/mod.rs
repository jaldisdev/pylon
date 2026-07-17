//! Migration file parsing, ID computation, and chain validation.
//!
//! Every migration file has a two-line header:
//!   -- migration: m1<38-hex>
//!   -- onto: m1<38-hex> | initial
//!
//! The migration ID is `m1` + first 19 bytes (38 hex chars) of SHA-256(body).
//! The short ID (used in filenames) is `m1` + first 3 bytes (6 hex chars).

use sha2::{Digest, Sha256};

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("migration file has no header: {0}")]
    MissingHeader(String),
    #[error("migration file has malformed header line: {0:?}")]
    MalformedHeader(String),
    #[error("migration ID mismatch in {filename}: header says {header_id}, body hashes to {computed_id}")]
    IdMismatch { filename: String, header_id: String, computed_id: String },
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
    /// Full migration ID: `m1` + 38 hex chars.
    pub id: String,
    /// Parent's full ID, or the literal string `"initial"`.
    pub onto: String,
    /// IDs this migration squashes (§12); empty for normal migrations.
    pub squashed: Vec<String>,
    /// Original filename (stem only, e.g. `00001_m1a3f9bc`).
    pub filename: String,
    /// Everything after the two header lines.
    pub body: String,
}

impl MigrationFile {
    /// Short ID used in filenames: `m1` + first 6 hex chars of SHA-256(body).
    pub fn short_id(&self) -> &str {
        &self.id[..8] // "m1" + 6 hex chars
    }

    /// Whether this is the "first" migration (onto == "initial").
    pub fn is_first(&self) -> bool {
        self.onto == "initial"
    }
}

// ── ID computation ────────────────────────────────────────────────────────────

/// Compute the full migration ID from a body string (§5).
/// Returns `m1` + first 38 hex chars of SHA-256(body bytes).
pub fn compute_id(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    let hex = hex::encode(&digest[..19]); // 19 bytes = 38 hex chars
    format!("m1{}", hex)
}

/// Compute the short migration ID (filename component) from a body string.
/// Returns `m1` + first 6 hex chars of SHA-256(body bytes).
pub fn compute_short_id(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    let hex = hex::encode(&digest[..3]); // 3 bytes = 6 hex chars
    format!("m1{}", hex)
}

// ── Parsing ───────────────────────────────────────────────────────────────────

/// Parse a migration file's content. `filename` is used for error messages.
pub fn parse(content: &str, filename: &str) -> Result<MigrationFile, MigrationError> {
    let mut lines = content.splitn(3, '\n');

    let migration_line = lines.next()
        .ok_or_else(|| MigrationError::MissingHeader(filename.to_string()))?;
    let onto_line = lines.next()
        .ok_or_else(|| MigrationError::MissingHeader(filename.to_string()))?;
    let rest = lines.next().unwrap_or(""); // body (may be empty)

    let id = parse_header_line(migration_line, "migration")
        .ok_or_else(|| MigrationError::MalformedHeader(migration_line.to_string()))?;
    let onto = parse_header_line(onto_line, "onto")
        .ok_or_else(|| MigrationError::MalformedHeader(onto_line.to_string()))?;

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
            return rest.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    vec![]
}

// ── Integrity verification ─────────────────────────────────────────────────────

/// Re-hash a migration's body and verify it matches the ID in the header (§9.2).
pub fn verify_integrity(m: &MigrationFile) -> Result<(), MigrationError> {
    let computed = compute_id(&m.body);
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
    let by_id: HashMap<&str, &MigrationFile> = migrations.iter()
        .map(|m| (m.id.as_str(), m))
        .collect();

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
        2.. => return Err(MigrationError::ChainConflict(
            format!("multiple migrations claim onto=initial: {}",
                roots.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join(", "))
        )),
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
        return Err(MigrationError::ChainConflict(
            format!("chain has {} entries but {} files exist — possible cycle or orphan",
                chain.len(), migrations.len())
        ));
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
    let id = compute_id(body);
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
        let id = compute_id("hello");
        assert!(id.starts_with("m1"));
        assert_eq!(id.len(), 40); // "m1" + 38 hex
    }

    #[test]
    fn test_compute_short_id() {
        let sid = compute_short_id("hello");
        assert!(sid.starts_with("m1"));
        assert_eq!(sid.len(), 8); // "m1" + 6 hex
    }

    #[test]
    fn test_id_is_prefix_of_short_id() {
        let body = "SELECT 1;";
        let full = compute_id(body);
        let short = compute_short_id(body);
        assert!(full.starts_with(&short));
    }

    #[test]
    fn test_parse_and_verify() {
        let body = make_body("Person");
        let content = render_file("initial", &body, &[]);
        let m = parse(&content, "00001_m1abc123").unwrap();
        assert_eq!(m.onto, "initial");
        assert!(m.id.starts_with("m1"));
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
}
