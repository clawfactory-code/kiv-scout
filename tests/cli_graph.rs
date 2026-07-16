use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
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
