//! Read-only git diff collection and changed-line to indexed-symbol mapping.
//!
//! This module intentionally does not know about the impact graph or CLI. It
//! produces explicit file pivots which those layers can consume.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::Path;
use std::process::Command;

const MAX_ERROR_CHARS: usize = 320;
const MAX_DIFF_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

impl LineRange {
    pub fn new(start: usize, end: usize) -> Option<Self> {
        (start > 0 && end >= start).then_some(Self { start, end })
    }

    fn overlaps(self, other: Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangedHunk {
    /// Lines in the reference-side file. `None` means the hunk adds lines only.
    pub old_lines: Option<LineRange>,
    /// Lines in the working-tree-side file. `None` means the hunk deletes lines only.
    pub new_lines: Option<LineRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangedFile {
    pub old_path: Option<String>,
    pub path: Option<String>,
    pub kind: ChangeKind,
    pub binary: bool,
    pub hunks: Vec<ChangedHunk>,
}

impl ChangedFile {
    /// The path to use as an impact pivot. Deleted files fall back to the old path.
    pub fn pivot_path(&self) -> Option<&str> {
        self.path.as_deref().or(self.old_path.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitDiff {
    pub reference: String,
    pub files: Vec<ChangedFile>,
}

#[derive(Debug)]
pub enum DiffError {
    GitNotFound,
    RepositoryHasNoWorktree,
    InvalidReference(String),
    DiffTooLarge { bytes: usize, limit: usize },
    Git(io::Error),
    Parse(String),
}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitNotFound => write!(f, "git executable not found"),
            Self::RepositoryHasNoWorktree => write!(f, "repository has no git worktree"),
            Self::InvalidReference(message) => write!(f, "git diff failed: {message}"),
            Self::DiffTooLarge { bytes, limit } => {
                write!(
                    f,
                    "git diff is too large to analyze ({bytes} bytes; limit {limit})"
                )
            }
            Self::Git(error) => write!(f, "could not run git diff: {error}"),
            Self::Parse(message) => write!(f, "could not parse git diff: {message}"),
        }
    }
}

impl std::error::Error for DiffError {}

/// Run a read-only comparison between `reference` and the repository worktree.
///
/// The argument is rejected when it could be interpreted as a git option. The
/// command performs no fetch, checkout, index update, or other repository write.
pub fn read_git_diff(repo_root: &Path, reference: &str) -> Result<GitDiff, DiffError> {
    if reference.is_empty() || reference.starts_with('-') || reference.contains('\0') {
        return Err(DiffError::InvalidReference(
            "reference must be a non-empty git revision, not an option".to_string(),
        ));
    }

    let output = Command::new("git")
        .args(["diff", "--unified=0", "--no-ext-diff", reference, "--"])
        .current_dir(repo_root)
        .output()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                DiffError::GitNotFound
            } else {
                DiffError::Git(error)
            }
        })?;

    if !output.status.success() {
        let message = first_actionable_line(&String::from_utf8_lossy(&output.stderr));
        let lower_message = message.to_ascii_lowercase();
        if lower_message.contains("not a git repository")
            || lower_message.contains("not a git work tree")
        {
            return Err(DiffError::RepositoryHasNoWorktree);
        }
        return Err(DiffError::InvalidReference(message));
    }
    if output.stdout.len() > MAX_DIFF_BYTES {
        return Err(DiffError::DiffTooLarge {
            bytes: output.stdout.len(),
            limit: MAX_DIFF_BYTES,
        });
    }

    parse_git_diff(reference, &String::from_utf8_lossy(&output.stdout))
}

/// Parse the stable headers emitted by `git diff --unified=0`.
pub fn parse_git_diff(reference: &str, text: &str) -> Result<GitDiff, DiffError> {
    let mut files = Vec::new();
    let mut current: Option<ChangedFile> = None;

    for line in text.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            if let Some(file) = current.take() {
                files.push(finalize_file(file));
            }
            let (old_path, path) = parse_diff_paths(header);
            current = Some(ChangedFile {
                old_path,
                path,
                kind: ChangeKind::Modified,
                binary: false,
                hunks: Vec::new(),
            });
            continue;
        }

        let Some(file) = current.as_mut() else {
            continue;
        };
        if line.starts_with("new file mode ") {
            file.kind = ChangeKind::Added;
        } else if line.starts_with("deleted file mode ") {
            file.kind = ChangeKind::Deleted;
        } else if let Some(path) = line.strip_prefix("rename from ") {
            file.old_path = Some(decode_git_path(path));
            file.kind = ChangeKind::Renamed;
        } else if let Some(path) = line.strip_prefix("rename to ") {
            file.path = Some(decode_git_path(path));
            file.kind = ChangeKind::Renamed;
        } else if let Some(path) = line.strip_prefix("--- ") {
            file.old_path = parse_patch_path(path, "a/");
        } else if let Some(path) = line.strip_prefix("+++ ") {
            file.path = parse_patch_path(path, "b/");
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            file.binary = true;
        } else if line.starts_with("@@ ") {
            file.hunks.push(parse_hunk_header(line)?);
        }
    }
    if let Some(file) = current {
        files.push(finalize_file(file));
    }

    Ok(GitDiff {
        reference: reference.to_string(),
        files,
    })
}

fn finalize_file(mut file: ChangedFile) -> ChangedFile {
    if file.kind == ChangeKind::Added {
        file.old_path = None;
    } else if file.kind == ChangeKind::Deleted {
        file.path = None;
    }
    file
}

fn parse_patch_path(value: &str, prefix: &str) -> Option<String> {
    if value == "/dev/null" {
        return None;
    }
    let decoded = decode_git_path(value);
    Some(decoded.strip_prefix(prefix).unwrap_or(&decoded).to_string())
}

fn parse_diff_paths(value: &str) -> (Option<String>, Option<String>) {
    if value.starts_with('"') {
        if let Some(separator) = value.find("\" \"") {
            let old = decode_git_path(&value[..separator + 1]);
            let new = decode_git_path(&value[separator + 2..]);
            return (
                Some(old.strip_prefix("a/").unwrap_or(&old).to_string()),
                Some(new.strip_prefix("b/").unwrap_or(&new).to_string()),
            );
        }
    } else if let Some((old, new)) = value.split_once(" b/") {
        return (
            Some(old.strip_prefix("a/").unwrap_or(old).to_string()),
            Some(new.to_string()),
        );
    }
    (None, None)
}

fn decode_git_path(value: &str) -> String {
    let value = value.trim();
    if !(value.starts_with('"') && value.ends_with('"')) {
        return value.to_string();
    }

    // Git uses C-style quoting for unusual paths. Decode its common byte
    // escapes, including octal UTF-8 bytes, without trusting patch contents.
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 1;
    while index + 1 < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        if index + 1 >= bytes.len() {
            break;
        }
        match bytes[index] {
            b'n' => decoded.push(b'\n'),
            b't' => decoded.push(b'\t'),
            b'r' => decoded.push(b'\r'),
            b'\\' => decoded.push(b'\\'),
            b'"' => decoded.push(b'"'),
            b'0'..=b'7' => {
                let mut number = 0_u16;
                let mut count = 0;
                while index < bytes.len() - 1 && count < 3 && matches!(bytes[index], b'0'..=b'7') {
                    number = number * 8 + u16::from(bytes[index] - b'0');
                    index += 1;
                    count += 1;
                }
                decoded.push(number as u8);
                continue;
            }
            escaped => decoded.push(escaped),
        }
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn parse_hunk_header(line: &str) -> Result<ChangedHunk, DiffError> {
    let body = line
        .strip_prefix("@@ -")
        .and_then(|value| value.split_once(" @@").map(|(header, _)| header))
        .ok_or_else(|| DiffError::Parse(truncate(line, MAX_ERROR_CHARS)))?;
    let (old, new) = body
        .split_once(" +")
        .ok_or_else(|| DiffError::Parse(truncate(line, MAX_ERROR_CHARS)))?;
    Ok(ChangedHunk {
        old_lines: parse_range(old)?,
        new_lines: parse_range(new)?,
    })
}

fn parse_range(value: &str) -> Result<Option<LineRange>, DiffError> {
    let (start, count) = match value.split_once(',') {
        Some((start, count)) => (parse_usize(start)?, parse_usize(count)?),
        None => (parse_usize(value)?, 1),
    };
    if count == 0 {
        Ok(None)
    } else {
        let end = start
            .checked_add(count - 1)
            .ok_or_else(|| DiffError::Parse("hunk line range overflowed".to_string()))?;
        LineRange::new(start, end)
            .map(Some)
            .ok_or_else(|| DiffError::Parse("hunk line range started at zero".to_string()))
    }
}

fn parse_usize(value: &str) -> Result<usize, DiffError> {
    value
        .parse()
        .map_err(|_| DiffError::Parse(format!("invalid hunk line number: {}", truncate(value, 64))))
}

fn first_actionable_line(stderr: &str) -> String {
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("git returned an error without details");
    truncate(line, MAX_ERROR_CHARS)
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RangeEvidence {
    ExactRange,
    Approximate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedSymbolRange {
    pub name: String,
    pub kind: String,
    pub lines: LineRange,
    pub evidence: RangeEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangedSymbol {
    pub name: String,
    pub kind: String,
    pub lines: LineRange,
    pub evidence: RangeEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangedFileMapping {
    pub path: String,
    pub old_path: Option<String>,
    pub kind: ChangeKind,
    pub binary: bool,
    pub symbols: Vec<ChangedSymbol>,
    pub module_level: bool,
    pub unindexed: bool,
    pub deleted: bool,
}

/// Map diff hunks to the narrowest overlapping indexed symbol ranges.
///
/// `indexed` may contain the old path of a deleted/renamed file. Deletion-only
/// hunks use those old ranges; otherwise working-tree-side ranges take priority.
pub fn map_changed_files(
    diff: &GitDiff,
    indexed: &BTreeMap<String, Vec<IndexedSymbolRange>>,
) -> Vec<ChangedFileMapping> {
    diff.files
        .iter()
        .filter_map(|file| {
            let pivot = file.pivot_path()?.to_string();
            let pivot_index_path = if file.kind == ChangeKind::Deleted {
                file.old_path.as_deref()
            } else {
                file.path.as_deref().or(file.old_path.as_deref())
            };
            let pivot_is_indexed = pivot_index_path.is_some_and(|path| indexed.contains_key(path));
            let mut changed_symbols = Vec::new();
            let mut seen = BTreeSet::new();
            let mut module_level = file.hunks.is_empty();

            for hunk in &file.hunks {
                let (changed_range, range_path) = if let Some(lines) = hunk.new_lines {
                    (Some(lines), file.path.as_deref())
                } else {
                    (hunk.old_lines, file.old_path.as_deref())
                };
                let Some(changed_range) = changed_range else {
                    module_level = true;
                    continue;
                };
                let Some(symbols) = range_path.and_then(|path| indexed.get(path)) else {
                    module_level = true;
                    continue;
                };
                let overlapping: Vec<_> = symbols
                    .iter()
                    .filter(|symbol| symbol.lines.overlaps(changed_range))
                    .collect();
                let Some(narrowest) = overlapping
                    .iter()
                    .map(|symbol| symbol.lines.end - symbol.lines.start)
                    .min()
                else {
                    module_level = true;
                    continue;
                };
                for symbol in overlapping
                    .into_iter()
                    .filter(|symbol| symbol.lines.end - symbol.lines.start == narrowest)
                {
                    let key = (
                        symbol.name.clone(),
                        symbol.kind.clone(),
                        symbol.lines,
                        symbol.evidence,
                    );
                    if seen.insert(key) {
                        changed_symbols.push(ChangedSymbol {
                            name: symbol.name.clone(),
                            kind: symbol.kind.clone(),
                            lines: symbol.lines,
                            evidence: symbol.evidence,
                        });
                    }
                }
            }

            let unindexed = !pivot_is_indexed;
            if unindexed {
                module_level = true;
            }
            changed_symbols.sort_by(|a, b| {
                (a.lines.start, a.lines.end, &a.name).cmp(&(b.lines.start, b.lines.end, &b.name))
            });
            Some(ChangedFileMapping {
                path: pivot,
                old_path: file.old_path.clone(),
                kind: file.kind,
                binary: file.binary,
                symbols: changed_symbols,
                module_level,
                unindexed,
                deleted: file.kind == ChangeKind::Deleted,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_added_modified_deleted_renamed_and_binary_files() {
        let patch = r#"diff --git a/src/modified.rs b/src/modified.rs
index 1111111..2222222 100644
--- a/src/modified.rs
+++ b/src/modified.rs
@@ -4,2 +4,3 @@
diff --git a/src/added.rs b/src/added.rs
new file mode 100644
--- /dev/null
+++ b/src/added.rs
@@ -0,0 +1,2 @@
diff --git a/src/deleted.rs b/src/deleted.rs
deleted file mode 100644
--- a/src/deleted.rs
+++ /dev/null
@@ -7,2 +0,0 @@
diff --git a/src/old.rs b/src/new.rs
similarity index 95%
rename from src/old.rs
rename to src/new.rs
diff --git a/assets/pixel.dat b/assets/pixel.dat
index 1111111..2222222 100644
Binary files a/assets/pixel.dat and b/assets/pixel.dat differ
"#;
        let parsed = parse_git_diff("HEAD", patch).unwrap();

        assert_eq!(parsed.files.len(), 5);
        assert_eq!(parsed.files[0].kind, ChangeKind::Modified);
        assert_eq!(parsed.files[0].path.as_deref(), Some("src/modified.rs"));
        assert_eq!(parsed.files[0].hunks[0].new_lines, LineRange::new(4, 6));
        assert_eq!(parsed.files[1].kind, ChangeKind::Added);
        assert_eq!(parsed.files[1].old_path, None);
        assert_eq!(parsed.files[2].kind, ChangeKind::Deleted);
        assert_eq!(parsed.files[2].path, None);
        assert_eq!(parsed.files[2].hunks[0].old_lines, LineRange::new(7, 8));
        assert_eq!(parsed.files[3].kind, ChangeKind::Renamed);
        assert_eq!(parsed.files[3].old_path.as_deref(), Some("src/old.rs"));
        assert_eq!(parsed.files[3].path.as_deref(), Some("src/new.rs"));
        assert!(parsed.files[4].binary);
    }

    #[test]
    fn maps_each_hunk_to_the_narrowest_symbol_and_keeps_module_changes() {
        let diff = GitDiff {
            reference: "HEAD".into(),
            files: vec![ChangedFile {
                old_path: Some("src/auth.rs".into()),
                path: Some("src/auth.rs".into()),
                kind: ChangeKind::Modified,
                binary: false,
                hunks: vec![
                    ChangedHunk {
                        old_lines: LineRange::new(45, 45),
                        new_lines: LineRange::new(45, 45),
                    },
                    ChangedHunk {
                        old_lines: LineRange::new(3, 3),
                        new_lines: LineRange::new(3, 3),
                    },
                ],
            }],
        };
        let mut indexed = BTreeMap::new();
        indexed.insert(
            "src/auth.rs".into(),
            vec![
                symbol("Auth", "impl", 20, 80, RangeEvidence::ExactRange),
                symbol(
                    "validate_token",
                    "function",
                    42,
                    68,
                    RangeEvidence::ExactRange,
                ),
            ],
        );

        let mapped = map_changed_files(&diff, &indexed);
        assert_eq!(mapped[0].symbols.len(), 1);
        assert_eq!(mapped[0].symbols[0].name, "validate_token");
        assert!(mapped[0].module_level);
        assert!(!mapped[0].unindexed);
    }

    #[test]
    fn labels_approximate_deleted_and_unindexed_pivots() {
        let diff = GitDiff {
            reference: "HEAD".into(),
            files: vec![
                ChangedFile {
                    old_path: Some("old.py".into()),
                    path: None,
                    kind: ChangeKind::Deleted,
                    binary: false,
                    hunks: vec![ChangedHunk {
                        old_lines: LineRange::new(8, 8),
                        new_lines: None,
                    }],
                },
                ChangedFile {
                    old_path: Some("missing.rs".into()),
                    path: Some("missing.rs".into()),
                    kind: ChangeKind::Modified,
                    binary: false,
                    hunks: vec![],
                },
            ],
        };
        let mut indexed = BTreeMap::new();
        indexed.insert(
            "old.py".into(),
            vec![symbol(
                "legacy",
                "function",
                5,
                12,
                RangeEvidence::Approximate,
            )],
        );

        let mapped = map_changed_files(&diff, &indexed);
        assert_eq!(mapped[0].symbols[0].evidence, RangeEvidence::Approximate);
        assert!(mapped[0].deleted);
        assert!(!mapped[0].unindexed);
        assert!(mapped[1].unindexed);
        assert!(mapped[1].module_level);
    }

    #[test]
    fn deletion_only_rename_hunks_use_old_symbol_ranges() {
        let diff = GitDiff {
            reference: "HEAD".into(),
            files: vec![ChangedFile {
                old_path: Some("src/old.rs".into()),
                path: Some("src/new.rs".into()),
                kind: ChangeKind::Renamed,
                binary: false,
                hunks: vec![ChangedHunk {
                    old_lines: LineRange::new(12, 12),
                    new_lines: None,
                }],
            }],
        };
        let mut indexed = BTreeMap::new();
        indexed.insert(
            "src/old.rs".into(),
            vec![symbol(
                "removed_branch",
                "function",
                10,
                14,
                RangeEvidence::ExactRange,
            )],
        );

        let mapped = map_changed_files(&diff, &indexed);
        assert_eq!(mapped[0].symbols[0].name, "removed_branch");
        assert!(mapped[0].unindexed, "the new pivot is not indexed yet");
    }

    #[test]
    fn read_git_diff_includes_staged_unstaged_rename_delete_and_binary_changes() {
        let root = temporary_directory("git-diff-fixture");
        fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "kiv@example.test"]);
        git(&root, &["config", "user.name", "Kiv Test"]);
        fs::write(root.join("src/modify.rs"), "one\ntwo\n").unwrap();
        fs::write(root.join("src/delete.rs"), "delete me\n").unwrap();
        fs::write(root.join("src/rename.rs"), "rename me\n").unwrap();
        fs::write(root.join("binary.dat"), [0_u8, 1, 2]).unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "base"]);

        fs::write(root.join("src/modify.rs"), "one\ntwo changed\n").unwrap();
        fs::write(root.join("src/added.rs"), "added\n").unwrap();
        git(&root, &["add", "src/added.rs"]);
        git(&root, &["mv", "src/rename.rs", "src/renamed.rs"]);
        fs::remove_file(root.join("src/delete.rs")).unwrap();
        fs::write(root.join("binary.dat"), [0_u8, 255, 2]).unwrap();

        let before_status = git_output(&root, &["status", "--porcelain=v1"]);
        let index_before = fs::read(root.join(".git/index")).unwrap();
        let diff = read_git_diff(&root, "HEAD").unwrap();
        let index_after = fs::read(root.join(".git/index")).unwrap();
        let after_status = git_output(&root, &["status", "--porcelain=v1"]);

        let pivots: BTreeSet<_> = diff
            .files
            .iter()
            .filter_map(ChangedFile::pivot_path)
            .collect();
        assert!(pivots.contains("src/modify.rs"));
        assert!(pivots.contains("src/added.rs"));
        assert!(pivots.contains("src/delete.rs"));
        assert!(pivots.contains("src/renamed.rs"));
        assert!(pivots.contains("binary.dat"));
        assert!(diff.files.iter().any(|file| file.binary));
        assert!(diff.files.iter().any(|file| {
            file.kind == ChangeKind::Renamed && file.path.as_deref() == Some("src/renamed.rs")
        }));
        assert_eq!(index_before, index_after);
        assert_eq!(before_status, after_status);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reports_non_git_and_bad_ref_errors_concisely() {
        let root = temporary_directory("not-git");
        let non_git = read_git_diff(&root, "HEAD").unwrap_err().to_string();
        assert_eq!(non_git, "repository has no git worktree");

        git(&root, &["init", "-q"]);
        let bad_ref = read_git_diff(&root, "definitely-not-a-ref")
            .unwrap_err()
            .to_string();
        assert!(bad_ref.starts_with("git diff failed:"));
        assert!(bad_ref.chars().count() <= MAX_ERROR_CHARS + 17);

        fs::remove_dir_all(root).unwrap();
    }

    fn symbol(
        name: &str,
        kind: &str,
        start: usize,
        end: usize,
        evidence: RangeEvidence,
    ) -> IndexedSymbolRange {
        IndexedSymbolRange {
            name: name.into(),
            kind: kind.into(),
            lines: LineRange::new(start, end).unwrap(),
            evidence,
        }
    }

    fn temporary_directory(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("kiv-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_output(root: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    }
}
