# Offline dependency-resolution contract

Run the complete checked-fixture gate with:

```sh
cargo test graph_eval -- --nocapture
```

Every language has independently labeled exact, ambiguous, and unresolved observations. Resolver
output is compared with `expected-edges.json`; the evaluator does not generate or update labels.

| Language | Supported exact forms represented here |
|---|---|
| Rust | Local `mod`, `crate`, `self`, and `super` module paths when one indexed file matches. |
| Python | Relative modules and repository-absolute modules when one indexed file matches. |
| TypeScript | Relative file and `index` paths with supported TS/JS extensions. |
| JavaScript | Relative file and `index` paths with supported TS/JS extensions. |

Bare JavaScript/TypeScript packages, standard-library imports, missing targets, and collisions fail
closed as unresolved or ambiguous. The test command also runs 20 independently shuffled file/import
orders, target-only incremental equivalence checks, and metric edge-case tests.
