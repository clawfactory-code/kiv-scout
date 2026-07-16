use anyhow::Result;
use rusqlite::{Connection, Transaction, params};
use serde::Serialize;
#[cfg(test)]
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ImportKind {
    Import,
    Require,
    Use,
    Module,
}

impl ImportKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Require => "require",
            Self::Use => "use",
            Self::Module => "module",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "require" => Self::Require,
            "use" => Self::Use,
            "module" => Self::Module,
            _ => Self::Import,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ImportRef {
    pub(crate) raw_target: String,
    pub(crate) kind: ImportKind,
    pub(crate) line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct DependencyEdge {
    pub(crate) source_path: String,
    pub(crate) raw_target: String,
    pub(crate) target_path: Option<String>,
    pub(crate) kind: String,
    pub(crate) line: usize,
    pub(crate) resolution: String,
    pub(crate) resolver: String,
    pub(crate) candidate_paths: Vec<String>,
}

pub(crate) fn recompute_dependency_edges(tx: &Transaction<'_>) -> Result<()> {
    let files = load_files(tx)?;
    let paths = files.keys().cloned().collect::<BTreeSet<_>>();
    let mut stmt = tx.prepare(
        "SELECT i.file_path, f.lang, i.target, i.kind, i.line
         FROM imports i JOIN files f ON f.path = i.file_path
         ORDER BY i.file_path, i.line, i.kind, i.target",
    )?;
    let observations = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                ImportRef {
                    raw_target: row.get(2)?,
                    kind: ImportKind::parse(&row.get::<_, String>(3)?),
                    line: row.get::<_, i64>(4)? as usize,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    tx.execute("DELETE FROM dependency_edges", [])?;
    for (source, lang, import) in observations {
        let edge = resolve_import(&source, &lang, &import, &paths);
        tx.execute(
            "INSERT INTO dependency_edges(
                source_path, raw_target, target_path, kind, line, resolution, resolver,
                candidate_paths_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                edge.source_path,
                edge.raw_target,
                edge.target_path,
                edge.kind,
                edge.line as i64,
                edge.resolution,
                edge.resolver,
                serde_json::to_string(&edge.candidate_paths)?,
            ],
        )?;
    }
    Ok(())
}

fn load_files(tx: &Transaction<'_>) -> Result<BTreeMap<String, String>> {
    let mut stmt = tx.prepare("SELECT path, lang FROM files ORDER BY path")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    let mut files = BTreeMap::new();
    for row in rows {
        let (path, lang) = row?;
        files.insert(path, lang);
    }
    Ok(files)
}

fn resolve_import(
    source_path: &str,
    lang: &str,
    import: &ImportRef,
    paths: &BTreeSet<String>,
) -> DependencyEdge {
    let (resolver, candidates) = match lang {
        "typescript" | "javascript" => (
            "typescript-relative-v1",
            resolve_javascript(source_path, &import.raw_target, paths),
        ),
        "python" => (
            "python-module-v1",
            resolve_python(source_path, &import.raw_target, paths),
        ),
        "rust" => ("rust-local-v1", resolve_rust(source_path, import, paths)),
        _ => ("unsupported-v1", Vec::new()),
    };
    let (resolution, target_path, candidate_paths) = match candidates.as_slice() {
        [only] => ("exact", Some(only.clone()), Vec::new()),
        [] => ("unresolved", None, Vec::new()),
        _ => ("ambiguous", None, candidates),
    };
    DependencyEdge {
        source_path: source_path.to_string(),
        raw_target: import.raw_target.clone(),
        target_path,
        kind: import.kind.as_str().to_string(),
        line: import.line,
        resolution: resolution.to_string(),
        resolver: resolver.to_string(),
        candidate_paths,
    }
}

fn resolve_javascript(source: &str, raw: &str, paths: &BTreeSet<String>) -> Vec<String> {
    if !raw.starts_with("./") && !raw.starts_with("../") {
        return Vec::new();
    }
    let base = Path::new(source).parent().unwrap_or_else(|| Path::new(""));
    let Some(stem) = normalize_repo_path(&base.join(raw)) else {
        return Vec::new();
    };
    let extensions = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];
    let mut proposed = vec![stem.clone()];
    if Path::new(&stem).extension().is_none() {
        proposed.extend(extensions.iter().map(|ext| format!("{stem}.{ext}")));
        proposed.extend(extensions.iter().map(|ext| format!("{stem}/index.{ext}")));
    }
    existing_candidates(proposed, paths)
}

fn resolve_python(source: &str, raw: &str, paths: &BTreeSet<String>) -> Vec<String> {
    let dots = raw.chars().take_while(|ch| *ch == '.').count();
    let module = raw[dots..].trim_matches('.').replace('.', "/");
    if dots == 0 {
        if module.is_empty() {
            return Vec::new();
        }
        let file_suffix = format!("{module}.py");
        let package_suffix = format!("{module}/__init__.py");
        return paths
            .iter()
            .filter(|path| {
                path.as_str() == file_suffix
                    || path.ends_with(&format!("/{file_suffix}"))
                    || path.as_str() == package_suffix
                    || path.ends_with(&format!("/{package_suffix}"))
            })
            .cloned()
            .collect();
    }
    let mut base = if dots == 0 {
        PathBuf::new()
    } else {
        Path::new(source)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf()
    };
    // One leading dot means the current package; each additional dot ascends once.
    for _ in 1..dots {
        if !base.pop() {
            return Vec::new();
        }
    }
    if module.is_empty() {
        return Vec::new();
    }
    let Some(stem) = normalize_repo_path(&base.join(module)) else {
        return Vec::new();
    };
    existing_candidates(
        vec![format!("{stem}.py"), format!("{stem}/__init__.py")],
        paths,
    )
}

fn resolve_rust(source: &str, import: &ImportRef, paths: &BTreeSet<String>) -> Vec<String> {
    let raw = import.raw_target.trim();
    if raw.is_empty() || raw.starts_with("std::") || raw.starts_with("core::") {
        return Vec::new();
    }
    let source_path = Path::new(source);
    let source_dir = source_path.parent().unwrap_or_else(|| Path::new(""));
    if import.kind == ImportKind::Module {
        let stem = source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let base = if matches!(stem, "lib" | "main" | "mod") {
            source_dir.to_path_buf()
        } else {
            source_dir.join(stem)
        };
        return rust_module_candidates(&base, raw, paths);
    }

    let mut segments = raw
        .trim_start_matches("::")
        .split("::")
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return Vec::new();
    }
    let crate_src = rust_crate_source_dir(source_path);
    let mut fallback = Vec::new();
    let base = match segments[0] {
        "crate" => {
            segments.remove(0);
            fallback = rust_module_identity_candidates(&crate_src, paths);
            crate_src
        }
        "self" => {
            segments.remove(0);
            if paths.contains(source) {
                fallback.push(source.to_string());
            }
            rust_current_module_dir(source_path)
        }
        "super" => {
            let mut base = rust_current_module_dir(source_path);
            while segments.first() == Some(&"super") {
                segments.remove(0);
                base.pop();
            }
            fallback = rust_module_identity_candidates(&base, paths);
            base
        }
        _ => rust_current_module_dir(source_path),
    };
    if segments.is_empty() {
        return fallback;
    }

    // A use path may end in an item. Prefer the longest path that names an indexed module,
    // then fall back toward the first segment without guessing between multiple modules.
    for len in (1..=segments.len()).rev() {
        let module = segments[..len].join("/");
        let candidates = rust_module_candidates(&base, &module, paths);
        if !candidates.is_empty() {
            return candidates;
        }
    }
    if segments == ["*"] {
        Vec::new()
    } else {
        fallback
    }
}

fn rust_crate_source_dir(source: &Path) -> PathBuf {
    let components = source.components().collect::<Vec<_>>();
    let src_index = components
        .iter()
        .rposition(|part| part.as_os_str() == "src");
    match src_index {
        Some(index) => components[..=index].iter().collect(),
        None => PathBuf::new(),
    }
}

fn rust_current_module_dir(source: &Path) -> PathBuf {
    let dir = source.parent().unwrap_or_else(|| Path::new(""));
    let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if matches!(stem, "lib" | "main" | "mod") {
        dir.to_path_buf()
    } else {
        dir.join(stem)
    }
}

fn rust_module_candidates(base: &Path, module: &str, paths: &BTreeSet<String>) -> Vec<String> {
    let Some(stem) = normalize_repo_path(&base.join(module)) else {
        return Vec::new();
    };
    existing_candidates(vec![format!("{stem}.rs"), format!("{stem}/mod.rs")], paths)
}

fn rust_module_identity_candidates(base: &Path, paths: &BTreeSet<String>) -> Vec<String> {
    let Some(stem) = normalize_repo_path(base) else {
        return Vec::new();
    };
    let mut proposed = vec![format!("{stem}.rs"), format!("{stem}/mod.rs")];
    if base.file_name().and_then(|name| name.to_str()) == Some("src") {
        proposed.extend([format!("{stem}/lib.rs"), format!("{stem}/main.rs")]);
    }
    existing_candidates(proposed, paths)
}

fn normalize_repo_path(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => parts.push(value.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

fn existing_candidates(proposed: Vec<String>, paths: &BTreeSet<String>) -> Vec<String> {
    proposed
        .into_iter()
        .filter(|path| paths.contains(path))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[allow(dead_code)] // Stable exact-only internal query API; impact uses evidence-rich rows.
pub(crate) fn forward_dependencies(conn: &Connection, source_path: &str) -> Result<Vec<String>> {
    exact_paths(
        conn,
        "SELECT DISTINCT target_path FROM dependency_edges
         WHERE source_path = ?1 AND resolution = 'exact'
         ORDER BY target_path",
        source_path,
    )
}

#[allow(dead_code)] // Stable exact-only internal query API; impact uses evidence-rich rows.
pub(crate) fn reverse_dependencies(conn: &Connection, target_path: &str) -> Result<Vec<String>> {
    exact_paths(
        conn,
        "SELECT DISTINCT source_path FROM dependency_edges
         WHERE target_path = ?1 AND resolution = 'exact'
         ORDER BY source_path",
        target_path,
    )
}

#[allow(dead_code)]
fn exact_paths(conn: &Connection, sql: &str, path: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![path], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Return graph meaning in a stable order for evaluation and snapshots.
#[cfg(test)]
pub(crate) fn semantic_dependency_edges(conn: &Connection) -> Result<Vec<DependencyEdge>> {
    let mut stmt = conn.prepare(
        "SELECT source_path, raw_target, target_path, kind, line, resolution, resolver,
                candidate_paths_json
         FROM dependency_edges
         ORDER BY source_path, raw_target, target_path, kind, line, resolution, resolver,
                  candidate_paths_json",
    )?;
    let rows = stmt.query_map([], |row| {
        let candidates_json = row.get::<_, String>(7)?;
        let candidate_paths = serde_json::from_str(&candidates_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                candidates_json.len(),
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        Ok(DependencyEdge {
            source_path: row.get(0)?,
            raw_target: row.get(1)?,
            target_path: row.get(2)?,
            kind: row.get(3)?,
            line: row.get::<_, i64>(4)? as usize,
            resolution: row.get(5)?,
            resolver: row.get(6)?,
            candidate_paths,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
pub(crate) fn semantic_edge_digest(conn: &Connection) -> Result<String> {
    let bytes = serde_json::to_vec(&semantic_dependency_edges(conn)?)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn import(raw: &str, kind: ImportKind) -> ImportRef {
        ImportRef {
            raw_target: raw.to_string(),
            kind,
            line: 1,
        }
    }

    #[test]
    fn javascript_resolution_is_exact_ambiguous_or_unresolved() {
        let indexed = paths(&[
            "src/app.ts",
            "src/thing.ts",
            "src/thing/index.ts",
            "node.ts",
        ]);
        let ambiguous = resolve_import(
            "src/app.ts",
            "typescript",
            &import("./thing", ImportKind::Import),
            &indexed,
        );
        assert_eq!(ambiguous.resolution, "ambiguous");
        assert_eq!(
            ambiguous.candidate_paths,
            vec!["src/thing.ts", "src/thing/index.ts"]
        );
        assert!(ambiguous.target_path.is_none());

        let unresolved = resolve_import(
            "src/app.ts",
            "typescript",
            &import("react", ImportKind::Import),
            &indexed,
        );
        assert_eq!(unresolved.resolution, "unresolved");
    }

    #[test]
    fn python_and_rust_resolve_only_local_indexed_modules() {
        let indexed = paths(&[
            "pkg/sub/use.py",
            "pkg/sub/local.py",
            "pkg/absolute.py",
            "src/lib.rs",
            "src/graph.rs",
        ]);
        assert_eq!(
            resolve_import(
                "pkg/sub/use.py",
                "python",
                &import(".local", ImportKind::Import),
                &indexed,
            )
            .target_path
            .as_deref(),
            Some("pkg/sub/local.py")
        );
        assert_eq!(
            resolve_import(
                "pkg/sub/use.py",
                "python",
                &import("pkg.absolute", ImportKind::Import),
                &indexed,
            )
            .target_path
            .as_deref(),
            Some("pkg/absolute.py")
        );
        assert_eq!(
            resolve_import(
                "src/lib.rs",
                "rust",
                &import("graph", ImportKind::Module),
                &indexed,
            )
            .target_path
            .as_deref(),
            Some("src/graph.rs")
        );
    }

    #[test]
    fn python_absolute_modules_match_one_root_and_expose_multi_root_ambiguity() {
        let one_root = paths(&["app/use.py", "src/pkg/module.py"]);
        let exact = resolve_import(
            "app/use.py",
            "python",
            &import("pkg.module", ImportKind::Import),
            &one_root,
        );
        assert_eq!(exact.target_path.as_deref(), Some("src/pkg/module.py"));

        let multiple_roots = paths(&["app/use.py", "lib/pkg/module.py", "src/pkg/module.py"]);
        let ambiguous = resolve_import(
            "app/use.py",
            "python",
            &import("pkg.module", ImportKind::Import),
            &multiple_roots,
        );
        assert_eq!(ambiguous.resolution, "ambiguous");
        assert_eq!(
            ambiguous.candidate_paths,
            vec!["lib/pkg/module.py", "src/pkg/module.py"]
        );
        assert!(ambiguous.target_path.is_none());
    }

    #[test]
    fn rust_prefixes_fall_back_to_their_owning_module() {
        let indexed = paths(&["src/main.rs", "src/scout/mod.rs", "src/scout/capsule.rs"]);
        assert_eq!(
            resolve_import(
                "src/scout/capsule.rs",
                "rust",
                &import("crate::SharedType", ImportKind::Use),
                &indexed,
            )
            .target_path
            .as_deref(),
            Some("src/main.rs")
        );
        let wildcard = resolve_import(
            "src/scout/capsule.rs",
            "rust",
            &import("super::*", ImportKind::Use),
            &indexed,
        );
        assert_eq!(wildcard.resolution, "unresolved");
        assert!(wildcard.target_path.is_none());
    }
}
