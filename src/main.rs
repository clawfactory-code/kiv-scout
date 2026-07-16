use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Parser, Subcommand};
use regex::Regex;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Instant, SystemTime};
use tree_sitter::{Language, Node, Parser as TsParser};
use walkdir::{DirEntry, WalkDir};

mod graph;
#[cfg(test)]
mod graph_eval;
mod scout;

use graph::{ImportKind, ImportRef, recompute_dependency_edges};
use scout::diff::{
    ChangedFileMapping, IndexedSymbolRange, LineRange, RangeEvidence, map_changed_files,
    read_git_diff,
};
use scout::policy::{
    ArchitectureConfig, ArchitecturePolicy, BoundaryCheckInput, BoundaryCheckResult,
    ExactDependencyEvidence,
};
use scout::{
    BenchArgs, CapsuleCap, CapsuleMode, ImpactOptions, ImpactResult, ImpactRole, capsule,
    graph_capsule, impact_from_paths, impact_from_query, render_impact_markdown,
    render_markdown_with_pivot_heading, render_skeleton,
};

const INDEX_SCHEMA_VERSION: &str = "3";

#[derive(Parser)]
#[command(name = "kiv-scout")]
#[command(about = "Local codebase index, skeleton, capsule, and MCP context server")]
#[command(version)]
struct Cli {
    /// Optional TOML config file. Defaults to ./kiv-scout.toml when present.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Automatically build or refresh the index before commands that need it.
    #[arg(long, global = true)]
    auto_index: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Index repositories or remove generated Kiv Scout index files.
    Index {
        /// Repository root, `all`, `remove`, or `remove all`.
        #[arg(value_name = "TARGET", num_args = 0..)]
        args: Vec<String>,
    },
    /// Print index status.
    Status {
        /// Repository root. Defaults to current directory or config default_repo.
        dir: Option<PathBuf>,
    },
    /// Print a token-efficient skeleton of a file.
    Skeleton {
        file: PathBuf,
        #[arg(long, default_value = "standard")]
        detail: String,
    },
    /// Generate a ranked context capsule for a query.
    Capsule {
        query: String,
        /// Preset context budget: default, files, tight, balanced, full, wide, or deep.
        #[arg(long)]
        cap: Option<String>,
        /// Override the preset token budget.
        #[arg(long)]
        max_tokens: Option<usize>,
        #[arg(long)]
        include_tests: bool,
        /// Override the preset render mode: full, compact, or files-only.
        #[arg(long)]
        mode: Option<String>,
        /// Override the preset file count.
        #[arg(long)]
        max_files: Option<usize>,
        /// Opt in to exact graph expansion: deps,rdeps,tests.
        #[arg(long, value_delimiter = ',')]
        related: Vec<String>,
        /// Exact graph traversal depth for related files (maximum 3).
        #[arg(long, default_value_t = 1)]
        related_depth: usize,
    },
    /// Show exact dependency impact for a lexical query or git diff.
    Impact {
        /// Lexical source-pointer query. Mutually exclusive with --diff.
        #[arg(required_unless_present = "diff", conflicts_with = "diff")]
        query: Option<String>,
        /// Use files changed relative to this local git reference as pivots.
        #[arg(long, conflicts_with = "query")]
        diff: Option<String>,
        #[arg(long, default_value_t = 1)]
        depth: usize,
        #[arg(long, default_value_t = 20)]
        max_files: usize,
        #[arg(long, default_value_t = 8_000)]
        max_tokens: usize,
        #[arg(long)]
        include_tests: bool,
        /// Output format: markdown or json.
        #[arg(long, default_value = "markdown")]
        format: String,
    },
    /// Check explicit repository architecture rules.
    Check {
        #[command(subcommand)]
        command: CheckCommands,
    },
    /// Benchmark index/status/capsule/skeleton paths for a repo.
    Bench(BenchArgs),
    /// Run a minimal MCP-compatible JSON-lines server over stdio.
    Mcp {
        /// Repository root. Defaults to current directory or config default_repo.
        dir: Option<PathBuf>,
    },
    /// Manage the background-friendly incremental index watcher.
    Watcher {
        #[command(subcommand)]
        command: WatcherCommands,
    },
}

#[derive(Subcommand)]
enum CheckCommands {
    /// Check exact dependency edges against configured layer deny rules.
    Boundaries {
        /// Limit the check to outgoing edges from one repo-relative file.
        #[arg(long)]
        path: Option<String>,
        /// Output format: markdown or json.
        #[arg(long, default_value = "markdown")]
        format: String,
    },
}

#[derive(Subcommand)]
enum WatcherCommands {
    /// Run the external watch command to incrementally update watched repos.
    Start {
        /// Seconds between watch passes.
        #[arg(long, default_value_t = 5)]
        interval_secs: u64,
        /// Run one pass and exit. Useful for cron, launchd, tests, or manual checks.
        #[arg(long)]
        once: bool,
    },
    /// Print watched repositories.
    List,
    /// Add a repository to the watcher list without indexing it.
    Add {
        /// Repository root.
        dir: PathBuf,
    },
    /// Remove a repository from the watcher list.
    Remove {
        /// Repository root.
        dir: PathBuf,
    },
}

#[derive(Debug, Clone)]
struct SourcePath {
    path: String,
    lang: String,
    abs: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Symbol {
    name: String,
    kind: String,
    line: usize,
    end_line: usize,
    range_evidence: String,
    signature: String,
}

#[derive(Debug, Clone)]
struct IndexedFile {
    path: String,
    lang: String,
    hash: String,
    text: String,
    symbols: Vec<Symbol>,
    imports: Vec<ImportRef>,
}

#[derive(Debug, Serialize)]
struct Status {
    repo_root: String,
    indexed_at: String,
    files: i64,
    symbols: i64,
    imports: i64,
    exact_edges: i64,
    ambiguous_imports: i64,
    unresolved_imports: i64,
}

#[derive(Debug, Default, Deserialize)]
struct Config {
    /// Default repository to index/query when a command does not pass a path.
    default_repo: Option<PathBuf>,
    /// Default capsule preset. CLI --cap overrides this.
    default_capsule: Option<String>,
    /// Default capsule token budget. CLI --max-tokens overrides this.
    max_tokens: Option<usize>,
    /// Default capsule file count. CLI --max-files overrides this.
    max_files: Option<usize>,
    /// Include test files in capsules by default.
    include_tests: Option<bool>,
    /// Automatically build or refresh the index before commands that need it.
    auto_index: Option<bool>,
    /// Max token budget accepted by the MCP get_context_capsule tool.
    mcp_max_tokens: Option<usize>,
    /// Max file count accepted by the MCP get_context_capsule tool.
    mcp_max_files: Option<usize>,
    /// Explicit repository-owned architecture policy; no policy is inferred.
    architecture: Option<ArchitectureConfig>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = load_config(cli.config.as_deref())?;
    let auto_index = cli.auto_index || config.auto_index.unwrap_or(false);
    match cli.command {
        Commands::Index { args } => handle_index_command(args, &config)?,
        Commands::Status { dir } => {
            let root = repo_root(dir.or_else(|| config.default_repo.clone()))?;
            let conn = open_db_maybe_auto_index(&root, auto_index)?;
            let status = load_status(&conn, &root)?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Commands::Skeleton { file, detail } => {
            let root = repo_root(config.default_repo.clone())?;
            let target = if file.is_absolute() {
                file
            } else {
                root.join(file)
            };
            let text = fs::read_to_string(&target)
                .with_context(|| format!("failed to read {}", target.display()))?;
            let rel = relative_path(&root, &target)?;
            let lang = language_for(&rel);
            print!("{}", render_skeleton(&rel, &lang, &text, &detail));
        }
        Commands::Capsule {
            query,
            cap,
            max_tokens,
            include_tests,
            mode,
            max_files,
            related,
            related_depth,
        } => {
            let root = repo_root(config.default_repo.clone())?;
            let conn = open_db_maybe_auto_index(&root, auto_index)?;
            let cap_name = cap
                .or_else(|| config.default_capsule.clone())
                .unwrap_or_else(|| "default".to_string());
            let cap = CapsuleCap::parse(&cap_name)?;
            let mode = mode.as_deref().map(CapsuleMode::parse).unwrap_or(cap.mode);
            let max_tokens = max_tokens.or(config.max_tokens).unwrap_or(cap.max_tokens);
            let max_files = max_files.or(config.max_files).unwrap_or(cap.max_files);
            let include_tests = include_tests || config.include_tests.unwrap_or(false);
            if related.is_empty() {
                print!(
                    "{}",
                    capsule(&conn, &query, max_tokens, include_tests, mode, max_files)?
                );
            } else {
                let directions = parse_related_roles(&related)?;
                let result = impact_from_query(
                    &conn,
                    &query,
                    ImpactOptions {
                        depth: related_depth,
                        max_files,
                        max_tokens,
                        include_tests: include_tests || directions.contains(&ImpactRole::Test),
                        directions,
                    },
                )?;
                print!(
                    "{}",
                    graph_capsule(&conn, &query, &result, max_tokens, mode)?
                );
            }
        }
        Commands::Impact {
            query,
            diff,
            depth,
            max_files,
            max_tokens,
            include_tests,
            format,
        } => {
            validate_output_format(&format)?;
            let depth = depth.clamp(1, 3);
            let max_files = max_files.clamp(1, 500);
            let max_tokens = max_tokens.clamp(64, 32_000);
            let root = repo_root(config.default_repo.clone())?;
            let conn = open_db_maybe_auto_index(&root, auto_index)?;
            let options = default_impact_options(depth, max_files, max_tokens, include_tests);
            if let Some(reference) = diff {
                let diff = read_git_diff(&root, &reference)?;
                let indexed_at = load_status(&conn, &root)?.indexed_at;
                let symbol_ranges = load_symbol_ranges(&conn)?;
                let mut changed = map_changed_files(&diff, &symbol_ranges);
                let changed_total = changed.len();
                changed.truncate(max_files);
                let paths = changed
                    .iter()
                    .filter(|file| symbol_ranges.contains_key(&file.path))
                    .map(|file| file.path.clone())
                    .collect::<Vec<_>>();
                let impact = impact_from_paths(&conn, &paths, options)?;
                render_diff_impact(
                    &changed,
                    changed_total,
                    &impact,
                    &reference,
                    &indexed_at,
                    &format,
                    max_tokens,
                )?;
            } else {
                let query = query.context("impact requires a query or --diff")?;
                let result = impact_from_query(&conn, &query, options)?;
                render_impact(&result, &format, max_tokens)?;
            }
        }
        Commands::Check { command } => match command {
            CheckCommands::Boundaries { path, format } => {
                validate_output_format(&format)?;
                let root = repo_root(config.default_repo.clone())?;
                let conn = open_db_maybe_auto_index(&root, auto_index)?;
                let Some(result) =
                    evaluate_boundaries(&conn, config.architecture.as_ref(), path.as_deref())?
                else {
                    println!("no architecture policy configured");
                    return Ok(());
                };
                if format == "json" {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    print!("{}", result.render_markdown());
                }
                if !result.violations.is_empty() {
                    bail!("architecture boundary check found violations");
                }
            }
        },
        Commands::Bench(args) => scout::bench_command(args)?,
        Commands::Mcp { dir } => {
            let root = repo_root(dir.or_else(|| config.default_repo.clone()))?;
            run_mcp(&root, &config, auto_index)?;
        }
        Commands::Watcher { command } => handle_watcher_command(command)?,
    }
    Ok(())
}

fn validate_output_format(format: &str) -> Result<()> {
    if matches!(format, "markdown" | "json") {
        Ok(())
    } else {
        bail!("unknown output format '{format}'; expected markdown or json")
    }
}

fn parse_related_roles(values: &[String]) -> Result<BTreeSet<ImpactRole>> {
    let mut roles = BTreeSet::new();
    for value in values {
        match value.as_str() {
            "deps" | "dependencies" => {
                roles.insert(ImpactRole::Dependency);
            }
            "rdeps" | "dependents" => {
                roles.insert(ImpactRole::Dependent);
            }
            "tests" => {
                roles.insert(ImpactRole::Test);
            }
            other => bail!("unknown related role '{other}'; expected deps,rdeps,tests"),
        }
    }
    Ok(roles)
}

fn default_impact_options(
    depth: usize,
    max_files: usize,
    max_tokens: usize,
    include_tests: bool,
) -> ImpactOptions {
    ImpactOptions {
        depth,
        max_files,
        max_tokens,
        include_tests,
        directions: BTreeSet::from([
            ImpactRole::Dependency,
            ImpactRole::Dependent,
            ImpactRole::Test,
        ]),
    }
}

fn render_impact(result: &ImpactResult, format: &str, max_tokens: usize) -> Result<()> {
    if format == "json" {
        println!("{}", serde_json::to_string_pretty(result)?);
    } else {
        print!(
            "{}",
            render_impact_markdown(result, "Kiv Scout Impact", max_tokens)
        );
    }
    Ok(())
}

fn render_diff_impact(
    changed: &[ChangedFileMapping],
    changed_total: usize,
    impact: &ImpactResult,
    reference: &str,
    indexed_at: &str,
    format: &str,
    max_tokens: usize,
) -> Result<()> {
    if format == "json" {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "reference": reference,
                "indexed_at": indexed_at,
                "changed": changed,
                "changed_total": changed_total,
                "changed_truncated": changed_total > changed.len(),
                "impact": impact,
            }))?
        );
        return Ok(());
    }

    print!(
        "{}",
        diff_impact_markdown(
            changed,
            changed_total,
            impact,
            reference,
            indexed_at,
            max_tokens,
        )
    );
    Ok(())
}

fn diff_impact_markdown(
    changed: &[ChangedFileMapping],
    changed_total: usize,
    impact: &ImpactResult,
    reference: &str,
    indexed_at: &str,
    max_tokens: usize,
) -> String {
    let mut output = format!(
        "# Kiv Scout Diff Impact\n\n**Reference:** `{reference}`\n\n**Index snapshot:** `{indexed_at}`. Unindexed paths remain visible; refresh with `--auto-index` when current graph coverage matters.\n\n## Changed\n\n"
    );
    for file in changed {
        let flags = [
            file.binary.then_some("binary"),
            file.deleted.then_some("deleted"),
            file.unindexed.then_some("unindexed"),
            file.module_level.then_some("module-level"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        output.push_str(&format!(
            "- `{}` — {:?}{}\n",
            file.path,
            file.kind,
            if flags.is_empty() {
                String::new()
            } else {
                format!("; {flags}")
            }
        ));
        for symbol in &file.symbols {
            output.push_str(&format!(
                "  - `{}` {} lines {}-{} ({:?})\n",
                symbol.kind, symbol.name, symbol.lines.start, symbol.lines.end, symbol.evidence
            ));
        }
    }
    if changed_total > changed.len() {
        output.push_str(&format!(
            "- … {} additional changed files omitted by the file cap\n",
            changed_total - changed.len()
        ));
    }
    output.push('\n');
    let remaining = max_tokens.saturating_sub(output.len() / 4).max(64);
    let impact_markdown =
        render_markdown_with_pivot_heading(impact, "Graph impact", "Changed pivots", remaining);
    output.push_str(&impact_markdown);
    if output.len() / 4 > max_tokens {
        let marker = "\n\n[diff impact truncated by token cap]\n";
        let max_bytes = max_tokens.saturating_mul(4);
        while output.len().saturating_add(marker.len()) > max_bytes {
            if output.pop().is_none() {
                break;
            }
        }
        output.push_str(marker);
    }
    output
}

fn load_symbol_ranges(conn: &Connection) -> Result<BTreeMap<String, Vec<IndexedSymbolRange>>> {
    let mut result = BTreeMap::<String, Vec<IndexedSymbolRange>>::new();
    let mut files = conn.prepare("SELECT path FROM files ORDER BY path")?;
    for path in files
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
    {
        result.insert(path, Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT file_path, name, kind, line, end_line, range_evidence
         FROM symbols ORDER BY file_path, line, end_line, name",
    )?;
    let rows = stmt.query_map([], |row| {
        let start = row.get::<_, i64>(3)? as usize;
        let end = row.get::<_, i64>(4)? as usize;
        Ok((
            row.get::<_, String>(0)?,
            IndexedSymbolRange {
                name: row.get(1)?,
                kind: row.get(2)?,
                lines: LineRange::new(start, end).unwrap_or(LineRange { start: 1, end: 1 }),
                evidence: match row.get::<_, String>(5)?.as_str() {
                    "exact-range" => RangeEvidence::ExactRange,
                    _ => RangeEvidence::Approximate,
                },
            },
        ))
    })?;
    for row in rows {
        let (path, symbol) = row?;
        result.entry(path).or_default().push(symbol);
    }
    Ok(result)
}

fn evaluate_boundaries(
    conn: &Connection,
    config: Option<&ArchitectureConfig>,
    path: Option<&str>,
) -> Result<Option<BoundaryCheckResult>> {
    let Some(policy) = ArchitecturePolicy::from_config(config)? else {
        return Ok(None);
    };
    let checked_paths = if let Some(path) = path {
        let exists = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE path = ?1)",
            params![path],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            bail!("indexed path not found: {path}");
        }
        vec![path.to_string()]
    } else {
        let mut stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
        stmt.query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?
    };

    let mut sql = String::from(
        "SELECT source_path, target_path, raw_target, kind, line, resolver
         FROM dependency_edges WHERE resolution = 'exact'",
    );
    if path.is_some() {
        sql.push_str(" AND source_path = ?1");
    }
    sql.push_str(" ORDER BY source_path, target_path, line, raw_target");
    let mut stmt = conn.prepare(&sql)?;
    let map_edge = |row: &rusqlite::Row<'_>| {
        Ok(ExactDependencyEvidence {
            source: row.get(0)?,
            target: row.get(1)?,
            raw_target: row.get(2)?,
            kind: row.get(3)?,
            line: row.get::<_, i64>(4)? as usize,
            resolver: row.get(5)?,
        })
    };
    let exact_edges = if let Some(path) = path {
        stmt.query_map(params![path], map_edge)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map([], map_edge)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let unresolved_observations = if let Some(path) = path {
        conn.query_row(
            "SELECT COUNT(*) FROM dependency_edges WHERE resolution != 'exact' AND source_path = ?1",
            params![path],
            |row| row.get::<_, i64>(0),
        )? as usize
    } else {
        conn.query_row(
            "SELECT COUNT(*) FROM dependency_edges WHERE resolution != 'exact'",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize
    };
    Ok(Some(policy.evaluate(BoundaryCheckInput {
        checked_paths: &checked_paths,
        exact_edges: &exact_edges,
        unresolved_observations,
    })?))
}

fn load_config(path: Option<&Path>) -> Result<Config> {
    let path = path
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("KIV_SCOUT_CONFIG").map(PathBuf::from))
        .or_else(|| {
            let local = PathBuf::from("kiv-scout.toml");
            local.exists().then_some(local)
        });
    let Some(path) = path else {
        return Ok(Config::default());
    };
    if !path.exists() {
        bail!("config file not found: {}", path.display());
    }
    let text =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

fn handle_index_command(args: Vec<String>, config: &Config) -> Result<()> {
    match args.as_slice() {
        [] => {
            let root = repo_root(config.default_repo.clone())?;
            index_one_repo(&root)
        }
        [target] if target == "all" => index_all_repos(),
        [target] if target == "remove" => {
            let root = repo_root(config.default_repo.clone())?;
            remove_index_for_repo(&root)
        }
        [command, target] if command == "remove" && target == "all" => remove_all_indexes(),
        [command, target] if command == "remove" => {
            let root = repo_root(Some(PathBuf::from(target)))?;
            remove_index_for_repo(&root)
        }
        [target] => {
            let root = repo_root(Some(PathBuf::from(target)))?;
            index_one_repo(&root)
        }
        _ => bail!(
            "unsupported index arguments. Use `kiv-scout index [repo]`, `kiv-scout index all`, `kiv-scout index remove [repo]`, or `kiv-scout index remove all`"
        ),
    }
}

fn index_one_repo(root: &Path) -> Result<()> {
    let status = index_repo(root)?;
    add_watch_repo(root)?;
    println!(
        "Indexed {} files, {} symbols, {} imports into {}",
        status.files,
        status.symbols,
        status.imports,
        db_path(root).display()
    );
    Ok(())
}

fn index_all_repos() -> Result<()> {
    let repos = discover_user_repos()?;
    if repos.is_empty() {
        bail!(
            "no git repositories found. Set KIV_SCOUT_INDEX_ROOTS to a path-separated list of roots to scan."
        );
    }
    println!("Discovered {} repositories", repos.len());
    for repo in repos {
        index_one_repo(&repo)?;
    }
    Ok(())
}

fn remove_index_for_repo(root: &Path) -> Result<()> {
    let removed = remove_index_files(root)?;
    remove_watch_repo(root)?;
    println!(
        "Removed {} Kiv Scout index files from {}",
        removed,
        root.display()
    );
    Ok(())
}

fn remove_all_indexes() -> Result<()> {
    let mut repos = read_watchlist()?;
    repos.extend(discover_user_repos()?);
    repos.sort();
    repos.dedup();
    if repos.is_empty() {
        bail!(
            "no git repositories found in the watchlist or scan roots. Set KIV_SCOUT_INDEX_ROOTS to a path-separated list of roots to scan."
        );
    }
    let mut total = 0;
    for repo in &repos {
        total += remove_index_files(repo)?;
        let _ = remove_watch_repo(repo);
    }
    println!(
        "Removed {} Kiv Scout index files from {} repositories",
        total,
        repos.len()
    );
    Ok(())
}

fn repo_root(dir: Option<PathBuf>) -> Result<PathBuf> {
    let start = dir.unwrap_or(std::env::current_dir()?);
    let start = fs::canonicalize(&start)
        .with_context(|| format!("failed to canonicalize {}", start.display()))?;
    let mut cur = start.as_path();
    loop {
        if cur.join(".git").exists() || cur.join(".kiv").exists() {
            return Ok(cur.to_path_buf());
        }
        match cur.parent() {
            Some(parent) => cur = parent,
            None => return Ok(start),
        }
    }
}

fn discover_user_repos() -> Result<Vec<PathBuf>> {
    let roots = index_scan_roots()?;
    let mut repos = BTreeSet::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(should_descend_for_repo_discovery)
            .flatten()
        {
            if entry.file_type().is_dir()
                && entry.path().join(".git").exists()
                && let Ok(path) = fs::canonicalize(entry.path())
            {
                repos.insert(path);
            }
        }
    }
    Ok(repos.into_iter().collect())
}

fn index_scan_roots() -> Result<Vec<PathBuf>> {
    if let Some(roots) = env::var_os("KIV_SCOUT_INDEX_ROOTS") {
        return Ok(env::split_paths(&roots).collect());
    }
    let home = env::var_os("HOME").context("HOME is not set; set KIV_SCOUT_INDEX_ROOTS")?;
    Ok(vec![PathBuf::from(home)])
}

fn should_descend_for_repo_discovery(entry: &DirEntry) -> bool {
    should_descend_for_repo_discovery_name(&entry.file_name().to_string_lossy(), entry.depth())
}

fn should_descend_for_repo_discovery_name(name: &str, depth: usize) -> bool {
    let name = name.to_ascii_lowercase();
    if depth == 0 {
        return true;
    }
    !matches!(
        name.as_str(),
        ".git"
            | ".kiv"
            | ".research"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | "snapshots"
            | ".next"
            | ".venv"
            | "venv"
            | "env"
            | ".env"
            | "site-packages"
            | "__pypackages__"
            | "__pycache__"
            | ".pytest_cache"
            | ".mypy_cache"
            | ".ruff_cache"
            | ".tox"
            | ".nox"
            | "htmlcov"
            | "coverage"
            | ".parcel-cache"
            | ".turbo"
            | ".cache"
            | ".cargo"
            | ".npm"
            | ".rustup"
            | ".local"
            | ".trash"
            | "library"
            | "applications"
            | "downloads"
            | "movies"
            | "music"
            | "pictures"
            | "public"
    ) && !name.starts_with(".venv-")
        && !name.starts_with(".venv_")
        && !name.starts_with("venv-")
        && !name.starts_with("venv_")
}

fn db_path(root: &Path) -> PathBuf {
    root.join(".kiv").join("index.db")
}

fn remove_index_files(root: &Path) -> Result<usize> {
    let kiv_dir = root.join(".kiv");
    let names = ["index.db", "index.db-wal", "index.db-shm"];
    let mut removed = 0;
    for name in names {
        let path = kiv_dir.join(name);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            removed += 1;
        }
    }
    if kiv_dir.exists() && fs::read_dir(&kiv_dir)?.next().is_none() {
        fs::remove_dir(&kiv_dir)
            .with_context(|| format!("failed to remove {}", kiv_dir.display()))?;
    }
    Ok(removed)
}

fn open_db(root: &Path) -> Result<Connection> {
    let path = db_path(root);
    if !path.exists() {
        bail!(
            "index not found at {}; run `kiv-scout index` first",
            path.display()
        );
    }
    Connection::open(path).context("failed to open index database")
}

fn open_db_maybe_auto_index(root: &Path, auto_index: bool) -> Result<Connection> {
    if auto_index {
        auto_index_if_needed(root)?;
    }
    open_db(root)
}

fn auto_index_if_needed(root: &Path) -> Result<()> {
    let path = db_path(root);
    if !path.exists() {
        if io::stderr().is_terminal() {
            eprintln!("Auto-indexing {} (missing index)", root.display());
        }
        index_repo(root)?;
        return Ok(());
    }

    let mut conn = Connection::open(&path).context("failed to open index database")?;
    init_schema(&conn)?;
    let result = update_index_incremental(&mut conn, root, &path)?;
    if result.changed() && io::stderr().is_terminal() {
        eprintln!(
            "Auto-index updated {} ({} changed/new, {} removed)",
            root.display(),
            result.upserted,
            result.deleted
        );
    }
    Ok(())
}

#[derive(Debug, Default)]
struct IncrementalIndexResult {
    upserted: usize,
    deleted: usize,
    touched_unchanged: usize,
    rebuilt: bool,
}

impl IncrementalIndexResult {
    fn changed(&self) -> bool {
        self.rebuilt || self.upserted > 0 || self.deleted > 0
    }

    fn observed_newer_sources(&self) -> bool {
        self.changed() || self.touched_unchanged > 0
    }
}

fn update_index_incremental(
    conn: &mut Connection,
    root: &Path,
    index_path: &Path,
) -> Result<IncrementalIndexResult> {
    let index_modified = modified_time(index_path)?;
    let sources = collect_source_paths(root)?;
    let existing = indexed_file_hashes(conn)?;
    let current_paths = sources
        .iter()
        .map(|source| source.path.clone())
        .collect::<BTreeSet<_>>();
    let deleted = existing
        .keys()
        .filter(|path| !current_paths.contains(*path))
        .cloned()
        .collect::<Vec<_>>();

    let mut upserts = Vec::new();
    let mut touched_unchanged = 0;
    for source in sources {
        let known_hash = existing.get(&source.path);
        let should_check = known_hash.is_none()
            || modified_time(&source.abs)
                .map(|modified| modified > index_modified)
                .unwrap_or(false);
        if !should_check {
            continue;
        }
        let text = match fs::read_to_string(&source.abs) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let hash = hash_text(&text);
        if known_hash == Some(&hash) {
            touched_unchanged += 1;
            continue;
        }
        let symbols = extract_symbols(&source.lang, &text);
        let imports = extract_import_refs(&source.lang, &text);
        upserts.push(IndexedFile {
            path: source.path,
            lang: source.lang,
            hash,
            text,
            symbols,
            imports,
        });
    }

    let result = IncrementalIndexResult {
        upserted: upserts.len(),
        deleted: deleted.len(),
        touched_unchanged,
        rebuilt: false,
    };
    if result.observed_newer_sources() {
        write_index_delta(conn, root, &upserts, &deleted)?;
    }
    Ok(result)
}

fn indexed_file_hashes(conn: &Connection) -> Result<BTreeMap<String, String>> {
    let mut stmt = conn.prepare("SELECT path, hash FROM files")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut hashes = BTreeMap::new();
    for row in rows {
        let (path, hash) = row?;
        hashes.insert(path, hash);
    }
    Ok(hashes)
}

fn modified_time(path: &Path) -> Result<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .with_context(|| format!("failed to read modified time for {}", path.display()))
}

fn handle_watcher_command(command: WatcherCommands) -> Result<()> {
    match command {
        WatcherCommands::Start {
            interval_secs,
            once,
        } => start_watcher(interval_secs, once),
        WatcherCommands::List => {
            for repo in read_watchlist()? {
                println!("{}", repo.display());
            }
            Ok(())
        }
        WatcherCommands::Add { dir } => {
            let root = repo_root(Some(dir))?;
            add_watch_repo(&root)?;
            println!("Watching {}", root.display());
            Ok(())
        }
        WatcherCommands::Remove { dir } => {
            let root = repo_root(Some(dir))?;
            remove_watch_repo(&root)?;
            println!("Removed {}", root.display());
            Ok(())
        }
    }
}

fn start_watcher(interval_secs: u64, once: bool) -> Result<()> {
    if once {
        return watcher_pass();
    }
    ensure_watch_command()?;
    let exe = env::current_exe().context("failed to resolve current executable")?;
    eprintln!("{}", watcher_platform_note(interval_secs.max(1)));
    let status = ProcessCommand::new("watch")
        .arg("-n")
        .arg(interval_secs.max(1).to_string())
        .arg(exe)
        .arg("watcher")
        .arg("start")
        .arg("--once")
        .status()
        .context("failed to start external `watch` command")?;
    if !status.success() {
        bail!("external `watch` command exited with status {status}");
    }
    Ok(())
}

fn watcher_pass() -> Result<()> {
    let repos = read_watchlist()?;
    if repos.is_empty() {
        eprintln!(
            "No repositories are in the Kiv Scout watchlist. Run `kiv-scout index /path/to/repo` first."
        );
        return Ok(());
    }
    for repo in repos {
        if !repo.exists() {
            eprintln!("Skipping missing watched repo {}", repo.display());
            continue;
        }
        let result = watcher_update_repo(&repo)?;
        if result.rebuilt {
            eprintln!("Rebuilt {}", repo.display());
        } else if result.changed() {
            eprintln!(
                "Updated {} ({} changed/new, {} removed)",
                repo.display(),
                result.upserted,
                result.deleted
            );
        }
    }
    Ok(())
}

fn watcher_platform_note(interval_secs: u64) -> String {
    match env::consts::OS {
        "macos" => format!(
            "Kiv Scout watcher starting on macOS via external `watch` every {interval_secs}s."
        ),
        "linux" => format!(
            "Kiv Scout watcher starting on Linux via external `watch` every {interval_secs}s."
        ),
        _ => format!("Kiv Scout watcher starting via external `watch` every {interval_secs}s."),
    }
}

fn ensure_watch_command() -> Result<()> {
    if command_exists("watch") {
        return Ok(());
    }
    eprintln!("{}", watch_missing_message());
    if !io::stdin().is_terminal() {
        bail!("external `watch` command is required for `kiv-scout watcher start`");
    }
    if prompt_yes_no("Install `watch` now? [y/N] ")? {
        install_watch_command()?;
        if command_exists("watch") {
            return Ok(());
        }
        bail!("`watch` still was not found on PATH after install attempt");
    }
    bail!("external `watch` command is required for `kiv-scout watcher start`")
}

fn command_exists(name: &str) -> bool {
    env::var_os("PATH")
        .map(|paths| {
            env::split_paths(&paths).any(|path| {
                let candidate = path.join(name);
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

fn prompt_yes_no(prompt: &str) -> Result<bool> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

fn install_watch_command() -> Result<()> {
    let command = watch_install_command().ok_or_else(|| {
        anyhow::anyhow!("no supported package manager found for installing `watch`")
    })?;
    eprintln!("Running: {}", command.join(" "));
    let status = ProcessCommand::new(&command[0])
        .args(&command[1..])
        .status()
        .with_context(|| format!("failed to run {}", command.join(" ")))?;
    if !status.success() {
        bail!("install command exited with status {status}");
    }
    Ok(())
}

fn watch_install_command() -> Option<Vec<String>> {
    match env::consts::OS {
        "macos" => command_exists("brew").then(|| {
            vec![
                "brew".to_string(),
                "install".to_string(),
                "watch".to_string(),
            ]
        }),
        "linux" => {
            if command_exists("apt-get") {
                Some(vec![
                    "sudo".to_string(),
                    "apt-get".to_string(),
                    "install".to_string(),
                    "-y".to_string(),
                    "procps".to_string(),
                ])
            } else if command_exists("dnf") {
                Some(vec![
                    "sudo".to_string(),
                    "dnf".to_string(),
                    "install".to_string(),
                    "-y".to_string(),
                    "procps-ng".to_string(),
                ])
            } else if command_exists("pacman") {
                Some(vec![
                    "sudo".to_string(),
                    "pacman".to_string(),
                    "-S".to_string(),
                    "procps-ng".to_string(),
                ])
            } else if command_exists("brew") {
                Some(vec![
                    "brew".to_string(),
                    "install".to_string(),
                    "watch".to_string(),
                ])
            } else {
                None
            }
        }
        _ => None,
    }
}

fn watch_missing_message() -> &'static str {
    match env::consts::OS {
        "macos" => {
            "The external `watch` command is required but was not found. On macOS, install it with `brew install watch`."
        }
        "linux" => {
            "The external `watch` command is required but was not found. Install it with your package manager, for example `sudo apt-get install procps`, `sudo dnf install procps-ng`, or `sudo pacman -S procps-ng`."
        }
        _ => "The external `watch` command is required but was not found on PATH.",
    }
}

fn watcher_update_repo(root: &Path) -> Result<IncrementalIndexResult> {
    let path = db_path(root);
    if !path.exists() {
        index_repo(root)?;
        return Ok(IncrementalIndexResult {
            rebuilt: true,
            ..IncrementalIndexResult::default()
        });
    }
    let mut conn = Connection::open(&path).context("failed to open index database")?;
    init_schema(&conn)?;
    update_index_incremental(&mut conn, root, &path)
}

fn add_watch_repo(root: &Path) -> Result<()> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;
    let mut repos = read_watchlist()?;
    if !repos.iter().any(|repo| repo == &root) {
        repos.push(root);
        write_watchlist(&repos)?;
    }
    Ok(())
}

fn remove_watch_repo(root: &Path) -> Result<()> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;
    let mut repos = read_watchlist()?;
    repos.retain(|repo| repo != &root);
    write_watchlist(&repos)
}

fn read_watchlist() -> Result<Vec<PathBuf>> {
    let path = watchlist_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut repos = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    repos.sort();
    repos.dedup();
    Ok(repos)
}

fn write_watchlist(repos: &[PathBuf]) -> Result<()> {
    let path = watchlist_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut repos = repos.to_vec();
    repos.sort();
    repos.dedup();
    let text = repos
        .iter()
        .map(|repo| repo.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, format!("{text}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

fn watchlist_path() -> Result<PathBuf> {
    if let Some(home) = env::var_os("KIV_SCOUT_HOME") {
        return Ok(PathBuf::from(home).join("watchlist"));
    }
    if let Some(state_home) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(state_home)
            .join("kiv-scout")
            .join("watchlist"));
    }
    let home = env::var_os("HOME").context("HOME is not set; set KIV_SCOUT_HOME")?;
    Ok(PathBuf::from(home).join(".kiv-scout").join("watchlist"))
}

fn index_repo(root: &Path) -> Result<Status> {
    let mut progress = IndexProgress::new(root);
    fs::create_dir_all(root.join(".kiv"))?;
    let mut conn = Connection::open(db_path(root))?;
    init_schema_for_rebuild(&conn)?;
    progress.phase("Discovering source files");
    let files = collect_source_paths(root)?;
    progress.set_total(files.len());
    let mut indexed = Vec::with_capacity(files.len());
    for file in files {
        let text = match fs::read_to_string(&file.abs) {
            Ok(text) => text,
            Err(_) => {
                progress.tick();
                continue;
            }
        };
        let hash = hash_text(&text);
        let symbols = extract_symbols(&file.lang, &text);
        let imports = extract_import_refs(&file.lang, &text);
        indexed.push(IndexedFile {
            path: file.path,
            lang: file.lang,
            hash,
            text,
            symbols,
            imports,
        });
        progress.tick();
    }
    progress.phase("Writing index database");
    write_index(&mut conn, root, &indexed)?;
    let status = load_status(&conn, root)?;
    progress.finish(&status);
    Ok(status)
}

fn init_schema(conn: &Connection) -> Result<()> {
    let table_count = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if table_count > 0 {
        let version = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok();
        if version.as_deref() != Some(INDEX_SCHEMA_VERSION) {
            bail!(
                "Kiv Scout index schema is incompatible (found {}, need {}); run `kiv-scout index` to rebuild {}",
                version.as_deref().unwrap_or("legacy/unknown"),
                INDEX_SCHEMA_VERSION,
                "the generated index"
            );
        }
    }
    create_schema(conn)
}

fn init_schema_for_rebuild(conn: &Connection) -> Result<()> {
    if init_schema(conn).is_err() {
        conn.execute_batch(
            "DROP TABLE IF EXISTS file_fts;
             DROP TABLE IF EXISTS dependency_edges;
             DROP TABLE IF EXISTS imports;
             DROP TABLE IF EXISTS symbols;
             DROP TABLE IF EXISTS files;
             DROP TABLE IF EXISTS metadata;",
        )?;
    }
    create_schema(conn)
}

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS files (
            path TEXT PRIMARY KEY,
            lang TEXT NOT NULL,
            hash TEXT NOT NULL,
            content TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS symbols (
            id INTEGER PRIMARY KEY,
            file_path TEXT NOT NULL,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            range_evidence TEXT NOT NULL,
            signature TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS imports (
            id INTEGER PRIMARY KEY,
            file_path TEXT NOT NULL,
            target TEXT NOT NULL,
            kind TEXT NOT NULL,
            line INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS dependency_edges (
            id INTEGER PRIMARY KEY,
            source_path TEXT NOT NULL,
            raw_target TEXT NOT NULL,
            target_path TEXT,
            kind TEXT NOT NULL,
            line INTEGER NOT NULL,
            resolution TEXT NOT NULL CHECK (
                resolution IN ('exact', 'ambiguous', 'unresolved')
            ),
            resolver TEXT NOT NULL,
            candidate_paths_json TEXT NOT NULL DEFAULT '[]'
        );
        CREATE INDEX IF NOT EXISTS dependency_edges_source_idx
            ON dependency_edges(source_path);
        CREATE INDEX IF NOT EXISTS dependency_edges_target_idx
            ON dependency_edges(target_path) WHERE target_path IS NOT NULL;
        CREATE INDEX IF NOT EXISTS dependency_edges_resolution_idx
            ON dependency_edges(resolution);
        CREATE VIRTUAL TABLE IF NOT EXISTS file_fts
            USING fts5(path, content, symbols);
        ",
    )?;
    conn.execute(
        "INSERT INTO metadata(key, value) VALUES ('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![INDEX_SCHEMA_VERSION],
    )?;
    Ok(())
}

fn write_index(conn: &mut Connection, root: &Path, files: &[IndexedFile]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM metadata", [])?;
    tx.execute("DELETE FROM files", [])?;
    tx.execute("DELETE FROM symbols", [])?;
    tx.execute("DELETE FROM imports", [])?;
    tx.execute("DELETE FROM dependency_edges", [])?;
    tx.execute("DELETE FROM file_fts", [])?;

    write_metadata(&tx, root)?;

    for file in files {
        insert_indexed_file(&tx, file)?;
    }
    recompute_dependency_edges(&tx)?;
    tx.commit()?;
    Ok(())
}

fn write_index_delta(
    conn: &mut Connection,
    root: &Path,
    upserts: &[IndexedFile],
    deleted: &[String],
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    write_metadata(&tx, root)?;
    for path in deleted {
        delete_indexed_file(&tx, path)?;
    }
    for file in upserts {
        delete_indexed_file(&tx, &file.path)?;
        insert_indexed_file(&tx, file)?;
    }
    recompute_dependency_edges(&tx)?;
    tx.commit()?;
    Ok(())
}

fn write_metadata(tx: &rusqlite::Transaction<'_>, root: &Path) -> Result<()> {
    let indexed_at = Utc::now().to_rfc3339();
    tx.execute("DELETE FROM metadata", [])?;
    tx.execute(
        "INSERT INTO metadata(key, value) VALUES
            ('repo_root', ?1), ('indexed_at', ?2), ('version', ?3), ('schema_version', ?4)",
        params![
            root.display().to_string(),
            indexed_at,
            env!("CARGO_PKG_VERSION"),
            INDEX_SCHEMA_VERSION
        ],
    )?;
    Ok(())
}

fn delete_indexed_file(tx: &rusqlite::Transaction<'_>, path: &str) -> Result<()> {
    tx.execute("DELETE FROM files WHERE path = ?1", params![path])?;
    tx.execute("DELETE FROM symbols WHERE file_path = ?1", params![path])?;
    tx.execute("DELETE FROM imports WHERE file_path = ?1", params![path])?;
    tx.execute("DELETE FROM file_fts WHERE path = ?1", params![path])?;
    Ok(())
}

fn insert_indexed_file(tx: &rusqlite::Transaction<'_>, file: &IndexedFile) -> Result<()> {
    tx.execute(
        "INSERT INTO files(path, lang, hash, content) VALUES (?1, ?2, ?3, ?4)",
        params![file.path, file.lang, file.hash, file.text],
    )?;
    let symbol_text = file
        .symbols
        .iter()
        .map(|s| format!("{} {} {}", s.kind, s.name, s.signature))
        .collect::<Vec<_>>()
        .join("\n");
    tx.execute(
        "INSERT INTO file_fts(path, content, symbols) VALUES (?1, ?2, ?3)",
        params![file.path, file.text, symbol_text],
    )?;
    for symbol in &file.symbols {
        tx.execute(
            "INSERT INTO symbols(file_path, name, kind, line, end_line, range_evidence, signature)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                file.path,
                symbol.name,
                symbol.kind,
                symbol.line as i64,
                symbol.end_line as i64,
                symbol.range_evidence,
                symbol.signature
            ],
        )?;
    }
    for import in &file.imports {
        tx.execute(
            "INSERT INTO imports(file_path, target, kind, line) VALUES (?1, ?2, ?3, ?4)",
            params![
                file.path,
                import.raw_target,
                import.kind.as_str(),
                import.line as i64
            ],
        )?;
    }
    Ok(())
}

fn collect_source_paths(root: &Path) -> Result<Vec<SourcePath>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let abs = entry.path().to_path_buf();
        let rel = relative_path(root, &abs)?;
        if should_skip(&rel) {
            continue;
        }
        let lang = language_for(&rel);
        if lang == "unknown" {
            continue;
        }
        files.push(SourcePath {
            path: rel,
            lang,
            abs,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

struct IndexProgress {
    enabled: bool,
    total: usize,
    current: usize,
    started: Instant,
    last_draw: Instant,
}

impl IndexProgress {
    fn new(root: &Path) -> Self {
        let enabled = io::stderr().is_terminal();
        let now = Instant::now();
        if enabled {
            eprintln!("Indexing {}", root.display());
        }
        Self {
            enabled,
            total: 0,
            current: 0,
            started: now,
            last_draw: now,
        }
    }

    fn phase(&mut self, label: &str) {
        if self.enabled {
            eprintln!("{label}...");
        }
    }

    fn set_total(&mut self, total: usize) {
        self.total = total;
        self.current = 0;
        self.last_draw = Instant::now();
        if self.enabled {
            self.draw(true);
        }
    }

    fn tick(&mut self) {
        self.current += 1;
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if self.current == self.total || now.duration_since(self.last_draw).as_millis() >= 120 {
            self.last_draw = now;
            self.draw(false);
        }
    }

    fn finish(&mut self, status: &Status) {
        if !self.enabled {
            return;
        }
        eprintln!(
            "Indexed {} files, {} symbols, {} imports in {:.1}s",
            status.files,
            status.symbols,
            status.imports,
            self.started.elapsed().as_secs_f64()
        );
    }

    fn draw(&self, force_newline: bool) {
        let width = 28;
        let filled = if self.total == 0 {
            0
        } else {
            self.current * width / self.total
        };
        let empty = width - filled;
        let percent = if self.total == 0 {
            0
        } else {
            self.current * 100 / self.total
        };
        eprint!(
            "\r[{done}{rest}] {current}/{total} files ({percent}%)",
            done = "#".repeat(filled),
            rest = ".".repeat(empty),
            current = self.current,
            total = self.total,
        );
        let _ = io::stderr().flush();
        if force_newline || self.current == self.total {
            eprintln!();
        }
    }
}

fn relative_path(root: &Path, abs: &Path) -> Result<String> {
    let rel = abs
        .strip_prefix(root)
        .with_context(|| format!("{} is not under {}", abs.display(), root.display()))?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

pub(crate) fn should_skip(rel: &str) -> bool {
    let lower_rel = rel.to_ascii_lowercase();
    let parts: Vec<&str> = rel.split('/').collect();
    parts.windows(2).any(|parts| {
        parts[0].eq_ignore_ascii_case(".claude") && parts[1].eq_ignore_ascii_case("worktrees")
    }) || parts.iter().any(|part| {
        let lower = part.to_ascii_lowercase();
        matches!(
            lower.as_str(),
            ".git"
                | ".kiv"
                | ".research"
                | "node_modules"
                | "target"
                | "dist"
                | "build"
                | "snapshots"
                | ".next"
                | ".venv"
                | "venv"
                | "env"
                | ".env"
                | "site-packages"
                | "__pypackages__"
                | "__pycache__"
                | ".pytest_cache"
                | ".mypy_cache"
                | ".ruff_cache"
                | ".tox"
                | ".nox"
                | "htmlcov"
                | "coverage"
                | ".parcel-cache"
                | ".turbo"
        )
    }) || parts.iter().any(|part| {
        let lower = part.to_ascii_lowercase();
        lower.starts_with(".venv-")
            || lower.starts_with(".venv_")
            || lower.starts_with("venv-")
            || lower.starts_with("venv_")
    }) || parts.first() == Some(&"personal")
        || lower_rel.ends_with(".lock")
        || lower_rel.ends_with("-lock.json")
        || lower_rel.ends_with("pnpm-lock.yaml")
        || lower_rel.ends_with("bun.lockb")
        || lower_rel.ends_with(".min.js")
        || lower_rel.ends_with(".min.css")
        || lower_rel.ends_with(".map")
        || lower_rel.ends_with(".png")
        || lower_rel.ends_with(".jpg")
        || lower_rel.ends_with(".jpeg")
        || lower_rel.ends_with(".gif")
        || lower_rel.ends_with(".webp")
        || lower_rel.ends_with(".svg")
        || lower_rel.ends_with(".pdf")
        || lower_rel.ends_with(".sqlite")
        || lower_rel.ends_with(".db")
}

fn language_for(path: &str) -> String {
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("rs") => "rust",
        Some("ts") | Some("tsx") => "typescript",
        Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => "javascript",
        Some("py") => "python",
        Some("go") => "go",
        Some("java") => "java",
        Some("rb") => "ruby",
        Some("php") => "php",
        Some("swift") => "swift",
        Some("kt") | Some("kts") => "kotlin",
        Some("c") | Some("h") => "c",
        Some("cc") | Some("cpp") | Some("hpp") => "cpp",
        Some("cs") => "csharp",
        Some("md") => "markdown",
        Some("toml") | Some("yaml") | Some("yml") | Some("json") => "config",
        _ => "unknown",
    }
    .to_string()
}

fn hash_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub(crate) fn extract_symbols(lang: &str, text: &str) -> Vec<Symbol> {
    extract_symbols_tree_sitter(lang, text).unwrap_or_else(|| extract_symbols_regex(lang, text))
}

fn extract_symbols_tree_sitter(lang: &str, text: &str) -> Option<Vec<Symbol>> {
    let tree = parse_tree(lang, text)?;
    let mut symbols = Vec::new();
    collect_symbol_nodes(tree.root_node(), text, &mut symbols);
    symbols.sort_by_key(|symbol| symbol.line);
    Some(symbols)
}

fn parse_tree(lang: &str, text: &str) -> Option<tree_sitter::Tree> {
    let language = tree_sitter_language(lang)?;
    let mut parser = TsParser::new();
    parser.set_language(&language).ok()?;
    parser.parse(text, None)
}

fn tree_sitter_language(lang: &str) -> Option<Language> {
    match lang {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        _ => None,
    }
}

fn collect_symbol_nodes(node: Node<'_>, text: &str, symbols: &mut Vec<Symbol>) {
    if let Some((kind, name_node, signature_node)) = symbol_from_node(node) {
        symbols.push(Symbol {
            name: node_text(name_node, text).to_string(),
            kind: kind.to_string(),
            line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            range_evidence: "exact-range".to_string(),
            signature: signature_for(signature_node, text),
        });
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_symbol_nodes(child, text, symbols);
    }
}

fn symbol_from_node<'a>(node: Node<'a>) -> Option<(&'static str, Node<'a>, Node<'a>)> {
    match node.kind() {
        "function_item" | "function_declaration" | "function_definition" | "method_definition" => {
            Some(("function", node.child_by_field_name("name")?, node))
        }
        "struct_item" => Some(("struct", node.child_by_field_name("name")?, node)),
        "enum_item" => Some(("enum", node.child_by_field_name("name")?, node)),
        "trait_item" => Some(("trait", node.child_by_field_name("name")?, node)),
        "impl_item" => impl_symbol(node),
        "class_declaration" | "class_definition" => {
            Some(("class", node.child_by_field_name("name")?, node))
        }
        "interface_declaration" => Some(("interface", node.child_by_field_name("name")?, node)),
        "type_alias_declaration" => Some(("type", node.child_by_field_name("name")?, node)),
        "variable_declarator" if has_function_value(node) => Some((
            "function",
            node.child_by_field_name("name")?,
            node.parent().unwrap_or(node),
        )),
        _ => None,
    }
}

fn impl_symbol<'a>(node: Node<'a>) -> Option<(&'static str, Node<'a>, Node<'a>)> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "type_identifier" | "generic_type" | "scoped_type_identifier"
        ) {
            return Some(("impl", child, node));
        }
    }
    None
}

fn has_function_value(node: Node<'_>) -> bool {
    node.child_by_field_name("value")
        .map(|value| matches!(value.kind(), "arrow_function" | "function_expression"))
        .unwrap_or(false)
}

fn node_text<'a>(node: Node<'_>, text: &'a str) -> &'a str {
    node.utf8_text(text.as_bytes()).unwrap_or("")
}

fn signature_for(node: Node<'_>, text: &str) -> String {
    text.lines()
        .nth(node.start_position().row)
        .unwrap_or("")
        .trim()
        .chars()
        .take(180)
        .collect()
}

fn extract_symbols_regex(lang: &str, text: &str) -> Vec<Symbol> {
    let patterns = match lang {
        "rust" => vec![
            (
                r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)",
                "function",
            ),
            (
                r"^\s*(?:pub\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)",
                "struct",
            ),
            (r"^\s*(?:pub\s+)?enum\s+([A-Za-z_][A-Za-z0-9_]*)", "enum"),
            (r"^\s*(?:pub\s+)?trait\s+([A-Za-z_][A-Za-z0-9_]*)", "trait"),
            (
                r"^\s*(?:pub\s+)?impl(?:\s+.*)?\s+for\s+([A-Za-z_][A-Za-z0-9_]*)",
                "impl",
            ),
        ],
        "typescript" | "javascript" => vec![
            (
                r"^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_$][A-Za-z0-9_$]*)",
                "function",
            ),
            (
                r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*(?:async\s*)?\(",
                "function",
            ),
            (
                r"^\s*(?:export\s+)?class\s+([A-Za-z_$][A-Za-z0-9_$]*)",
                "class",
            ),
            (
                r"^\s*(?:export\s+)?interface\s+([A-Za-z_$][A-Za-z0-9_$]*)",
                "interface",
            ),
            (
                r"^\s*(?:export\s+)?type\s+([A-Za-z_$][A-Za-z0-9_$]*)",
                "type",
            ),
        ],
        "python" => vec![
            (
                r"^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)",
                "function",
            ),
            (r"^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)", "class"),
        ],
        "go" => vec![
            (
                r"^\s*func\s+(?:\([^)]*\)\s*)?([A-Za-z_][A-Za-z0-9_]*)",
                "function",
            ),
            (r"^\s*type\s+([A-Za-z_][A-Za-z0-9_]*)\s+struct", "struct"),
            (
                r"^\s*type\s+([A-Za-z_][A-Za-z0-9_]*)\s+interface",
                "interface",
            ),
        ],
        "java" | "csharp" | "kotlin" | "swift" | "cpp" | "c" => vec![
            (
                r"^\s*(?:public|private|protected|internal|static|final|open|func|void|int|str|bool|class|struct|enum|interface|\w+)[\w\s:<>,*&\[\]]+\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(",
                "function",
            ),
            (
                r"^\s*(?:public\s+)?(?:class|struct|enum|interface)\s+([A-Za-z_][A-Za-z0-9_]*)",
                "type",
            ),
        ],
        "ruby" => vec![
            (r"^\s*def\s+([A-Za-z_][A-Za-z0-9_!?=]*)", "function"),
            (r"^\s*class\s+([A-Za-z_][A-Za-z0-9_:]*)", "class"),
            (r"^\s*module\s+([A-Za-z_][A-Za-z0-9_:]*)", "module"),
        ],
        _ => vec![],
    };

    let compiled: Vec<(Regex, &str)> = patterns
        .into_iter()
        .filter_map(|(pat, kind)| Regex::new(pat).ok().map(|re| (re, kind)))
        .collect();
    let mut symbols = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        for (re, kind) in &compiled {
            if let Some(caps) = re.captures(line)
                && let Some(name) = caps.get(1)
            {
                symbols.push(Symbol {
                    name: name.as_str().to_string(),
                    kind: (*kind).to_string(),
                    line: idx + 1,
                    end_line: idx + 1,
                    range_evidence: "approximate".to_string(),
                    signature: trimmed.chars().take(180).collect(),
                });
                break;
            }
        }
    }
    let final_line = text.lines().count().max(1);
    for index in 0..symbols.len() {
        symbols[index].end_line = symbols
            .get(index + 1)
            .map(|next| next.line.saturating_sub(1).max(symbols[index].line))
            .unwrap_or(final_line.max(symbols[index].line));
    }
    symbols
}

pub(crate) fn extract_imports(lang: &str, text: &str) -> Vec<String> {
    extract_import_refs(lang, text)
        .into_iter()
        .map(|import| import.raw_target)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn extract_import_refs(lang: &str, text: &str) -> Vec<ImportRef> {
    if lang == "python"
        && let Some(imports) = extract_import_refs_tree_sitter(lang, text)
    {
        return imports;
    }
    let mut imports = extract_import_refs_regex(lang, text);
    if let Some(ast_imports) = extract_import_refs_tree_sitter(lang, text) {
        imports.extend(ast_imports);
    }
    let imports = imports
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    imports
        .iter()
        .filter(|candidate| {
            !imports.iter().any(|other| {
                other.line == candidate.line
                    && other.kind == candidate.kind
                    && other.raw_target.len() > candidate.raw_target.len()
                    && other.raw_target.starts_with(&candidate.raw_target)
            })
        })
        .cloned()
        .collect()
}

fn extract_import_refs_tree_sitter(lang: &str, text: &str) -> Option<Vec<ImportRef>> {
    let tree = parse_tree(lang, text)?;
    let mut imports = BTreeSet::new();
    collect_import_nodes(lang, tree.root_node(), text, &mut imports);
    Some(imports.into_iter().collect())
}

fn collect_import_nodes(lang: &str, node: Node<'_>, text: &str, imports: &mut BTreeSet<ImportRef>) {
    let line = node.start_position().row + 1;
    match node.kind() {
        "use_declaration" => {
            let target = node_text(node, text)
                .trim()
                .strip_prefix("use ")
                .unwrap_or(node_text(node, text).trim())
                .trim_end_matches(';')
                .trim();
            if !target.is_empty() {
                imports.insert(ImportRef {
                    raw_target: target.to_string(),
                    kind: ImportKind::Use,
                    line,
                });
            }
        }
        "mod_item" => {
            if node.child_by_field_name("body").is_none()
                && let Some(name) = node.child_by_field_name("name")
            {
                imports.insert(ImportRef {
                    raw_target: node_text(name, text).to_string(),
                    kind: ImportKind::Module,
                    line,
                });
            }
        }
        "import_statement" => {
            if lang == "python" {
                collect_python_import_targets(node_text(node, text), line, imports);
            } else if let Some(source) = node.child_by_field_name("source") {
                imports.insert(ImportRef {
                    raw_target: strip_quotes(node_text(source, text)).to_string(),
                    kind: ImportKind::Import,
                    line,
                });
            } else {
                imports.insert(ImportRef {
                    raw_target: node_text(node, text).trim().to_string(),
                    kind: ImportKind::Import,
                    line,
                });
            }
        }
        "import_from_statement" => {
            let import_text = node_text(node, text);
            if import_text.trim_start().starts_with("from ") {
                if let Some(target) = python_from_import_target(import_text) {
                    imports.insert(ImportRef {
                        raw_target: target,
                        kind: ImportKind::Import,
                        line,
                    });
                }
            } else {
                imports.insert(ImportRef {
                    raw_target: import_text.trim().to_string(),
                    kind: ImportKind::Import,
                    line,
                });
            }
        }
        "call_expression" => {
            if is_require_call(node, text)
                && let Some(target) = first_string_argument(node, text)
            {
                imports.insert(ImportRef {
                    raw_target: target,
                    kind: ImportKind::Require,
                    line,
                });
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_import_nodes(lang, child, text, imports);
    }
}

fn is_require_call(node: Node<'_>, text: &str) -> bool {
    node.child_by_field_name("function")
        .map(|function| function.kind() == "identifier" && node_text(function, text) == "require")
        .unwrap_or(false)
}

fn first_string_argument(node: Node<'_>, text: &str) -> Option<String> {
    let args = node.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        if matches!(child.kind(), "string" | "string_fragment") {
            return Some(strip_quotes(node_text(child, text)).to_string());
        }
    }
    None
}

fn collect_python_import_targets(
    import_text: &str,
    line: usize,
    imports: &mut BTreeSet<ImportRef>,
) {
    let Some(rest) = import_text.trim().strip_prefix("import ") else {
        return;
    };
    for target in rest.split(',') {
        let target = target.trim().split(" as ").next().unwrap_or("").trim();
        if !target.is_empty() {
            imports.insert(ImportRef {
                raw_target: target.to_string(),
                kind: ImportKind::Import,
                line,
            });
        }
    }
}

fn python_from_import_target(import_text: &str) -> Option<String> {
    let rest = import_text.trim().strip_prefix("from ")?;
    let target = rest.split(" import ").next()?.trim();
    (!target.is_empty()).then(|| target.to_string())
}

fn strip_quotes(input: &str) -> &str {
    input.trim().trim_matches('"').trim_matches('\'')
}

fn extract_import_refs_regex(lang: &str, text: &str) -> Vec<ImportRef> {
    let patterns = match lang {
        "rust" => vec![
            (r"^\s*use\s+([^;]+)", ImportKind::Use),
            (
                r"^\s*mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;",
                ImportKind::Module,
            ),
        ],
        "typescript" | "javascript" => vec![
            (
                r#"^\s*import\s+.*?\s+from\s+['"]([^'"]+)['"]"#,
                ImportKind::Import,
            ),
            (r#"^\s*import\s+['"]([^'"]+)['"]"#, ImportKind::Import),
            (r#"require\(['"]([^'"]+)['"]\)"#, ImportKind::Require),
        ],
        "python" => vec![
            (r"^\s*import\s+([A-Za-z0-9_., \t]+)", ImportKind::Import),
            (r"^\s*from\s+([A-Za-z0-9_.]+)\s+import", ImportKind::Import),
        ],
        "go" => vec![(r#"^\s*"([^"]+)""#, ImportKind::Import)],
        _ => vec![],
    };
    let mut imports = BTreeSet::new();
    for (pat, kind) in patterns {
        if let Ok(re) = Regex::new(pat) {
            for (index, line_text) in text.lines().enumerate() {
                for caps in re.captures_iter(line_text) {
                    if let Some(target) = caps.get(1) {
                        let targets =
                            if lang == "python" && line_text.trim_start().starts_with("import ") {
                                target.as_str().split(',').collect::<Vec<_>>()
                            } else {
                                vec![target.as_str()]
                            };
                        for target in targets {
                            let target = target.trim().split(" as ").next().unwrap_or("").trim();
                            if !target.is_empty() {
                                imports.insert(ImportRef {
                                    raw_target: target.to_string(),
                                    kind,
                                    line: index + 1,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    imports.into_iter().collect()
}

fn load_status(conn: &Connection, root: &Path) -> Result<Status> {
    let indexed_at = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'indexed_at'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "unknown".to_string());
    let files = count_table(conn, "files")?;
    let symbols = count_table(conn, "symbols")?;
    let imports = count_table(conn, "imports")?;
    let graph_counts = |resolution: &str| -> Result<i64> {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM dependency_edges WHERE resolution = ?1",
            params![resolution],
            |row| row.get(0),
        )?)
    };
    Ok(Status {
        repo_root: root.display().to_string(),
        indexed_at,
        files,
        symbols,
        imports,
        exact_edges: graph_counts("exact")?,
        ambiguous_imports: graph_counts("ambiguous")?,
        unresolved_imports: graph_counts("unresolved")?,
    })
}

fn count_table(conn: &Connection, table: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    conn.query_row(&sql, [], |row| row.get(0))
        .map_err(Into::into)
}

#[derive(Deserialize)]
struct McpRequest {
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

fn run_mcp(root: &Path, config: &Config, auto_index: bool) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<McpRequest>(&line) {
            Ok(req) => handle_mcp_request(root, config, auto_index, req),
            Err(err) => json!({"id": null, "error": {"message": err.to_string()}}),
        };
        writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
        stdout.flush()?;
    }
    Ok(())
}

fn handle_mcp_request(root: &Path, config: &Config, auto_index: bool, req: McpRequest) -> Value {
    let id = req.id.unwrap_or(Value::Null);
    let result = match req.method.as_str() {
        "tools/list" => Ok(json!({
            "tools": [
                {"name": "index_status", "description": "Show terse Kiv Scout index status"},
                {"name": "get_skeleton", "description": "Return a bounded file skeleton"},
                {"name": "get_context_capsule", "description": "Rank files for a query; defaults to a bounded file list"},
                {"name": "get_change_impact", "description": "Return bounded exact dependency impact for a query or local git diff"},
                {"name": "check_architecture_boundaries", "description": "Check explicit repository layer deny rules over exact edges"}
            ]
        })),
        "tools/call" => mcp_tool_call(root, config, auto_index, &req.params),
        _ => Err(anyhow::anyhow!("unknown method {}", req.method)),
    };
    match result {
        Ok(value) => json!({"id": id, "result": value}),
        Err(err) => json!({"id": id, "error": {"message": err.to_string()}}),
    }
}

fn mcp_tool_call(root: &Path, config: &Config, auto_index: bool, params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("missing tool name")?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match name {
        "index_status" => {
            let conn = open_db_maybe_auto_index(root, auto_index)?;
            Ok(json!(load_status(&conn, root)?))
        }
        "get_skeleton" => {
            let file = args
                .get("file")
                .and_then(Value::as_str)
                .context("missing file")?;
            let detail = args
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("minimal");
            let path = root.join(file);
            let text = fs::read_to_string(&path)?;
            let lang = language_for(file);
            Ok(
                json!({"content": truncate_mcp_text(render_skeleton(file, &lang, &text, detail), 4000)}),
            )
        }
        "get_context_capsule" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .context("missing query")?;
            let mcp_max_tokens = config.mcp_max_tokens.unwrap_or(900).clamp(1, 32_000);
            let max_tokens = args
                .get("max_tokens")
                .and_then(Value::as_u64)
                .map(|value| value.clamp(1, mcp_max_tokens as u64) as usize)
                .unwrap_or_else(|| config.max_tokens.unwrap_or(500).min(mcp_max_tokens));
            let include_tests = args
                .get("include_tests")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| config.include_tests.unwrap_or(false));
            let mode = args
                .get("mode")
                .and_then(Value::as_str)
                .map(CapsuleMode::parse)
                .filter(|mode| *mode != CapsuleMode::Full)
                .unwrap_or(CapsuleMode::FilesOnly);
            let mcp_max_files = config.mcp_max_files.unwrap_or(10).clamp(1, 100);
            let max_files = args
                .get("max_files")
                .and_then(Value::as_u64)
                .map(|value| value.clamp(1, mcp_max_files as u64) as usize)
                .unwrap_or_else(|| config.max_files.unwrap_or(6).min(mcp_max_files));
            let conn = open_db_maybe_auto_index(root, auto_index)?;
            let content = capsule(&conn, query, max_tokens, include_tests, mode, max_files)?;
            Ok(json!({"content": truncate_mcp_text(content, 4000)}))
        }
        "get_change_impact" => {
            let query = args.get("query").and_then(Value::as_str);
            let diff_reference = args.get("diff").and_then(Value::as_str);
            if query.is_some() == diff_reference.is_some() {
                bail!("get_change_impact requires exactly one of query or diff");
            }
            let depth = args
                .get("depth")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 3) as usize;
            let max_files_cap = config.mcp_max_files.unwrap_or(10).clamp(1, 100);
            let max_files = args
                .get("max_files")
                .and_then(Value::as_u64)
                .unwrap_or(10)
                .clamp(1, max_files_cap as u64) as usize;
            let max_tokens_cap = config.mcp_max_tokens.unwrap_or(900).clamp(64, 32_000);
            let max_tokens = args
                .get("max_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(max_tokens_cap.min(900) as u64)
                .clamp(64, max_tokens_cap as u64) as usize;
            let include_tests = args
                .get("include_tests")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let options = default_impact_options(depth, max_files, max_tokens, include_tests);
            let conn = open_db_maybe_auto_index(root, auto_index)?;
            if let Some(reference) = diff_reference {
                let diff = read_git_diff(root, reference)?;
                let indexed_at = load_status(&conn, root)?.indexed_at;
                let symbol_ranges = load_symbol_ranges(&conn)?;
                let mut changed = map_changed_files(&diff, &symbol_ranges);
                let changed_total = changed.len();
                changed.truncate(max_files);
                let pivots = changed
                    .iter()
                    .filter(|file| symbol_ranges.contains_key(&file.path))
                    .map(|file| file.path.clone())
                    .collect::<Vec<_>>();
                let impact = impact_from_paths(&conn, &pivots, options)?;
                let content = diff_impact_markdown(
                    &changed,
                    changed_total,
                    &impact,
                    reference,
                    &indexed_at,
                    max_tokens,
                );
                Ok(json!({
                    "content": truncate_mcp_text(content, max_tokens.saturating_mul(4)),
                    "reference": reference,
                    "indexed_at": indexed_at,
                    "changed": changed,
                    "changed_total": changed_total,
                    "changed_truncated": changed_total > max_files,
                    "files": impact.files,
                    "unresolved": impact.unresolved,
                    "truncated": impact.truncated,
                    "omitted_files": impact.omitted_files,
                    "omitted_observations": impact.omitted_observations,
                }))
            } else {
                let query = query.context("missing query")?;
                let impact = impact_from_query(&conn, query, options)?;
                let content = render_impact_markdown(&impact, "Kiv Scout Impact", max_tokens);
                Ok(json!({
                    "content": truncate_mcp_text(content, max_tokens.saturating_mul(4)),
                    "files": impact.files,
                    "unresolved": impact.unresolved,
                    "truncated": impact.truncated,
                    "omitted_files": impact.omitted_files,
                    "omitted_observations": impact.omitted_observations,
                }))
            }
        }
        "check_architecture_boundaries" => {
            let path = args.get("path").and_then(Value::as_str);
            let max_violations = args
                .get("max_violations")
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .clamp(1, 100) as usize;
            let conn = open_db_maybe_auto_index(root, auto_index)?;
            let Some(mut result) = evaluate_boundaries(&conn, config.architecture.as_ref(), path)?
            else {
                return Ok(json!({
                    "content": "no architecture policy configured",
                    "violations": [],
                    "truncated": false,
                }));
            };
            let total_violations = result.violations.len();
            result.violations.truncate(max_violations);
            let content = result.render_markdown();
            Ok(json!({
                "content": truncate_mcp_text(content, 4000),
                "result": result,
                "total_violations": total_violations,
                "truncated": total_violations > max_violations,
            }))
        }
        _ => bail!("unknown tool {name}"),
    }
}

fn truncate_mcp_text(text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text;
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!(
        "{}\n\n[... {} more characters truncated by Kiv Scout MCP cap]",
        kept,
        text.chars().count().saturating_sub(max_chars)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    type SemanticEdge = (String, String, Option<String>, String, i64, String, String);

    fn indexed_file(path: &str, lang: &str, imports: Vec<ImportRef>) -> IndexedFile {
        IndexedFile {
            path: path.to_string(),
            lang: lang.to_string(),
            hash: format!("hash-{path}"),
            text: String::new(),
            symbols: Vec::new(),
            imports,
        }
    }

    fn import_ref(raw_target: &str, kind: ImportKind, line: usize) -> ImportRef {
        ImportRef {
            raw_target: raw_target.to_string(),
            kind,
            line,
        }
    }

    fn semantic_edges(conn: &Connection) -> Vec<SemanticEdge> {
        let mut stmt = conn
            .prepare(
                "SELECT source_path, raw_target, target_path, kind, line, resolution,
                        candidate_paths_json
                 FROM dependency_edges
                 ORDER BY source_path, line, kind, raw_target",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }

    #[test]
    fn extracts_typescript_symbols() {
        let text =
            "export function add(a: number, b: number) { return a + b; }\nexport class Box {}";
        let symbols = extract_symbols("typescript", text);
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "add");
        assert_eq!(symbols[1].kind, "class");
    }

    #[test]
    fn extracts_tree_sitter_symbol_shapes() {
        let rust = "impl Widget {\n    pub fn draw(&self) {}\n}\npub trait Render {}\n";
        let rust_symbols = extract_symbols("rust", rust);
        assert!(rust_symbols.iter().any(|s| s.kind == "impl"));
        assert!(
            rust_symbols
                .iter()
                .any(|s| s.kind == "function" && s.name == "draw")
        );
        assert!(
            rust_symbols
                .iter()
                .any(|s| s.kind == "trait" && s.name == "Render")
        );

        let python = "class Box:\n    def open(self):\n        pass\n";
        let python_symbols = extract_symbols("python", python);
        assert!(
            python_symbols
                .iter()
                .any(|s| s.kind == "class" && s.name == "Box")
        );
        assert!(
            python_symbols
                .iter()
                .any(|s| s.kind == "function" && s.name == "open")
        );
        let open = python_symbols.iter().find(|s| s.name == "open").unwrap();
        assert_eq!(open.line, 2);
        assert_eq!(open.end_line, 3);
        assert_eq!(open.range_evidence, "exact-range");
    }

    #[test]
    fn extracts_tree_sitter_imports() {
        let js = "import thing from './thing';\nconst fs = require('fs');\n";
        let imports = extract_imports("javascript", js);
        assert!(imports.iter().any(|item| item == "./thing"));
        assert!(imports.iter().any(|item| item == "fs"));

        let rust = "use std::fs;\nmod capsule;\n";
        let imports = extract_imports("rust", rust);
        assert!(imports.iter().any(|item| item == "std::fs"));
        assert!(imports.iter().any(|item| item == "capsule"));
    }

    #[test]
    fn import_evidence_preserves_kind_and_source_line() {
        let refs = extract_import_refs(
            "javascript",
            "import thing from './thing';\nconst fs = require('fs');\n",
        );
        assert!(refs.contains(&import_ref("./thing", ImportKind::Import, 1)));
        assert!(refs.contains(&import_ref("fs", ImportKind::Require, 2)));

        let refs = extract_import_refs("rust", "use crate::graph;\nmod capsule;\n");
        assert!(refs.contains(&import_ref("crate::graph", ImportKind::Use, 1)));
        assert!(refs.contains(&import_ref("capsule", ImportKind::Module, 2)));

        let refs = extract_import_refs(
            "rust",
            "use scout::{CapsuleMode, capsule};\nmod tests { use super::*; }\n",
        );
        assert_eq!(
            refs.iter()
                .filter(|item| item.line == 1 && item.kind == ImportKind::Use)
                .count(),
            1
        );
        assert!(
            !refs
                .iter()
                .any(|item| { item.kind == ImportKind::Module && item.raw_target == "tests" })
        );
    }

    #[test]
    fn graph_recomputes_when_only_targets_change() {
        let mut conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let source = indexed_file(
            "src/app.ts",
            "typescript",
            vec![import_ref("./thing", ImportKind::Import, 3)],
        );
        let direct = indexed_file("src/thing.ts", "typescript", Vec::new());
        write_index(&mut conn, Path::new("/repo"), &[source, direct]).unwrap();
        assert_eq!(
            graph::forward_dependencies(&conn, "src/app.ts").unwrap(),
            vec!["src/thing.ts"]
        );
        assert_eq!(
            graph::reverse_dependencies(&conn, "src/thing.ts").unwrap(),
            vec!["src/app.ts"]
        );

        let index_target = indexed_file("src/thing/index.ts", "typescript", Vec::new());
        write_index_delta(&mut conn, Path::new("/repo"), &[index_target], &[]).unwrap();
        let rows = semantic_edges(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].5, "ambiguous");
        assert_eq!(rows[0].6, r#"["src/thing.ts","src/thing/index.ts"]"#);
        assert!(
            graph::forward_dependencies(&conn, "src/app.ts")
                .unwrap()
                .is_empty()
        );

        write_index_delta(
            &mut conn,
            Path::new("/repo"),
            &[],
            &["src/thing.ts".to_string(), "src/thing/index.ts".to_string()],
        )
        .unwrap();
        let rows = semantic_edges(&conn);
        assert_eq!(rows[0].5, "unresolved");
        assert_eq!(rows[0].6, "[]");
    }

    #[test]
    fn graph_rows_are_deterministic_across_input_order() {
        let files = vec![
            indexed_file(
                "src/app.ts",
                "typescript",
                vec![
                    import_ref("react", ImportKind::Import, 1),
                    import_ref("./thing", ImportKind::Import, 2),
                ],
            ),
            indexed_file("src/thing/index.ts", "typescript", Vec::new()),
            indexed_file("src/thing.ts", "typescript", Vec::new()),
        ];
        let mut reversed = files.clone();
        reversed.reverse();
        let mut first = Connection::open_in_memory().unwrap();
        let mut second = Connection::open_in_memory().unwrap();
        init_schema(&first).unwrap();
        init_schema(&second).unwrap();
        write_index(&mut first, Path::new("/repo"), &files).unwrap();
        write_index(&mut second, Path::new("/repo"), &reversed).unwrap();
        assert_eq!(semantic_edges(&first), semantic_edges(&second));
    }

    #[test]
    fn incompatible_schema_is_actionable_and_full_index_can_rebuild() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO metadata(key, value) VALUES ('schema_version', '1');",
        )
        .unwrap();
        let error = init_schema(&conn).unwrap_err().to_string();
        assert!(error.contains("run `kiv-scout index` to rebuild"));
        init_schema_for_rebuild(&conn).unwrap();
        let version: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, INDEX_SCHEMA_VERSION);
    }

    #[test]
    fn capsule_mode_parses_compact_aliases() {
        assert_eq!(CapsuleMode::parse("compact"), CapsuleMode::Compact);
        assert_eq!(CapsuleMode::parse("files"), CapsuleMode::FilesOnly);
        assert_eq!(CapsuleMode::parse("unknown"), CapsuleMode::Full);
    }

    #[test]
    fn capsule_cap_presets_set_context_budgets() {
        let files = CapsuleCap::parse("files").unwrap();
        assert_eq!(files.mode, CapsuleMode::FilesOnly);
        assert_eq!(files.max_files, 20);

        let balanced = CapsuleCap::parse("balanced").unwrap();
        assert_eq!(balanced.mode, CapsuleMode::Compact);
        assert_eq!(balanced.max_tokens, 6000);

        let deep = CapsuleCap::parse("deep").unwrap();
        assert_eq!(deep.mode, CapsuleMode::Full);
        assert_eq!(deep.max_tokens, 16000);

        assert!(CapsuleCap::parse("not-a-cap").is_err());
    }

    #[test]
    fn render_skeleton_minimal_omits_signatures() {
        let text = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        let skeleton = render_skeleton("src/lib.rs", "rust", text, "minimal");
        assert!(skeleton.contains("`function` add"));
        assert!(!skeleton.contains("pub fn add"));
    }

    #[test]
    fn skips_generated_dirs() {
        assert!(should_skip("target/debug/app"));
        assert!(should_skip("node_modules/pkg/index.js"));
        assert!(should_skip(".claude/worktrees/worker-123/src/duplicate.rs"));
        assert!(should_skip(
            "packages/app/.claude/worktrees/worker-123/src/duplicate.rs"
        ));
        assert!(should_skip("venv/lib/python3.12/site-packages/pkg/mod.py"));
        assert!(should_skip("src/generated/bundle.min.js"));
        assert!(should_skip("package-lock.json"));
        assert!(should_skip("pnpm-lock.yaml"));
        assert!(should_skip("bun.lockb"));
        assert!(should_skip(".research/kiv-finetune/LATEST.md"));
        assert!(should_skip("personal/local-note.md"));
        assert!(!should_skip("src/main.rs"));
        assert!(!should_skip("README.md"));
    }

    #[test]
    fn repo_discovery_prunes_dependency_and_cache_dirs() {
        assert!(should_descend_for_repo_discovery_name("home", 0));
        assert!(!should_descend_for_repo_discovery_name("node_modules", 1));
        assert!(!should_descend_for_repo_discovery_name(".venv-local", 1));
        assert!(!should_descend_for_repo_discovery_name(".Trash", 1));
        assert!(should_descend_for_repo_discovery_name("codebase", 1));
    }

    #[test]
    fn extracts_python_imports_without_multiline_bleed() {
        let text = r#"
import copy
import heapq, inspect as inspect_mod
from enum import Enum
from package.submodule import (
    A,
    B,
)

def run():
    pass
"#;
        let imports = extract_imports("python", text);
        assert!(imports.contains(&"copy".to_string()));
        assert!(imports.contains(&"heapq".to_string()));
        assert!(imports.contains(&"inspect".to_string()));
        assert!(imports.contains(&"enum".to_string()));
        assert!(imports.contains(&"package.submodule".to_string()));
        assert!(!imports.iter().any(|import| import.contains('\n')));
    }

    #[test]
    fn parses_blank_config() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.default_repo.is_none());
    }

    #[test]
    fn parses_auto_index_config() {
        let config: Config = toml::from_str("auto_index = true").unwrap();
        assert_eq!(config.auto_index, Some(true));
    }
}
