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
git clone https://github.com/clawfactory-code/kiv-scout.git
cd kiv-scout
cargo build --release
```

Run from the checkout:

```bash
cargo run -- index /path/to/repo
```

Or install locally from the checkout:

```bash
cargo install --path .
```

## Quick Start

Index a repository:

```bash
kiv-scout index /path/to/repo
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

## Limitations

- The index is local and explicit; run `kiv-scout index` after meaningful repo changes.
- Tree-sitter extraction is currently strongest for Rust, TypeScript, JavaScript, and Python.
- Ranking is lexical and symbol-aware, not semantic.
- MCP output is intentionally bounded and may omit useful context until you raise caps.
- Calling it too often in a long agent conversation can create context bloat; use clear triggers.

## License

Kiv Scout is licensed under the [MIT License](LICENSE).
