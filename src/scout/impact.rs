use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::capsule::{RankedPath, estimate_tokens, is_test_path, ranked_paths};

const MAX_DEPTH: usize = 3;
const MAX_PIVOTS: usize = 3;

#[derive(Clone, Debug)]
pub(crate) struct ImpactOptions {
    pub(crate) depth: usize,
    pub(crate) max_files: usize,
    pub(crate) max_tokens: usize,
    pub(crate) include_tests: bool,
    pub(crate) directions: BTreeSet<ImpactRole>,
}

impl ImpactOptions {
    pub(crate) fn bounded(mut self) -> Self {
        self.depth = self.depth.clamp(1, MAX_DEPTH);
        self.max_files = self.max_files.clamp(1, 500);
        self.max_tokens = self.max_tokens.clamp(64, 32_000);
        self
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ImpactRole {
    Pivot,
    Dependency,
    Dependent,
    Test,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct EdgeEvidence {
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) raw_target: String,
    pub(crate) kind: String,
    pub(crate) line: usize,
    pub(crate) resolver: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ImpactFile {
    pub(crate) path: String,
    pub(crate) roles: BTreeSet<ImpactRole>,
    pub(crate) depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) score: Option<f64>,
    pub(crate) evidence: Vec<EdgeEvidence>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct GraphObservation {
    pub(crate) source: String,
    pub(crate) raw_target: String,
    pub(crate) kind: String,
    pub(crate) line: usize,
    pub(crate) resolution: String,
    pub(crate) resolver: String,
    pub(crate) candidates: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ImpactResult {
    pub(crate) files: Vec<ImpactFile>,
    pub(crate) unresolved: Vec<GraphObservation>,
    pub(crate) truncated: bool,
    pub(crate) omitted_files: usize,
    pub(crate) omitted_observations: usize,
}

#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Reverse,
}

pub(crate) fn impact_from_query(
    conn: &Connection,
    query: &str,
    options: ImpactOptions,
) -> Result<ImpactResult> {
    let ranked = ranked_paths(conn, query, options.include_tests)?;
    impact_from_ranked_paths(conn, &ranked, MAX_PIVOTS, options)
}

pub(crate) fn impact_from_paths(
    conn: &Connection,
    paths: &[String],
    options: ImpactOptions,
) -> Result<ImpactResult> {
    let ranked = paths
        .iter()
        .map(|path| RankedPath {
            path: path.clone(),
            score: 0.0,
        })
        .collect::<Vec<_>>();
    let max_pivots = options.max_files;
    impact_from_ranked_paths(conn, &ranked, max_pivots, options)
}

fn impact_from_ranked_paths(
    conn: &Connection,
    ranked: &[RankedPath],
    max_pivots: usize,
    options: ImpactOptions,
) -> Result<ImpactResult> {
    let options = options.bounded();
    let pivots = ranked
        .iter()
        .take(max_pivots.min(options.max_files))
        .cloned()
        .collect::<Vec<_>>();
    let mut files = BTreeMap::<String, ImpactFile>::new();
    for pivot in &pivots {
        files.insert(
            pivot.path.clone(),
            ImpactFile {
                path: pivot.path.clone(),
                roles: BTreeSet::from([ImpactRole::Pivot]),
                depth: 0,
                score: Some(pivot.score),
                evidence: Vec::new(),
            },
        );
    }

    let pivot_paths = pivots
        .iter()
        .map(|pivot| pivot.path.clone())
        .collect::<Vec<_>>();
    let mut omitted_paths = BTreeSet::new();
    if options.directions.contains(&ImpactRole::Dependency) {
        traverse(
            conn,
            &pivot_paths,
            Direction::Forward,
            ImpactRole::Dependency,
            &options,
            &mut files,
            &mut omitted_paths,
        )?;
    }
    if options.directions.contains(&ImpactRole::Dependent)
        || options.directions.contains(&ImpactRole::Test)
    {
        traverse(
            conn,
            &pivot_paths,
            Direction::Reverse,
            ImpactRole::Dependent,
            &options,
            &mut files,
            &mut omitted_paths,
        )?;
    }

    let mut unresolved = observations_for(conn, &pivot_paths)?;
    unresolved.sort_by(|left, right| {
        (left.resolution != "ambiguous")
            .cmp(&(right.resolution != "ambiguous"))
            .then_with(|| left.source.cmp(&right.source))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.raw_target.cmp(&right.raw_target))
    });
    let observation_cap = 50usize;
    let omitted_observations = unresolved.len().saturating_sub(observation_cap);
    unresolved.truncate(observation_cap);
    let mut files = files.into_values().collect::<Vec<_>>();
    files.sort_by(|left, right| {
        role_order(left)
            .cmp(&role_order(right))
            .then_with(|| left.depth.cmp(&right.depth))
            .then_with(|| {
                right
                    .score
                    .partial_cmp(&left.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(ImpactResult {
        files,
        unresolved,
        truncated: !omitted_paths.is_empty() || omitted_observations > 0,
        omitted_files: omitted_paths.len(),
        omitted_observations,
    })
}

fn traverse(
    conn: &Connection,
    pivots: &[String],
    direction: Direction,
    role: ImpactRole,
    options: &ImpactOptions,
    files: &mut BTreeMap<String, ImpactFile>,
    omitted_paths: &mut BTreeSet<String>,
) -> Result<()> {
    let mut queue = VecDeque::new();
    let mut seen = BTreeSet::new();
    for (pivot_order, pivot) in pivots.iter().enumerate() {
        queue.push_back((pivot.clone(), 0usize, pivot_order));
        seen.insert(pivot.clone());
    }
    while let Some((path, depth, pivot_order)) = queue.pop_front() {
        if depth >= options.depth {
            continue;
        }
        let edges = edges_for(conn, &path, direction)?;
        for edge in edges {
            let next = match direction {
                Direction::Forward => edge.target.clone(),
                Direction::Reverse => edge.source.clone(),
            };
            let next_depth = depth + 1;
            let test = is_test_path(&next);
            if test && !options.include_tests {
                continue;
            }
            let display_role = if test { ImpactRole::Test } else { role.clone() };
            if test && !options.directions.contains(&ImpactRole::Test) {
                continue;
            }
            let include_file = test || options.directions.contains(&role);

            if include_file && let Some(existing) = files.get_mut(&next) {
                existing.roles.insert(display_role);
                existing.depth = existing.depth.min(next_depth);
                if !existing.evidence.iter().any(|item| same_edge(item, &edge)) {
                    existing.evidence.push(edge.clone());
                    sort_evidence(&mut existing.evidence);
                }
            } else if include_file && files.len() < options.max_files {
                files.insert(
                    next.clone(),
                    ImpactFile {
                        path: next.clone(),
                        roles: BTreeSet::from([display_role]),
                        depth: next_depth,
                        score: None,
                        evidence: vec![edge.clone()],
                    },
                );
            } else if include_file {
                omitted_paths.insert(next.clone());
            }
            if seen.insert(next.clone()) {
                queue.push_back((next, next_depth, pivot_order));
            }
        }
    }
    Ok(())
}

fn edges_for(conn: &Connection, path: &str, direction: Direction) -> Result<Vec<EdgeEvidence>> {
    let (sql, parameter) = match direction {
        Direction::Forward => (
            "SELECT source_path, target_path, raw_target, kind, line, resolver
             FROM dependency_edges
             WHERE resolution = 'exact' AND source_path = ?1
             ORDER BY target_path, source_path, raw_target, line",
            path,
        ),
        Direction::Reverse => (
            "SELECT source_path, target_path, raw_target, kind, line, resolver
             FROM dependency_edges
             WHERE resolution = 'exact' AND target_path = ?1
             ORDER BY source_path, target_path, raw_target, line",
            path,
        ),
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![parameter], |row| {
        Ok(EdgeEvidence {
            source: row.get(0)?,
            target: row.get(1)?,
            raw_target: row.get(2)?,
            kind: row.get(3)?,
            line: row.get::<_, i64>(4)? as usize,
            resolver: row.get(5)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn observations_for(conn: &Connection, paths: &[String]) -> Result<Vec<GraphObservation>> {
    let mut observations = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT source_path, raw_target, kind, line, resolution, resolver, candidate_paths_json
         FROM dependency_edges
         WHERE source_path = ?1 AND resolution != 'exact'
         ORDER BY source_path, line, raw_target, resolution",
    )?;
    for path in paths {
        let rows = stmt.query_map(params![path], |row| {
            let candidates_json = row.get::<_, String>(6)?;
            Ok(GraphObservation {
                source: row.get(0)?,
                raw_target: row.get(1)?,
                kind: row.get(2)?,
                line: row.get::<_, i64>(3)? as usize,
                resolution: row.get(4)?,
                resolver: row.get(5)?,
                candidates: serde_json::from_str(&candidates_json).unwrap_or_default(),
            })
        })?;
        observations.extend(rows.collect::<std::result::Result<Vec<_>, _>>()?);
    }
    Ok(observations)
}

fn role_order(file: &ImpactFile) -> usize {
    if file.roles.contains(&ImpactRole::Pivot) {
        0
    } else if file.roles.contains(&ImpactRole::Dependency) {
        1
    } else if file.roles.contains(&ImpactRole::Dependent) {
        2
    } else {
        3
    }
}

fn same_edge(left: &EdgeEvidence, right: &EdgeEvidence) -> bool {
    left.source == right.source
        && left.target == right.target
        && left.raw_target == right.raw_target
        && left.line == right.line
}

fn sort_evidence(evidence: &mut [EdgeEvidence]) {
    evidence.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.target.cmp(&right.target))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.raw_target.cmp(&right.raw_target))
    });
}

pub(crate) fn render_markdown(result: &ImpactResult, title: &str, max_tokens: usize) -> String {
    render_markdown_with_pivot_heading(result, title, "Lexical pivots", max_tokens)
}

pub(crate) fn render_markdown_with_pivot_heading(
    result: &ImpactResult,
    title: &str,
    pivot_heading: &str,
    max_tokens: usize,
) -> String {
    let max_tokens = max_tokens.clamp(64, 32_000);
    let mut out = format!("# {title}\n\n");
    let groups = [
        (ImpactRole::Pivot, pivot_heading),
        (ImpactRole::Dependency, "Dependencies"),
        (ImpactRole::Dependent, "Blast radius"),
        (ImpactRole::Test, "Likely tests"),
    ];
    let mut rendered = BTreeSet::new();
    let mut token_truncated = false;
    for (role, heading) in groups {
        let matching = result
            .files
            .iter()
            .filter(|file| file.roles.contains(&role) && !rendered.contains(&file.path))
            .collect::<Vec<_>>();
        if matching.is_empty() {
            continue;
        }
        let heading_text = format!("## {heading}\n\n");
        if estimate_tokens(&(out.clone() + &heading_text)) > max_tokens {
            token_truncated = true;
            break;
        }
        out.push_str(&heading_text);
        for file in matching {
            let roles = file
                .roles
                .iter()
                .map(|role| format!("{role:?}").to_lowercase())
                .collect::<Vec<_>>()
                .join(", ");
            let evidence = file
                .evidence
                .first()
                .map(|edge| {
                    format!(
                        "; via {}:{} `{}` ({})",
                        edge.source, edge.line, edge.raw_target, edge.resolver
                    )
                })
                .unwrap_or_default();
            let score = file
                .score
                .map(|score| format!("; score {score:.2}"))
                .unwrap_or_default();
            let line = format!(
                "- {} — roles: {}; depth {}{}{}\n",
                file.path, roles, file.depth, score, evidence
            );
            if estimate_tokens(&(out.clone() + &line)) > max_tokens {
                token_truncated = true;
                break;
            }
            out.push_str(&line);
            rendered.insert(file.path.clone());
        }
        out.push('\n');
        if token_truncated {
            break;
        }
    }

    if !result.unresolved.is_empty() && !token_truncated {
        let mut section = String::from("## Unresolved or ambiguous observations\n\n");
        let shown = result.unresolved.iter().take(12).collect::<Vec<_>>();
        for item in &shown {
            section.push_str(&format!(
                "- {}:{} `{}` — {} ({})\n",
                item.source, item.line, item.raw_target, item.resolution, item.resolver
            ));
        }
        let omitted = result
            .unresolved
            .len()
            .saturating_sub(shown.len())
            .saturating_add(result.omitted_observations);
        if omitted > 0 {
            section.push_str(&format!("- … {omitted} additional observations omitted\n"));
        }
        section.push('\n');
        if estimate_tokens(&(out.clone() + &section)) <= max_tokens {
            out.push_str(&section);
        } else {
            token_truncated = true;
        }
    }
    if result.truncated || token_truncated {
        out.push_str(&format!(
            "> Output truncated: {} graph files and {} observations omitted{}\n",
            result.omitted_files,
            result.omitted_observations,
            if token_truncated {
                " and token cap reached"
            } else {
                ""
            }
        ));
    }
    truncate_to_token_budget(out, max_tokens)
}

fn truncate_to_token_budget(mut output: String, max_tokens: usize) -> String {
    let max_bytes = max_tokens.saturating_mul(4);
    if output.len() <= max_bytes {
        return output;
    }
    while output.len() > max_bytes {
        output.pop();
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE files(path TEXT PRIMARY KEY, lang TEXT, hash TEXT, content TEXT);
            CREATE TABLE symbols(id INTEGER PRIMARY KEY, file_path TEXT, name TEXT, kind TEXT, line INTEGER, signature TEXT);
            CREATE TABLE imports(id INTEGER PRIMARY KEY, file_path TEXT, target TEXT);
            CREATE VIRTUAL TABLE file_fts USING fts5(path, content, symbols);
            CREATE TABLE dependency_edges(
                id INTEGER PRIMARY KEY, source_path TEXT, raw_target TEXT, target_path TEXT,
                kind TEXT, line INTEGER, resolution TEXT, resolver TEXT,
                candidate_paths_json TEXT DEFAULT '[]'
            );
            INSERT INTO files VALUES
                ('src/auth.rs', 'rust', '', 'token auth'),
                ('src/token.rs', 'rust', '', 'token parser'),
                ('src/api.rs', 'rust', '', 'auth endpoint'),
                ('tests/auth_test.rs', 'rust', '', 'auth integration test');
            INSERT INTO file_fts(path, content, symbols) VALUES
                ('src/auth.rs', 'token auth', ''),
                ('src/token.rs', 'token parser', ''),
                ('src/api.rs', 'auth endpoint', ''),
                ('tests/auth_test.rs', 'auth integration test', '');
            INSERT INTO dependency_edges(source_path, raw_target, target_path, kind, line, resolution, resolver) VALUES
                ('src/auth.rs', 'token', 'src/token.rs', 'use', 1, 'exact', 'rust-local'),
                ('src/api.rs', 'auth', 'src/auth.rs', 'use', 2, 'exact', 'rust-local'),
                ('tests/auth_test.rs', 'auth', 'src/auth.rs', 'use', 3, 'exact', 'rust-local'),
                ('src/auth.rs', 'external', NULL, 'use', 4, 'unresolved', 'rust-local');
            ",
        )
        .unwrap();
        conn
    }

    fn options(depth: usize, include_tests: bool) -> ImpactOptions {
        ImpactOptions {
            depth,
            max_files: 20,
            max_tokens: 2_000,
            include_tests,
            directions: BTreeSet::from([
                ImpactRole::Dependency,
                ImpactRole::Dependent,
                ImpactRole::Test,
            ]),
        }
    }

    #[test]
    fn aggregates_forward_reverse_and_test_roles() {
        let conn = fixture();
        let result = impact_from_paths(&conn, &["src/auth.rs".into()], options(1, true)).unwrap();
        assert!(result.files.iter().any(|file| {
            file.path == "src/token.rs" && file.roles.contains(&ImpactRole::Dependency)
        }));
        assert!(result.files.iter().any(|file| {
            file.path == "src/api.rs" && file.roles.contains(&ImpactRole::Dependent)
        }));
        assert!(result.files.iter().any(|file| {
            file.path == "tests/auth_test.rs" && file.roles.contains(&ImpactRole::Test)
        }));
        assert_eq!(result.unresolved.len(), 1);
    }

    #[test]
    fn depth_and_test_opt_in_are_hard_limits() {
        let conn = fixture();
        let result = impact_from_paths(&conn, &["src/token.rs".into()], options(1, false)).unwrap();
        assert!(result.files.iter().any(|file| file.path == "src/auth.rs"));
        assert!(!result.files.iter().any(|file| file.path == "src/api.rs"));
        assert!(!result.files.iter().any(|file| is_test_path(&file.path)));
    }

    #[test]
    fn cycles_and_shared_roles_render_once() {
        let conn = fixture();
        conn.execute(
            "INSERT INTO dependency_edges(source_path, raw_target, target_path, kind, line, resolution, resolver) VALUES (?1, ?2, ?3, 'use', 9, 'exact', 'rust-local')",
            params!["src/token.rs", "auth", "src/auth.rs"],
        )
        .unwrap();
        let result = impact_from_paths(&conn, &["src/auth.rs".into()], options(3, true)).unwrap();
        assert_eq!(
            result
                .files
                .iter()
                .filter(|file| file.path == "src/token.rs")
                .count(),
            1
        );
        let output = render_markdown(&result, "Kiv Scout Impact", 2_000);
        assert_eq!(output.matches("src/token.rs").count(), 1);
    }

    #[test]
    fn markdown_never_exceeds_the_token_budget_estimate() {
        let conn = fixture();
        let result = impact_from_paths(&conn, &["src/auth.rs".into()], options(3, true)).unwrap();
        let output = render_markdown(&result, "Kiv Scout Impact", 64);
        assert!(output.len() <= 64 * 4);
    }
}
