use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_kiv-scout")
}

fn temporary_repo(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("kiv-cli-{label}-{}-{nonce}", std::process::id()));
    fs::create_dir_all(root.join("src/domain")).unwrap();
    fs::create_dir_all(root.join("src/infra")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(
        root.join("src/domain/order.ts"),
        "import { db } from '../infra/db';\nexport function createOrder() { return db; }\n",
    )
    .unwrap();
    fs::write(root.join("src/infra/db.ts"), "export const db = 1;\n").unwrap();
    fs::write(
        root.join("tests/order.test.ts"),
        "import { createOrder } from '../src/domain/order';\ntest('order', createOrder);\n",
    )
    .unwrap();
    root
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .current_dir(root)
        .env("KIV_SCOUT_HOME", root.join(".kiv-scout-state"))
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn mcp_call(root: &Path, request: Value) -> Value {
    let mut child = Command::new(binary())
        .args(["mcp", root.to_str().unwrap()])
        .current_dir(root)
        .env("KIV_SCOUT_HOME", root.join(".kiv-scout-state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.as_mut().unwrap(), "{request}").unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", stdout(&output));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_impact_capsule_and_policy_use_the_same_exact_graph() {
    let root = temporary_repo("graph-surfaces");
    let indexed = run(&root, &["index", root.to_str().unwrap()]);
    assert!(indexed.status.success(), "{}", stdout(&indexed));

    let impact = run(
        &root,
        &[
            "impact",
            "createOrder domain order",
            "--depth",
            "2",
            "--include-tests",
            "--format",
            "json",
        ],
    );
    assert!(impact.status.success());
    let impact: Value = serde_json::from_slice(&impact.stdout).unwrap();
    let files = impact["files"].as_array().unwrap();
    assert!(
        files
            .iter()
            .any(|item| item["path"] == "src/domain/order.ts")
    );
    assert!(files.iter().any(|item| item["path"] == "src/infra/db.ts"));
    assert!(
        files
            .iter()
            .any(|item| item["path"] == "tests/order.test.ts")
    );

    let plain = run(
        &root,
        &["capsule", "createOrder domain order", "--cap", "files"],
    );
    assert!(plain.status.success());
    assert!(!stdout(&plain).contains("roles:"));
    let expanded = run(
        &root,
        &[
            "capsule",
            "createOrder domain order",
            "--cap",
            "files",
            "--related",
            "deps,rdeps,tests",
            "--related-depth",
            "2",
        ],
    );
    assert!(expanded.status.success());
    assert!(stdout(&expanded).contains("roles:"));

    fs::write(
        root.join("kiv-scout.toml"),
        r#"[architecture]
enabled = true

[[architecture.layers]]
name = "domain"
include = ["src/domain/**"]

[[architecture.layers]]
name = "infra"
include = ["src/infra/**"]

[[architecture.rules]]
from = "domain"
deny = ["infra"]
"#,
    )
    .unwrap();
    let policy = run(&root, &["check", "boundaries", "--format", "json"]);
    assert!(!policy.status.success());
    let policy: Value = serde_json::from_slice(&policy.stdout).unwrap();
    assert_eq!(policy["violations"].as_array().unwrap().len(), 1);
    assert_eq!(policy["violations"][0]["source"], "src/domain/order.ts");
    assert_eq!(policy["violations"][0]["target"], "src/infra/db.ts");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn absent_policy_is_a_successful_no_op() {
    let root = temporary_repo("no-policy");
    assert!(
        run(&root, &["index", root.to_str().unwrap()])
            .status
            .success()
    );
    let check = run(&root, &["check", "boundaries"]);
    assert!(check.status.success());
    assert_eq!(stdout(&check).trim(), "no architecture policy configured");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn mcp_defaults_return_model_sized_context_and_complete_skeletons() {
    let root = temporary_repo("mcp-context-limits");
    for index in 0..24 {
        fs::write(
            root.join(format!("src/domain/context_{index:02}.ts")),
            format!("export function whole_context_{index:02}() {{ return {index}; }}\n"),
        )
        .unwrap();
    }
    let large_source = (0..150)
        .map(|index| format!("export function function_{index:03}() {{ return {index}; }}\n"))
        .collect::<String>();
    fs::write(root.join("src/domain/large.ts"), large_source).unwrap();
    assert!(
        run(&root, &["index", root.to_str().unwrap()])
            .status
            .success()
    );

    let compact = mcp_call(
        &root,
        serde_json::json!({
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "get_context_capsule",
                "arguments": {"query": "whole_context"}
            }
        }),
    );
    assert!(
        compact["result"]["repo_fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(
        compact["result"]["index_fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(compact["result"]["pointers"].as_array().unwrap().len(), 24);
    assert_eq!(
        compact["result"]["pointers"][0]["source"],
        "kiv_scout_lexical_index"
    );
    let compact = compact["result"]["content"].as_str().unwrap();
    assert!(compact.contains("**Mode:** compact"));
    assert_eq!(
        compact
            .lines()
            .filter(|line| line.starts_with("### src/domain/context_"))
            .count(),
        24
    );

    let full = mcp_call(
        &root,
        serde_json::json!({
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "get_context_capsule",
                "arguments": {
                    "query": "whole_context",
                    "mode": "full",
                    "max_tokens": 24000,
                    "max_files": 20
                }
            }
        }),
    );
    assert!(
        full["result"]["content"]
            .as_str()
            .unwrap()
            .contains("**Mode:** full")
    );

    let skeleton = mcp_call(
        &root,
        serde_json::json!({
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "get_skeleton",
                "arguments": {"file": "src/domain/large.ts", "detail": "standard"}
            }
        }),
    );
    let skeleton = skeleton["result"]["content"].as_str().unwrap();
    assert!(skeleton.contains("function_149"));
    assert!(!skeleton.contains("truncated by Kiv Scout MCP cap"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn json_capsules_are_pointer_only_and_fingerprinted() {
    let root = temporary_repo("pointer-contract");
    assert!(
        run(&root, &["index", root.to_str().unwrap()])
            .status
            .success()
    );
    let before_status: Value =
        serde_json::from_slice(&run(&root, &["status", "."]).stdout).unwrap();
    let capsule = run(
        &root,
        &[
            "capsule",
            "createOrder domain order",
            "--cap",
            "files",
            "--format",
            "json",
        ],
    );
    assert!(capsule.status.success(), "{}", stdout(&capsule));
    let capsule: Value = serde_json::from_slice(&capsule.stdout).unwrap();
    assert_eq!(capsule["schema_version"], 1);
    assert_eq!(
        capsule["repo_fingerprint"],
        before_status["repo_fingerprint"]
    );
    assert_eq!(
        capsule["index_fingerprint"],
        before_status["index_fingerprint"]
    );
    assert_eq!(capsule["pointers"][0]["path"], "src/domain/order.ts");
    assert_eq!(capsule["pointers"][0]["confidence"], "advisory");
    assert_eq!(
        capsule["pointers"][0]["reason"],
        "lexical path, source, or symbol match"
    );
    let serialized = serde_json::to_string(&capsule).unwrap();
    assert!(!serialized.contains("return db"));

    fs::write(
        root.join("src/domain/order.ts"),
        "import { db } from '../infra/db';\nexport function createOrder() { return db + 1; }\n",
    )
    .unwrap();
    let after_status = run(&root, &["--auto-index", "status", "."]);
    assert!(after_status.status.success());
    let after_status: Value = serde_json::from_slice(&after_status.stdout).unwrap();
    assert_eq!(
        before_status["repo_fingerprint"],
        after_status["repo_fingerprint"]
    );
    assert_ne!(
        before_status["index_fingerprint"],
        after_status["index_fingerprint"]
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn watcher_prunes_missing_repos_and_rebuilds_legacy_indexes() {
    let root = temporary_repo("watcher-recovery");
    assert!(
        run(&root, &["index", root.to_str().unwrap()])
            .status
            .success()
    );
    let watchlist = root.join(".kiv-scout-state/watchlist");
    let missing = root.join("removed-worktree");
    let mut entries = fs::read_to_string(&watchlist).unwrap();
    entries.push_str(&format!("{}\n", missing.display()));
    fs::write(&watchlist, entries).unwrap();

    let conn = Connection::open(root.join(".kiv/index.db")).unwrap();
    conn.execute(
        "UPDATE metadata SET value = '1' WHERE key = 'schema_version'",
        [],
    )
    .unwrap();
    drop(conn);

    let pass = run(&root, &["watcher", "start", "--once"]);
    assert!(
        pass.status.success(),
        "{}",
        String::from_utf8_lossy(&pass.stderr)
    );
    let stderr = String::from_utf8(pass.stderr).unwrap();
    assert!(stderr.contains("Pruning missing watched repo"));
    assert!(stderr.contains("Rebuilt"));
    assert!(
        !fs::read_to_string(&watchlist)
            .unwrap()
            .contains("removed-worktree")
    );

    let status = run(&root, &["status", "."]);
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["files"], 3);

    fs::remove_dir_all(root).unwrap();
}
