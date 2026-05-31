use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Parser, Subcommand};
use regex::Regex;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use tree_sitter::{Language, Node, Parser as TsParser};
use walkdir::WalkDir;

mod scout;

use scout::{CapsuleCap, CapsuleMode, capsule, render_skeleton};

#[derive(Parser)]
#[command(name = "kiv-scout")]
#[command(about = "Local codebase index, skeleton, capsule, and MCP context server")]
struct Cli {
    /// Optional TOML config file. Defaults to ./kiv-scout.toml when present.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Index a repository into .kiv/index.db.
    Index {
        /// Repository root. Defaults to current directory or config default_repo.
        dir: Option<PathBuf>,
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
    },
    /// Scout benchmark workflows.
    Scout {
        #[command(subcommand)]
        command: scout::ScoutCommands,
    },
    /// Run a minimal MCP-compatible JSON-lines server over stdio.
    Mcp {
        /// Repository root. Defaults to current directory or config default_repo.
        dir: Option<PathBuf>,
    },
}

#[derive(Debug, Clone)]
struct SourceFile {
    path: String,
    hash: String,
    lang: String,
    text: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Symbol {
    name: String,
    kind: String,
    line: usize,
    signature: String,
}

#[derive(Debug, Clone)]
struct IndexedFile {
    path: String,
    lang: String,
    hash: String,
    text: String,
    symbols: Vec<Symbol>,
    imports: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Status {
    repo_root: String,
    indexed_at: String,
    files: i64,
    symbols: i64,
    imports: i64,
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
    /// Max token budget accepted by the MCP get_context_capsule tool.
    mcp_max_tokens: Option<usize>,
    /// Max file count accepted by the MCP get_context_capsule tool.
    mcp_max_files: Option<usize>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = load_config(cli.config.as_deref())?;
    match cli.command {
        Commands::Index { dir } => {
            let root = repo_root(dir.or_else(|| config.default_repo.clone()))?;
            let status = index_repo(&root)?;
            println!(
                "Indexed {} files, {} symbols, {} imports into {}",
                status.files,
                status.symbols,
                status.imports,
                db_path(&root).display()
            );
        }
        Commands::Status { dir } => {
            let root = repo_root(dir.or_else(|| config.default_repo.clone()))?;
            let conn = open_db(&root)?;
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
        } => {
            let root = repo_root(config.default_repo.clone())?;
            let conn = open_db(&root)?;
            let cap_name = cap
                .or_else(|| config.default_capsule.clone())
                .unwrap_or_else(|| "default".to_string());
            let cap = CapsuleCap::parse(&cap_name)?;
            let mode = mode.as_deref().map(CapsuleMode::parse).unwrap_or(cap.mode);
            let max_tokens = max_tokens.or(config.max_tokens).unwrap_or(cap.max_tokens);
            let max_files = max_files.or(config.max_files).unwrap_or(cap.max_files);
            let include_tests = include_tests || config.include_tests.unwrap_or(false);
            print!(
                "{}",
                capsule(&conn, &query, max_tokens, include_tests, mode, max_files)?
            );
        }
        Commands::Scout { command } => scout::handle_scout_command(command)?,
        Commands::Mcp { dir } => {
            let root = repo_root(dir.or_else(|| config.default_repo.clone()))?;
            run_mcp(&root, &config)?;
        }
    }
    Ok(())
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

fn db_path(root: &Path) -> PathBuf {
    root.join(".kiv").join("index.db")
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

fn index_repo(root: &Path) -> Result<Status> {
    fs::create_dir_all(root.join(".kiv"))?;
    let conn = Connection::open(db_path(root))?;
    init_schema(&conn)?;
    let files = collect_source_files(root)?;
    let mut indexed = Vec::with_capacity(files.len());
    for file in files {
        let symbols = extract_symbols(&file.lang, &file.text);
        let imports = extract_imports(&file.lang, &file.text);
        indexed.push(IndexedFile {
            path: file.path,
            lang: file.lang,
            hash: file.hash,
            text: file.text,
            symbols,
            imports,
        });
    }
    write_index(&conn, root, &indexed)?;
    load_status(&conn, root)
}

fn init_schema(conn: &Connection) -> Result<()> {
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
            signature TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS imports (
            id INTEGER PRIMARY KEY,
            file_path TEXT NOT NULL,
            target TEXT NOT NULL
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS file_fts
            USING fts5(path, content, symbols);
        ",
    )?;
    Ok(())
}

fn write_index(conn: &Connection, root: &Path, files: &[IndexedFile]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM metadata", [])?;
    tx.execute("DELETE FROM files", [])?;
    tx.execute("DELETE FROM symbols", [])?;
    tx.execute("DELETE FROM imports", [])?;
    tx.execute("DELETE FROM file_fts", [])?;

    let indexed_at = Utc::now().to_rfc3339();
    tx.execute(
        "INSERT INTO metadata(key, value) VALUES ('repo_root', ?1), ('indexed_at', ?2), ('version', ?3)",
        params![root.display().to_string(), indexed_at, env!("CARGO_PKG_VERSION")],
    )?;

    for file in files {
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
                "INSERT INTO symbols(file_path, name, kind, line, signature) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![file.path, symbol.name, symbol.kind, symbol.line as i64, symbol.signature],
            )?;
        }
        for import in &file.imports {
            tx.execute(
                "INSERT INTO imports(file_path, target) VALUES (?1, ?2)",
                params![file.path, import],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn collect_source_files(root: &Path) -> Result<Vec<SourceFile>> {
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
        let text = match fs::read_to_string(&abs) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let hash = hash_text(&text);
        files.push(SourceFile {
            path: rel,
            hash,
            lang,
            text,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn relative_path(root: &Path, abs: &Path) -> Result<String> {
    let rel = abs
        .strip_prefix(root)
        .with_context(|| format!("{} is not under {}", abs.display(), root.display()))?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

fn should_skip(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    parts.iter().any(|part| {
        matches!(
            *part,
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
                | "__pycache__"
        )
    }) || parts.first() == Some(&"personal")
        || rel.ends_with(".lock")
        || rel.ends_with(".png")
        || rel.ends_with(".jpg")
        || rel.ends_with(".jpeg")
        || rel.ends_with(".gif")
        || rel.ends_with(".pdf")
        || rel.ends_with(".sqlite")
        || rel.ends_with(".db")
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
                    signature: trimmed.chars().take(180).collect(),
                });
                break;
            }
        }
    }
    symbols
}

pub(crate) fn extract_imports(lang: &str, text: &str) -> Vec<String> {
    let mut imports = extract_imports_regex(lang, text);
    if let Some(ast_imports) = extract_imports_tree_sitter(lang, text) {
        imports.extend(ast_imports);
        imports.sort();
        imports.dedup();
    }
    imports
}

fn extract_imports_tree_sitter(lang: &str, text: &str) -> Option<Vec<String>> {
    let tree = parse_tree(lang, text)?;
    let mut imports = BTreeSet::new();
    collect_import_nodes(tree.root_node(), text, &mut imports);
    Some(imports.into_iter().collect())
}

fn collect_import_nodes(node: Node<'_>, text: &str, imports: &mut BTreeSet<String>) {
    match node.kind() {
        "use_declaration" => {
            let target = node_text(node, text)
                .trim()
                .strip_prefix("use ")
                .unwrap_or(node_text(node, text).trim())
                .trim_end_matches(';')
                .trim();
            if !target.is_empty() {
                imports.insert(target.to_string());
            }
        }
        "mod_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                imports.insert(node_text(name, text).to_string());
            }
        }
        "import_statement" => {
            if let Some(source) = node.child_by_field_name("source") {
                imports.insert(strip_quotes(node_text(source, text)).to_string());
            } else {
                imports.insert(node_text(node, text).trim().to_string());
            }
        }
        "import_from_statement" => {
            imports.insert(node_text(node, text).trim().to_string());
        }
        "call_expression" => {
            if is_require_call(node, text)
                && let Some(target) = first_string_argument(node, text)
            {
                imports.insert(target);
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_import_nodes(child, text, imports);
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

fn strip_quotes(input: &str) -> &str {
    input.trim().trim_matches('"').trim_matches('\'')
}

fn extract_imports_regex(lang: &str, text: &str) -> Vec<String> {
    let patterns = match lang {
        "rust" => vec![r"^\s*use\s+([^;]+)", r"^\s*mod\s+([A-Za-z_][A-Za-z0-9_]*)"],
        "typescript" | "javascript" => vec![
            r#"^\s*import\s+.*?\s+from\s+['"]([^'"]+)['"]"#,
            r#"^\s*import\s+['"]([^'"]+)['"]"#,
            r#"require\(['"]([^'"]+)['"]\)"#,
        ],
        "python" => vec![
            r"^\s*import\s+([A-Za-z0-9_.,\s]+)",
            r"^\s*from\s+([A-Za-z0-9_.]+)\s+import",
        ],
        "go" => vec![r#"^\s*"([^"]+)""#],
        _ => vec![],
    };
    let mut imports = BTreeSet::new();
    for pat in patterns {
        if let Ok(re) = Regex::new(pat) {
            for caps in re.captures_iter(text) {
                if let Some(target) = caps.get(1) {
                    imports.insert(target.as_str().trim().to_string());
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
    Ok(Status {
        repo_root: root.display().to_string(),
        indexed_at,
        files,
        symbols,
        imports,
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

fn run_mcp(root: &Path, config: &Config) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<McpRequest>(&line) {
            Ok(req) => handle_mcp_request(root, config, req),
            Err(err) => json!({"id": null, "error": {"message": err.to_string()}}),
        };
        writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
        stdout.flush()?;
    }
    Ok(())
}

fn handle_mcp_request(root: &Path, config: &Config, req: McpRequest) -> Value {
    let id = req.id.unwrap_or(Value::Null);
    let result = match req.method.as_str() {
        "tools/list" => Ok(json!({
            "tools": [
                {"name": "index_status", "description": "Show terse Kiv Scout index status"},
                {"name": "get_skeleton", "description": "Return a bounded file skeleton"},
                {"name": "get_context_capsule", "description": "Rank files for a query; defaults to a bounded file list"}
            ]
        })),
        "tools/call" => mcp_tool_call(root, config, &req.params),
        _ => Err(anyhow::anyhow!("unknown method {}", req.method)),
    };
    match result {
        Ok(value) => json!({"id": id, "result": value}),
        Err(err) => json!({"id": id, "error": {"message": err.to_string()}}),
    }
}

fn mcp_tool_call(root: &Path, config: &Config, params: &Value) -> Result<Value> {
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
            let conn = open_db(root)?;
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
            let conn = open_db(root)?;
            let content = capsule(&conn, query, max_tokens, include_tests, mode, max_files)?;
            Ok(json!({"content": truncate_mcp_text(content, 4000)}))
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
        assert!(should_skip(".research/kiv-finetune/LATEST.md"));
        assert!(should_skip("personal/wayward-kiv-install.md"));
        assert!(!should_skip("src/main.rs"));
        assert!(!should_skip("README.md"));
    }

    #[test]
    fn parses_blank_config() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.default_repo.is_none());
    }
}
