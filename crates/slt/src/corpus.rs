//! The pinned test corpus. `manifest.txt` lists every file with its SHA-256,
//! and a run refuses to report a pass rate unless every file is present and
//! unchanged. That ties the headline number to an exact, checkable input.

use std::path::Path;

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusFile {
    pub sha256: String,
    /// Relative to the corpus root, with `/` separators.
    pub path: String,
}

/// Parses `<sha256>  <path>` lines, the format `shasum -a 256` prints.
/// Lines starting with `#` are comments.
pub fn parse_manifest(text: &str) -> Result<Vec<CorpusFile>, String> {
    let mut files = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (sha256, path) = line
            .split_once("  ")
            .ok_or_else(|| format!("manifest line {}: expected '<sha256>  <path>'", number + 1))?;
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("manifest line {}: bad sha256", number + 1));
        }
        files.push(CorpusFile {
            sha256: sha256.to_ascii_lowercase(),
            path: path.trim().to_string(),
        });
    }
    if files.is_empty() {
        return Err("manifest lists no files".into());
    }
    Ok(files)
}

/// Reads a `# key: value` header line from the manifest, such as the source
/// URL or the upstream check-in.
pub fn header(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix('#')?.trim();
        let (k, v) = rest.split_once(':')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}

/// Checks every file against its pinned hash and lists every problem found.
pub fn verify(root: &Path, files: &[CorpusFile]) -> Result<(), String> {
    let mut problems = Vec::new();
    for file in files {
        match std::fs::read(root.join(&file.path)) {
            Ok(bytes) => {
                let actual = format!("{:x}", Sha256::digest(&bytes));
                if actual != file.sha256 {
                    problems.push(format!("{}: checksum mismatch", file.path));
                }
            }
            Err(e) => problems.push(format!("{}: {e}", file.path)),
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_rejects_malformed_lines() {
        let good = format!("# comment\n{}  select1.test\n", "a".repeat(64));
        assert_eq!(parse_manifest(&good).unwrap()[0].path, "select1.test");
        assert!(parse_manifest("abc  select1.test").is_err());
        assert!(parse_manifest("# only comments\n").is_err());
    }
}
