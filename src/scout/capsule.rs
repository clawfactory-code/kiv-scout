use anyhow::{Result, bail};
use rusqlite::{Connection, params};
use serde::Serialize;

use super::impact::{ImpactResult, ImpactRole};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CapsuleMode {
    Full,
    Compact,
    FilesOnly,
}

impl CapsuleMode {
    pub(crate) fn parse(value: &str) -> Self {
        match value {
            "compact" => Self::Compact,
            "files-only" | "files_only" | "files" => Self::FilesOnly,
            _ => Self::Full,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Compact => "compact",
            Self::FilesOnly => "files-only",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CapsuleCap {
    pub(crate) mode: CapsuleMode,
    pub(crate) max_tokens: usize,
    pub(crate) max_files: usize,
}

impl CapsuleCap {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "default" | "classic" => Ok(Self {
                mode: CapsuleMode::Full,
                max_tokens: 8000,
                max_files: 8,
            }),
            "files" | "files-only" | "paths" => Ok(Self {
                mode: CapsuleMode::FilesOnly,
                max_tokens: 1200,
                max_files: 20,
            }),
            "tight" | "small" => Ok(Self {
                mode: CapsuleMode::Compact,
                max_tokens: 2400,
                max_files: 8,
            }),
            "balanced" | "agent" => Ok(Self {
                mode: CapsuleMode::Compact,
                max_tokens: 8000,
                max_files: 24,
            }),
            "full" => Ok(Self {
                mode: CapsuleMode::Full,
                max_tokens: 12000,
                max_files: 12,
            }),
            "wide" => Ok(Self {
                mode: CapsuleMode::FilesOnly,
                max_tokens: 3000,
                max_files: 40,
            }),
            "deep" => Ok(Self {
                mode: CapsuleMode::Full,
                max_tokens: 32000,
                max_files: 24,
            }),
            _ => bail!("unknown cap preset '{value}'"),
        }
    }
}

pub(crate) fn render_skeleton(path: &str, lang: &str, text: &str, detail: &str) -> String {
    let symbols = crate::extract_symbols(lang, text);
    let imports = crate::extract_imports(lang, text);
    let mut out = String::new();
    out.push_str(&format!("# Skeleton: {path}\n\n"));
    if !imports.is_empty() {
        out.push_str("## Imports\n\n");
        for import in imports {
            out.push_str(&format!("- `{import}`\n"));
        }
        out.push('\n');
    }
    out.push_str("## Symbols\n\n");
    if symbols.is_empty() {
        out.push_str("*No symbols detected.*\n");
        return out;
    }
    for symbol in symbols {
        out.push_str(&format!(
            "- `{}` {} at line {}\n",
            symbol.kind, symbol.name, symbol.line
        ));
        if detail != "minimal" {
            out.push_str(&format!("  `{}`\n", symbol.signature));
        }
    }
    out
}

pub(crate) fn capsule(
    conn: &Connection,
    query: &str,
    max_tokens: usize,
    include_tests: bool,
    mode: CapsuleMode,
    max_files: usize,
) -> Result<String> {
    let rows = ranked_files(conn, query, include_tests)?;
    let mut out = String::new();
    out.push_str("# Kiv Scout Context Capsule\n\n");
    out.push_str(&format!("**Query:** {query}\n\n"));
    out.push_str(&format!("**Mode:** {}\n\n", mode.label()));
    out.push_str("## Pivot Files\n\n");
    if rows.is_empty() {
        out.push_str("*No indexed files matched. Try `kiv-scout index` or a broader query.*\n");
        return Ok(out);
    }
    let mut used = estimate_tokens(&out);
    let limit = max_files.max(1);
    for row in rows.into_iter().take(limit) {
        let section = row.render(query, mode);
        let section_tokens = estimate_tokens(&section);
        if used + section_tokens > max_tokens && used > 0 {
            break;
        }
        used += section_tokens;
        out.push_str(&section);
    }
    out.push_str(&format!("\n> Estimated tokens: {used}/{max_tokens}\n"));
    Ok(out)
}

pub(crate) fn graph_capsule(
    conn: &Connection,
    query: &str,
    result: &ImpactResult,
    max_tokens: usize,
    mode: CapsuleMode,
) -> Result<String> {
    let mut out = String::from("# Kiv Scout Context Capsule\n\n");
    out.push_str(&format!("**Query:** {query}\n\n"));
    out.push_str(&format!("**Mode:** {} + exact graph\n\n", mode.label()));
    let mut used = estimate_tokens(&out);
    let mut current_group = None;
    let mut rendered = 0usize;
    for file in &result.files {
        let group = if file.roles.contains(&ImpactRole::Pivot) {
            "Pivot Files"
        } else if file.roles.contains(&ImpactRole::Dependency) {
            "Dependencies"
        } else if file.roles.contains(&ImpactRole::Dependent) {
            "Blast Radius"
        } else {
            "Likely Tests"
        };
        let heading = (current_group != Some(group)).then(|| format!("## {group}\n\n"));
        let roles = file
            .roles
            .iter()
            .map(|role| format!("{role:?}").to_lowercase())
            .collect::<Vec<_>>()
            .join(", ");
        let row = conn.query_row(
            "SELECT lang, content FROM files WHERE path = ?1",
            params![file.path],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        );
        let section = match row {
            Ok((lang, content)) => {
                if mode == CapsuleMode::FilesOnly {
                    format!(
                        "- {} (roles: {}; depth {}{})\n",
                        file.path,
                        roles,
                        file.depth,
                        file.score
                            .map(|score| format!("; score {score:.2}"))
                            .unwrap_or_default()
                    )
                } else {
                    let ranked = RankedFile {
                        path: file.path.clone(),
                        lang,
                        content,
                        score: file.score.unwrap_or(0.0),
                    };
                    format!(
                        "**Roles:** {roles}; depth {}\n\n{}",
                        file.depth,
                        ranked.render(query, mode)
                    )
                }
            }
            Err(_) => format!(
                "- {} (roles: {}; depth {}; unindexed)\n",
                file.path, roles, file.depth
            ),
        };
        let addition = format!("{}{}", heading.as_deref().unwrap_or(""), section);
        let tokens = estimate_tokens(&addition);
        if used + tokens > max_tokens {
            break;
        }
        used += tokens;
        if let Some(heading) = heading {
            out.push_str(&heading);
            current_group = Some(group);
        }
        out.push_str(&section);
        rendered += 1;
    }
    if rendered < result.files.len() || result.truncated {
        out.push_str(&format!(
            "\n> Related output truncated; {} files rendered and {} graph files omitted.\n",
            rendered, result.omitted_files
        ));
    }
    out.push_str(&format!("\n> Estimated tokens: {used}/{max_tokens}\n"));
    while estimate_tokens(&out) > max_tokens {
        out.pop();
    }
    Ok(out)
}

struct RankedFile {
    path: String,
    lang: String,
    content: String,
    score: f64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RankedPath {
    pub(crate) path: String,
    pub(crate) score: f64,
}

pub(crate) fn ranked_paths(
    conn: &Connection,
    query: &str,
    include_tests: bool,
) -> Result<Vec<RankedPath>> {
    Ok(ranked_files(conn, query, include_tests)?
        .into_iter()
        .map(|file| RankedPath {
            path: file.path,
            score: file.score,
        })
        .collect())
}

impl RankedFile {
    fn render(&self, query: &str, mode: CapsuleMode) -> String {
        match mode {
            CapsuleMode::Full => {
                let skeleton = render_skeleton(&self.path, &self.lang, &self.content, "standard");
                let excerpt = best_excerpt(&self.content, query, 12);
                format!(
                    "### {} (score: {:.2})\n\n{}\n{}\n",
                    self.path, self.score, skeleton, excerpt
                )
            }
            CapsuleMode::Compact => {
                let skeleton = render_compact_skeleton(&self.lang, &self.content);
                let excerpt = best_excerpt(&self.content, query, 2);
                format!(
                    "### {} (score: {:.2})\n\n{}\n{}\n",
                    self.path, self.score, skeleton, excerpt
                )
            }
            CapsuleMode::FilesOnly => format!("- {} (score: {:.2})\n", self.path, self.score),
        }
    }
}

fn render_compact_skeleton(lang: &str, text: &str) -> String {
    let symbols = crate::extract_symbols(lang, text);
    let imports = crate::extract_imports(lang, text);
    let mut out = String::new();
    if !imports.is_empty() {
        out.push_str("Imports: ");
        push_limited_list(&mut out, imports.iter().map(String::as_str), 8);
        out.push('\n');
    }
    if symbols.is_empty() {
        out.push_str("Symbols: none\n");
        return out;
    }
    out.push_str("Symbols: ");
    let compact_symbols: Vec<String> = symbols
        .iter()
        .map(|symbol| format!("{} {}", symbol.kind, symbol.name))
        .collect();
    push_limited_list(&mut out, compact_symbols.iter().map(String::as_str), 24);
    out.push('\n');
    out
}

fn push_limited_list<'a>(out: &mut String, items: impl IntoIterator<Item = &'a str>, limit: usize) {
    for (count, item) in items.into_iter().enumerate() {
        if count == limit {
            if count > 0 {
                out.push_str(", ");
            }
            out.push_str("...");
            return;
        }
        if count > 0 {
            out.push_str(", ");
        }
        out.push_str(item);
    }
}

fn ranked_files(conn: &Connection, query: &str, include_tests: bool) -> Result<Vec<RankedFile>> {
    ranked_files_fts(conn, query, include_tests)
        .or_else(|_| ranked_files_scan(conn, query, include_tests))
}

fn ranked_files_fts(
    conn: &Connection,
    query: &str,
    include_tests: bool,
) -> Result<Vec<RankedFile>> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }

    let fts_query = terms.join(" OR ");
    let mut stmt = conn.prepare(
        "
        SELECT files.path, files.lang, files.content, -bm25(file_fts) AS score
        FROM file_fts
        JOIN files ON files.path = file_fts.path
        WHERE file_fts MATCH ?1
        ORDER BY bm25(file_fts)
        LIMIT 80
        ",
    )?;
    let rows = stmt.query_map(params![fts_query], |row| {
        Ok(RankedFile {
            path: row.get(0)?,
            lang: row.get(1)?,
            content: row.get(2)?,
            score: row.get::<_, f64>(3)?,
        })
    })?;

    let mut ranked = Vec::new();
    for row in rows {
        let mut file = row?;
        if !include_tests && is_test_path(&file.path) {
            continue;
        }
        let path_lower = file.path.to_lowercase();
        for term in &terms {
            if path_lower.contains(term) {
                file.score += 4.0;
            }
        }
        ranked.push(file);
    }
    ranked.sort_by(compare_ranked_files);
    Ok(ranked)
}

fn ranked_files_scan(
    conn: &Connection,
    query: &str,
    include_tests: bool,
) -> Result<Vec<RankedFile>> {
    let terms = query_terms(query);
    let mut stmt = conn.prepare("SELECT path, lang, content FROM files")?;
    let files = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    let mut ranked = Vec::new();
    for file in files {
        let (path, lang, content) = file?;
        if !include_tests && is_test_path(&path) {
            continue;
        }
        let symbols = symbols_for(conn, &path)?;
        let imports = imports_for(conn, &path)?;
        let haystack = format!(
            "{path}\n{content}\n{}\n{}",
            symbols.join("\n"),
            imports.join("\n")
        );
        let mut score = 0.0;
        let lower = haystack.to_lowercase();
        for term in &terms {
            let count = lower.matches(term).count() as f64;
            if count > 0.0 {
                score += count;
            }
            if path.to_lowercase().contains(term) {
                score += 4.0;
            }
            if symbols.iter().any(|s| s.to_lowercase().contains(term)) {
                score += 6.0;
            }
        }
        if score > 0.0 {
            ranked.push(RankedFile {
                path,
                lang,
                content,
                score,
            });
        }
    }
    ranked.sort_by(compare_ranked_files);
    Ok(ranked)
}

fn compare_ranked_files(a: &RankedFile, b: &RankedFile) -> std::cmp::Ordering {
    b.score
        .partial_cmp(&a.score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.path.cmp(&b.path))
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|s| s.to_lowercase())
        .filter(|s| s.len() > 1)
        .collect()
}

fn symbols_for(conn: &Connection, path: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT kind || ' ' || name || ' ' || signature FROM symbols WHERE file_path = ?1",
    )?;
    let rows = stmt.query_map(params![path], |row| row.get::<_, String>(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn imports_for(conn: &Connection, path: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT target FROM imports WHERE file_path = ?1")?;
    let rows = stmt.query_map(params![path], |row| row.get::<_, String>(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub(crate) fn is_test_path(path: &str) -> bool {
    path.contains("/test")
        || path.contains("/tests")
        || path.ends_with("_test.rs")
        || path.ends_with(".test.ts")
        || path.ends_with(".spec.ts")
}

fn best_excerpt(content: &str, query: &str, radius: usize) -> String {
    let terms = query_terms(query);
    let lines: Vec<&str> = content.lines().collect();
    let mut best_idx = 0usize;
    let mut best_score = 0usize;
    for (idx, line) in lines.iter().enumerate() {
        let lower = line.to_lowercase();
        let score = terms.iter().filter(|term| lower.contains(*term)).count();
        if score > best_score {
            best_idx = idx;
            best_score = score;
        }
    }
    if best_score == 0 {
        return String::new();
    }
    let start = best_idx.saturating_sub(radius / 2);
    let end = (best_idx + radius / 2 + 1).min(lines.len());
    let mut out = String::from("```text\n");
    for (idx, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("{:>4}: {}\n", start + idx + 1, line));
    }
    out.push_str("```\n");
    out
}

pub(crate) fn estimate_tokens(text: &str) -> usize {
    (text.len() / 4).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indexed_matches(count: usize) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE files (
                path TEXT PRIMARY KEY,
                lang TEXT NOT NULL,
                hash TEXT NOT NULL,
                content TEXT NOT NULL
            );
            CREATE TABLE symbols (
                id INTEGER PRIMARY KEY,
                file_path TEXT NOT NULL,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                line INTEGER NOT NULL,
                signature TEXT NOT NULL
            );
            CREATE TABLE imports (
                id INTEGER PRIMARY KEY,
                file_path TEXT NOT NULL,
                target TEXT NOT NULL
            );
            CREATE VIRTUAL TABLE file_fts USING fts5(path, content, symbols);
            ",
        )
        .unwrap();

        for index in 0..count {
            let path = format!("src/file_{index:02}.rs");
            let content = format!("pub fn needle_{index}() {{}}");
            conn.execute(
                "INSERT INTO files(path, lang, hash, content) VALUES (?1, 'rust', ?2, ?3)",
                params![path, index.to_string(), content],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO file_fts(path, content, symbols) VALUES (?1, ?2, ?3)",
                params![path, content, format!("function needle_{index}")],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn ranking_and_capsule_honor_file_limits_above_eight() {
        let conn = indexed_matches(12);

        assert_eq!(ranked_files_fts(&conn, "needle", true).unwrap().len(), 12);
        assert_eq!(ranked_files_scan(&conn, "needle", true).unwrap().len(), 12);

        let output = capsule(&conn, "needle", 10_000, true, CapsuleMode::FilesOnly, 12).unwrap();
        let pivot_count = output
            .lines()
            .filter(|line| line.starts_with("- src/file_"))
            .count();
        assert_eq!(pivot_count, 12);
    }
}
