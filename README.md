# Kiv Scout

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Kiv Scout is a local-first codebase index and MCP context server for quickly navigating large repositories.

It indexes source files into a local SQLite database, extracts symbols and imports, then returns ranked file pointers, compact skeletons, or bounded context capsules for a query. It is designed for coding agents and developers who need a cheap first pass over a large repo before opening specific files.

## Features

- Local SQLite index in `.kiv/index.db`
- Fast ranked capsules using SQLite FTS5 with scan fallback
- File skeletons built from imports and symbols
- Tree-sitter extraction for Rust, TypeScript, JavaScript, and Python
- Regex fallback for additional common languages
- Minimal JSON-lines MCP server over stdio
- Conservative MCP output limits for agent context control

## Install

Build from source:

```bash
git clone https://github.com/elimaine/kiv-scout.git
cd kiv-scout
cargo build --release
```

Run the built binary from the checkout:

```bash
./target/release/kiv-scout index /path/to/repo
```

If you want to run the checkout binary after changing directories, keep an absolute command handy:

```bash
export KIV_SCOUT="$PWD/target/release/kiv-scout"
$KIV_SCOUT index /path/to/repo
```

Or install locally from the checkout:

```bash
cargo install --path . --locked
```

If `kiv-scout` is not found after install, Cargo's bin directory is not on your shell `PATH`.
Either add it:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

Or keep using the checkout binary:

```bash
$KIV_SCOUT status /path/to/repo
```

## Quick Start

Index a repository:

```bash
kiv-scout index /path/to/repo
```

Indexing writes a local SQLite database to `/path/to/repo/.kiv/index.db`. In an interactive terminal, the index command prints progress phases and a file-count progress bar to stderr.

Kiv Scout skips common generated, package, and environment paths by default, including `.git`, `.kiv`, `node_modules`, `target`, build output directories, Python virtualenvs such as `.venv` and `venv`, `site-packages`, package lockfiles, source maps, and minified bundles.

To index all discovered user code repositories:

```bash
kiv-scout index all
```

`index all` scans common code roots under your home directory, such as `~/code`, `~/src`, `~/dev`, `~/projects`, `~/repos`, `~/work`, and `~/Developer`. It discovers git repositories while skipping package, build, cache, and virtualenv folders. To choose exact scan roots, set `KIV_SCOUT_INDEX_ROOTS` to a path-separated list.

To remove generated Kiv Scout database files:

```bash
kiv-scout index remove /path/to/repo
kiv-scout index remove
kiv-scout index remove all
```

`index remove` removes `.kiv/index.db`, `.kiv/index.db-wal`, and `.kiv/index.db-shm` for one repo. `index remove all` removes those files from watched and discovered repos.

If you want query commands to build or update the index automatically, add `--auto-index`. A missing index is built once; an existing index is updated incrementally by adding new files, refreshing changed files, and removing deleted files.

```bash
kiv-scout --auto-index capsule "where is authentication checked?" --cap files
```

Check index status:

```bash
kiv-scout status /path/to/repo
```

Find likely files for a task:

```bash
cd /path/to/repo
kiv-scout capsule "where is authentication checked?" --cap files
```

Then ask for compact context when the file list looks plausible:

```bash
kiv-scout capsule "where is authentication checked?" --cap balanced
```

Inspect a large file before opening it fully:

```bash
kiv-scout skeleton src/main.rs --detail minimal
```

Benchmark the local index/status/capsule/skeleton paths:

```bash
kiv-scout bench --repo /path/to/repo
```

## Watcher

`kiv-scout index /path/to/repo` registers that repository in Kiv Scout's local watchlist. To keep watched repositories updated, start the foreground watcher:

```bash
kiv-scout watcher start
```

The watcher polls watched repositories and applies the same incremental updater used by `--auto-index`: new files are added, changed files are refreshed, and removed files are deleted from the DB. It writes updates to each repo's existing `.kiv/index.db`.

On macOS and Linux, Kiv Scout uses built-in polling. It does not require the external Unix `watch` command, so macOS users do not need to install GNU `watch` with Homebrew.

Useful watcher commands:

```bash
kiv-scout watcher list
kiv-scout watcher add /path/to/repo
kiv-scout watcher remove /path/to/repo
kiv-scout watcher start --interval-secs 10
```

The watchlist is stored under `$KIV_SCOUT_HOME/watchlist`, `$XDG_STATE_HOME/kiv-scout/watchlist`, or `~/.kiv-scout/watchlist`.

## Query Effectively

Kiv Scout supports natural-language-looking queries, but ranking is lexical rather than embedding-based. It searches indexed paths, source text, and extracted symbols with SQLite FTS5 plus fallback scanning. There is no embedding model, vector database, or semantic nearest-neighbor search.

Good queries use words likely to appear in code, comments, file names, docs, or symbols:

```bash
kiv-scout capsule "request routing middleware" --cap files
kiv-scout capsule "websocket message dispatch" --cap files
kiv-scout capsule "validate prompt graph before execution" --cap files
```

Prefer concrete domain terms over vague questions. `where is auth checked` is usually better than `how does security work`; `queue retry backoff` is better than `why are jobs slow`.

If the first pass is too broad, add more exact terms from the file list or symbols:

```bash
kiv-scout capsule "PromptExecutor validate_prompt" --cap balanced
kiv-scout capsule "CacheKeySetInputSignature cache invalidation" --cap balanced
```

Use `--cap files` first to get candidate paths cheaply, then switch to `--cap balanced` or `--cap deep` only after the file list looks plausible. For a known large file, use `skeleton` before opening full source.

## MCP Mode

Kiv Scout can run as a small MCP-style JSON-lines server over stdio:

```bash
kiv-scout mcp /path/to/repo
```

Example request:

```json
{"id":1,"method":"tools/call","params":{"name":"get_context_capsule","arguments":{"query":"where is request routing configured?"}}}
```

Available MCP tools:

- `index_status`
- `get_skeleton`
- `get_context_capsule`

MCP responses are intentionally more conservative than CLI responses. By default, `get_context_capsule` rejects full mode, uses files-only output, clamps token and file counts, and truncates returned text.

## Capsule Budgets

Capsule presets:

| Preset | Mode | Default Budget | Use |
|---|---:|---:|---|
| `files` | files-only | 20 files / 1200 tokens | First pass |
| `tight` | compact | 8 files / 2400 tokens | Small agent context |
| `balanced` | compact | 12 files / 6000 tokens | Normal agent context |
| `full` | full | 8 files / 8000 tokens | Human inspection |
| `wide` | files-only | 40 files / 3000 tokens | Broad repo scan |
| `deep` | full | 12 files / 16000 tokens | Larger source excerpts |

Override presets from the CLI:

```bash
kiv-scout capsule "where is auth checked?" --cap balanced --max-files 20 --max-tokens 12000
```

MCP chunk sizes are deliberately smaller. To change them, copy `kiv-scout.toml.example` to `kiv-scout.toml` and adjust:

```toml
mcp_max_tokens = 2000
mcp_max_files = 20
max_tokens = 6000
max_files = 12
```

You can also pass a config explicitly:

```bash
kiv-scout --config ./kiv-scout.toml capsule "where is billing handled?"
```

## Agent Usage Guidance

Kiv Scout works best as a source-pointer layer, not as a tool to call on every message.

Good triggers:

- Starting work in an unfamiliar large repo
- Looking for the files behind a specific feature or bug
- Deciding which large file to inspect first
- Re-orienting after the task changes meaningfully

Poor triggers:

- Every agent turn
- Tiny repos where direct `rg` is faster
- Questions that already name the exact file and function
- Long conversations where repeated capsules would add context bloat

Current retrieved chunks are conservative on purpose. If your agent has a larger context window or better pruning, increase `mcp_max_tokens`, `mcp_max_files`, `max_tokens`, or `max_files` in `kiv-scout.toml`, or pass `--max-tokens` and `--max-files` to the CLI.

## Configuration

Kiv Scout looks for `kiv-scout.toml` in the current directory, or a path passed with `--config`, or the `KIV_SCOUT_CONFIG` environment variable.

The repository ships `kiv-scout.toml.example` as a blank template. Keep machine-specific paths in your local `kiv-scout.toml`; that file is ignored by git.

Set `auto_index = true` to make `status`, `capsule`, and MCP context calls build or incrementally update `.kiv/index.db` automatically when needed. Auto-index compares the current source file list with the DB, hashes new or modified files, and removes deleted files. Very large repositories may still do extra filesystem scanning before each indexed query.

## Limitations

- The index is local and explicit; run `kiv-scout index` after meaningful repo changes.
- Tree-sitter extraction is currently strongest for Rust, TypeScript, JavaScript, and Python.
- Ranking is lexical and symbol-aware, not semantic.
- MCP output is intentionally bounded and may omit useful context until you raise caps.
- Calling it too often in a long agent conversation can create context bloat; use clear triggers.

## License

Kiv Scout is licensed under the [MIT License](LICENSE).
