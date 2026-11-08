//! Document naming: derive the next name from the previous one (doc → doc2 → doc3)
//! and avoid collisions with files already in the target folder.

use std::path::{Path, PathBuf};

/// Splits a trailing counter off a name: `"doc12"` → `("doc", Some("12"))`.
fn split_counter(name: &str) -> (&str, Option<&str>) {
    let digits = name.chars().rev().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits == name.len() {
        (name, None)
    } else {
        let at = name.len() - digits;
        (&name[..at], Some(&name[at..]))
    }
}

/// The name that follows `prev` in its numbering scheme.
///
/// `doc` → `doc2`, `doc2` → `doc3`, `doc9` → `doc10`, `scan_007` → `scan_008`.
/// A name that is only digits counts up as well: `42` → `43`.
pub fn next_name(prev: &str) -> String {
    let prev = prev.trim();
    if prev.is_empty() {
        return "scan".into();
    }
    if prev.chars().all(|c| c.is_ascii_digit()) {
        return increment(prev);
    }
    match split_counter(prev) {
        (base, Some(num)) => format!("{base}{}", increment(num)),
        (base, None) => format!("{base}2"),
    }
}

/// Increments a decimal string, keeping zero padding (`007` → `008`, `099` → `100`).
fn increment(num: &str) -> String {
    let width = num.len();
    let n: u128 = num.parse().unwrap_or(0);
    format!("{:0width$}", n + 1, width = width)
}

/// Target paths for a document with the given name: `name.ext` for a single
/// file, `name-p1.ext`, `name-p2.ext`, … when the scan produced several files.
pub fn target_paths(dir: &Path, name: &str, ext: &str, count: usize) -> Vec<PathBuf> {
    if count <= 1 {
        vec![dir.join(format!("{name}.{ext}"))]
    } else {
        (1..=count)
            .map(|i| dir.join(format!("{name}-p{i}.{ext}")))
            .collect()
    }
}

/// Returns `name` if none of its target files exist yet, otherwise the first
/// name in its numbering scheme whose files are all free.
pub fn unique_name(dir: &Path, name: &str, ext: &str, count: usize) -> String {
    let mut candidate = name.trim().to_string();
    if candidate.is_empty() {
        candidate = "scan".into();
    }
    for _ in 0..100_000 {
        let taken = target_paths(dir, &candidate, ext, count)
            .iter()
            .any(|p| p.exists());
        if !taken {
            return candidate;
        }
        candidate = next_name(&candidate);
    }
    candidate
}

/// Names for `count` separate single-file documents, continuing the numbering
/// scheme and skipping names that are taken: `doc`, `doc2`, `doc3`, …
pub fn sequence_names(dir: &Path, name: &str, ext: &str, count: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(count);
    let mut candidate = unique_name(dir, name, ext, 1);
    while out.len() < count {
        out.push(candidate.clone());
        candidate = unique_name(dir, &next_name(&candidate), ext, 1);
    }
    out
}

/// Characters that are not allowed in file names on common platforms.
pub fn sanitize(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim_matches('.')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbering_scheme() {
        assert_eq!(next_name("doc"), "doc2");
        assert_eq!(next_name("doc2"), "doc3");
        assert_eq!(next_name("doc9"), "doc10");
        assert_eq!(next_name("scan_007"), "scan_008");
        assert_eq!(next_name("Rechnung 2-"), "Rechnung 2-2");
        assert_eq!(next_name("42"), "43");
        assert_eq!(next_name(""), "scan");
    }

    #[test]
    fn unique_skips_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.pdf"), b"x").unwrap();
        std::fs::write(dir.path().join("doc2.pdf"), b"x").unwrap();
        assert_eq!(unique_name(dir.path(), "doc", "pdf", 1), "doc3");
        assert_eq!(unique_name(dir.path(), "doc", "png", 1), "doc");
        std::fs::write(dir.path().join("img-p2.png"), b"x").unwrap();
        assert_eq!(unique_name(dir.path(), "img", "png", 3), "img2");
    }

    #[test]
    fn sequence_continues_scheme() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc2.pdf"), b"x").unwrap();
        assert_eq!(sequence_names(dir.path(), "doc", "pdf", 3), vec!["doc", "doc3", "doc4"]);
        assert_eq!(sequence_names(dir.path(), "inv007", "pdf", 2), vec!["inv007", "inv008"]);
    }

    #[test]
    fn sanitizes() {
        assert_eq!(sanitize(" a/b:c "), "a_b_c");
    }
}
