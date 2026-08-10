use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn binary() -> PathBuf {
    std::env::var_os("KIV_FAST_MOVING_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_kiv-scout")))
}

fn temp_repo() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("kiv-fast-moving-{}-{nonce}", std::process::id()));
    fs::create_dir_all(root.join("src/consumers")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(
        root.join("src/shared.ts"),
        "export function sharedValue() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/feature.ts"),
        "import { sharedValue } from './shared';\nexport function feature() { return sharedValue(); }\n",
    )
    .unwrap();
    for index in 0..80 {
        fs::write(
            root.join(format!("src/consumers/consumer_{index:02}.ts")),
            "import { feature } from '../feature';\nexport const value = feature();\n",
        )
        .unwrap();
    }
    for index in 0..20 {
        fs::write(
            root.join(format!("tests/consumer_{index:02}.test.ts")),
            format!(
                "import {{ value }} from '../src/consumers/consumer_{index:02}';\ntest('value', () => value);\n"
            ),
        )
        .unwrap();
    }
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "kiv@example.test"]);
    git(&root, &["config", "user.name", "Kiv Test"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "baseline"]);
    root
}

fn command(root: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .current_dir(root)
        .env("KIV_SCOUT_HOME", root.join(".kiv-scout-state"))
        .output()
        .unwrap()
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_status(root: &Path) -> Vec<u8> {
    Command::new("git")
        .args(["status", "--porcelain=v1"])
        .current_dir(root)
        .output()
        .unwrap()
        .stdout
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn incremental_graph_and_diff_impact_survive_rapid_target_changes() {
    let root = temp_repo();
    let full_started = Instant::now();
    let indexed = command(&root, &["index", root.to_str().unwrap()]);
    let full_ms = full_started.elapsed().as_secs_f64() * 1_000.0;
    assert!(indexed.status.success());
    let initial = json(&command(&root, &["status", "."]));
    assert_eq!(initial["files"], 102);
    assert_eq!(initial["exact_edges"], 101);

    fs::write(
        root.join("src/shared.ts"),
        "export function sharedValue() { return 2; }\nexport const changed = true;\n",
    )
    .unwrap();
    let status_before = git_status(&root);
    let impact_started = Instant::now();
    let impact = command(
        &root,
        &[
            "--auto-index",
            "impact",
            "--diff",
            "HEAD",
            "--depth",
            "3",
            "--include-tests",
            "--max-files",
            "30",
            "--format",
            "json",
        ],
    );
    let impact_ms = impact_started.elapsed().as_secs_f64() * 1_000.0;
    assert!(
        impact.status.success(),
        "{}",
        String::from_utf8_lossy(&impact.stderr)
    );
    assert_eq!(status_before, git_status(&root));
    let impact = json(&impact);
    assert_eq!(impact["changed"][0]["path"], "src/shared.ts");
    assert!(
        impact["impact"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| {
                file["path"] == "src/feature.ts"
                    && file["roles"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|role| role == "dependent")
            })
    );
    assert!(impact["impact"]["truncated"].as_bool().unwrap());

    fs::rename(root.join("src/shared.ts"), root.join("src/shared_v2.ts")).unwrap();
    let rename_started = Instant::now();
    let renamed = command(&root, &["--auto-index", "status", "."]);
    let rename_ms = rename_started.elapsed().as_secs_f64() * 1_000.0;
    assert!(renamed.status.success());
    let renamed = json(&renamed);
    assert_eq!(renamed["exact_edges"], 100);
    assert_eq!(renamed["unresolved_imports"], 1);

    fs::write(
        root.join("src/feature.ts"),
        "import { sharedValue } from './shared_v2';\nexport function feature() { return sharedValue(); }\n",
    )
    .unwrap();
    let repaired = json(&command(&root, &["--auto-index", "status", "."]));
    assert_eq!(repaired["exact_edges"], 101);
    assert_eq!(repaired["unresolved_imports"], 0);

    println!(
        "fast-moving fixture: full_index_ms={full_ms:.2} diff_impact_ms={impact_ms:.2} target_rename_refresh_ms={rename_ms:.2} files=102 edges=101"
    );
    fs::remove_dir_all(root).unwrap();
}
