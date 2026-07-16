# Kiv Scout

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Kiv Scout is a local-first codebase index and MCP context server for quickly navigating large repositories.

It indexes source files into a local SQLite database, extracts symbols and imports, then returns ranked file pointers, compact skeletons, or bounded context capsules for a query. It is designed for coding agents and developers who need a cheap first pass over a large repo before opening specific files.

## Features

- Local SQLite index in `.kiv/index.db`
- Fast ranked capsules using SQLite FTS5 with scan fallback
- File skeletons built from imports and symbols
- Tree-sitter extraction for Rust, TypeScript, JavaScript, and Python
- Evidence-bearing exact, ambiguous, and unresolved local dependency edges
- Bounded query- and diff-driven impact analysis with changed-symbol ranges
- Opt-in graph-expanded capsules and explicit architecture boundary policies
- Regex fallback for additional common languages
- Minimal JSON-lines MCP server over stdio
- Conservative MCP output limits for agent context control

## Install

Build from source:

```bash
git clone https://github.com/elimaine/kiv-scout.git
cd kiv-scout
./install.sh
```

The installer builds the release binary and installs `kiv-scout` into a user bin directory, preferring one already on your `PATH`. If it needs to use a directory that is not on `PATH`, it can add the PATH line to your shell startup file or print the exact command to run.

Then run:

```bash
kiv-scout index /path/to/repo
```

You can also build and run the checkout binary directly:

```bash
cargo build --release
export KIV_SCOUT="$PWD/target/release/kiv-scout"
$KIV_SCOUT index /path/to/repo
```

Manual Cargo install also works:

```bash
cargo install --path . --locked
```

If `kiv-scout` is not found after Cargo install, Cargo's bin directory is not on your shell `PATH`.
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

Kiv Scout skips common generated, package, and environment paths by default, including `.git`, `.kiv`, `node_modules`, `target`, build output directories, Python virtualenvs such as `.venv` and `venv`, `site-packages`, package lockfiles, source maps, and minified bundles. Nested Claude Code checkouts under `.claude/worktrees/` are also excluded so stale copies do not outrank files in the active repository.

To index all discovered user code repositories:

```bash
kiv-scout index all
```

`index all` scans your home directory for git repositories by looking for `.git` folders. It skips package, build, cache, virtualenv, and common non-code folders so dependency directories are not indexed as user code. To choose exact scan roots instead, set `KIV_SCOUT_INDEX_ROOTS` to a path-separated list.

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

Show exact forward dependencies, reverse blast radius, and likely tests from up
to three lexical pivots:

```bash
kiv-scout impact "auth token validation" --depth 2 --include-tests
kiv-scout impact "auth token validation" --format json
```

During review, use the local working-tree diff as the pivot source. This runs a
read-only `git diff --unified=0 --no-ext-diff <ref> --`; it does not fetch,
stage, checkout, or otherwise change Git state.

```bash
kiv-scout impact --diff origin/main --depth 2 --include-tests
```

Graph expansion for capsules is explicit. Without `--related`, capsule output
uses the existing lexical path unchanged.

```bash
kiv-scout capsule "auth token validation" --cap balanced \
  --related deps,rdeps,tests --related-depth 1
```

Benchmark local status, capsule, graph-impact, graph-expanded capsule, and skeleton paths:

```bash
kiv-scout bench --repo /path/to/repo
```

## Watcher

`kiv-scout index /path/to/repo` registers that repository in Kiv Scout's local watchlist. To keep watched repositories updated, start the foreground watcher:

```bash
kiv-scout watcher start
```

The watcher uses the external Unix `watch` command to run one incremental update pass on an interval. Each pass applies the same incremental updater used by `--auto-index`: new files are added, changed files are refreshed, and removed files are deleted from the DB. It writes updates to each repo's existing `.kiv/index.db`.

On Linux, `watch` is usually available through `procps` or `procps-ng`. On macOS, `watch` is not installed by default. If it is missing, `kiv-scout watcher start` explains the install command and, in an interactive terminal, asks before attempting to install it.

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
- `get_change_impact`
- `check_architecture_boundaries`

MCP responses are intentionally more conservative than CLI responses. By default, `get_context_capsule` rejects full mode, uses files-only output, clamps token and file counts, and truncates returned text.

`get_change_impact` accepts exactly one of `query` or `diff`, clamps graph depth
to three, and returns bounded Markdown plus structured file roles and evidence.
`check_architecture_boundaries` is read-only and returns success with no
violations when the repository has no explicit policy.

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

Generated indexes carry a schema version. A normal `kiv-scout index` rebuilds
an incompatible generated database; auto-index and watcher paths instead give
an actionable rebuild error rather than guessing at a migration.

## Dependency Resolution Contract

Kiv stores every observed import with its source path, raw target, kind, line,
resolver, and one of three states:

- `exact`: exactly one indexed repo-relative target; traversable by impact and policy checks.
- `ambiguous`: multiple sorted candidates; visible as evidence but never traversed.
- `unresolved`: external, unsupported, or otherwise unprovable; visible but never traversed.

The first supported slice is deliberately conservative:

- TypeScript/JavaScript: relative specifiers using exact files, supported source extensions, and `index` files.
- Python: explicit relative modules and absolute module paths that match exactly one indexed package root.
- Rust: external `mod`, `crate::`, `self::`, `super::`, and unprefixed paths only when an indexed local module proves the first segment. Bare wildcard prefixes fail closed.
- Other languages: raw observations remain unresolved.

Package exports, TypeScript aliases, environment-dependent Python namespaces,
external crates/packages, and function-call graphs are outside this contract.
Status reports exact, ambiguous, and unresolved counts so graph coverage is
visible rather than implied.

## Architecture Boundary Checks

Kiv has no built-in layer names or opinions. A repository can opt in with
explicit `globset` patterns and deny rules:

```toml
[architecture]
enabled = true

[[architecture.layers]]
name = "domain"
include = ["src/domain/**"]

[[architecture.layers]]
name = "infrastructure"
include = ["src/infrastructure/**"]

[[architecture.rules]]
from = "domain"
deny = ["infrastructure"]
```

Run the full policy or one file's outgoing edges:

```bash
kiv-scout check boundaries
kiv-scout check boundaries --path src/domain/order.ts --format json
```

Only exact edges can violate a rule. Unclassified files and unresolved imports
are counted but do not fail. Overlapping layer matches, malformed globs,
unknown rule references, and empty deny lists are configuration errors.
Configured violations return a non-zero exit status for CI; an absent or
disabled policy prints `no architecture policy configured` and succeeds.

## Graph Correctness Evaluation

The offline evaluator uses independent expected-edge JSON for Rust, Python,
TypeScript, and JavaScript. It checks exact, ambiguous, unresolved, target-only
incremental add/delete/rename behavior, per-language metrics, and one semantic
digest across twenty varied construction orders:

```bash
cargo test graph_eval -- --nocapture
```

The compact contract fixtures currently produce 100% exact precision/recall
and ambiguity accuracy in each supported language. A pinned, opt-in public
oracle for `fd`, `itsdangerous`, and `p-map` lives in `tests/corpus/graph`.
Follow its README to reproduce it.

That reviewed public sample is intentionally small. It does not establish the
broad 99% precision, 95% supported recall, 90% test-impact, or latency gates
required for enabling graph expansion by default. Impact and graph-expanded
capsules therefore remain advisory and opt-in.

## Limitations

- The index is local and explicit; run `kiv-scout index` after meaningful codebase changes, use `--auto-index`, or keep `kiv-scout watcher start` running to update the index incrementally.
- Tree-sitter extraction is currently strongest for Rust, TypeScript, JavaScript, and Python.
- Ranking is lexical and symbol-aware, not semantic.
- Exact dependency coverage is intentionally narrower than compiler/package-manager resolution; ambiguous and unsupported observations fail closed.
- Likely tests are opt-in and incomplete; Kiv does not claim to select every test affected by a change.
- MCP output is intentionally bounded and may omit useful context until you raise caps.
- Calling it too often in a long agent conversation can create context bloat; use clear triggers.

## License

Kiv Scout is licensed under the [MIT License](LICENSE).
