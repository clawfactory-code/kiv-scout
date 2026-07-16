# Public graph corpus

This opt-in layer measures Kiv Scout against independently labeled observations from public,
immutable commits. The compact contract fixtures remain the offline release gate; this corpus is
the evidence required before graph-backed expansion can become a default.

From any scratch directory, prepare the exact checkouts without writing paths into this repo:

```sh
mkdir -p kiv-graph-corpus
git clone https://github.com/sharkdp/fd.git kiv-graph-corpus/fd
git -C kiv-graph-corpus/fd checkout 1bfeea237a48c9545211e5c21d623d398fa712c6
git clone https://github.com/pallets/itsdangerous.git kiv-graph-corpus/itsdangerous
git -C kiv-graph-corpus/itsdangerous checkout 672971d66a2ef9f85151e53283113f33d642dabd
git clone https://github.com/sindresorhus/p-map.git kiv-graph-corpus/p-map
git -C kiv-graph-corpus/p-map checkout 3ada5f36632aca8df860c376856270b6d2ba2de8
```

Run the ignored evaluator from the Kiv Scout checkout:

```sh
KIV_GRAPH_CORPUS_ROOT=/path/to/kiv-graph-corpus \
  cargo test graph_eval::public_corpus_matches_pinned_oracles -- --ignored --nocapture
```

The evaluator verifies each checkout's exact Git commit before reading it, builds its graph in an
in-memory database, and scores only rows listed in the oracle JSON. Add labels by code review; do
not derive an oracle from resolver output. Paths in metadata are repository-relative.

Current limitation: the oracle is intentionally small and does not yet establish the 99% precision,
95% supported recall, or 90% test-impact release gates for enabling graph expansion by default.
Those defaults must remain off until a sufficiently broad reviewed oracle and latency baseline are
checked in.
