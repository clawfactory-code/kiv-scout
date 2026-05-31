use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Args;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use walkdir::WalkDir;

mod capsule;
pub(crate) use capsule::{CapsuleCap, CapsuleMode, capsule, render_skeleton};

#[derive(Args)]
pub struct BenchArgs {
    /// Repository root to benchmark.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Query for capsule benchmarks.
    #[arg(long, default_value = "where is request authentication checked")]
    query: String,
    /// Number of runs for low-latency commands.
    #[arg(long, default_value_t = 7)]
    runs: usize,
    /// Emit JSON instead of a table.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct BenchReport {
    repo: String,
    indexed_files: Option<u64>,
    index_db_bytes: Option<u64>,
    binary: String,
    generated_at: String,
    runs: Vec<BenchRun>,
}

#[derive(Debug, Serialize)]
struct BenchRun {
    name: String,
    samples: usize,
    min_ms: f64,
    median_ms: f64,
    mean_ms: f64,
    max_ms: f64,
}

pub fn bench_command(args: BenchArgs) -> Result<()> {
    let report = bench(&args.repo, &args.query, args.runs)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_bench_table(&report);
    }
    Ok(())
}

fn bench(repo: &Path, query: &str, runs: usize) -> Result<BenchReport> {
    let repo = fs::canonicalize(repo).with_context(|| format!("bad repo {}", repo.display()))?;
    let binary = current_binary()?;
    let mut results = Vec::new();

    results.push(time_command(
        "status",
        &binary,
        &["status", repo.to_str().context("non-utf8 repo path")?],
        &repo,
        runs.max(1),
    )?);
    results.push(time_command(
        "capsule",
        &binary,
        &["capsule", query, "--cap", "balanced"],
        &repo,
        runs.max(1),
    )?);

    if let Some(file) = largest_supported_file(&repo) {
        results.push(time_command(
            &format!("skeleton {}", display_rel(&repo, &file)),
            &binary,
            &["skeleton", file.to_str().context("non-utf8 file path")?],
            &repo,
            runs.clamp(1, 5),
        )?);
    }

    let index_db = repo.join(".kiv").join("index.db");
    Ok(BenchReport {
        repo: repo.display().to_string(),
        indexed_files: indexed_file_count(&repo).ok(),
        index_db_bytes: fs::metadata(index_db).ok().map(|meta| meta.len()),
        binary: binary.display().to_string(),
        generated_at: Utc::now().to_rfc3339(),
        runs: results,
    })
}

fn time_command(
    name: &str,
    binary: &Path,
    args: &[&str],
    cwd: &Path,
    samples: usize,
) -> Result<BenchRun> {
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        let output = Command::new(binary).args(args).current_dir(cwd).output()?;
        if !output.status.success() {
            bail!(
                "{} failed: {}",
                name,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    Ok(BenchRun {
        name: name.to_string(),
        samples,
        min_ms: times[0],
        median_ms: times[times.len() / 2],
        mean_ms: mean,
        max_ms: *times.last().unwrap_or(&times[0]),
    })
}

fn print_bench_table(report: &BenchReport) {
    println!("repo: {}", report.repo);
    println!("binary: {}", report.binary);
    if let Some(files) = report.indexed_files {
        println!("indexed_files: {files}");
    }
    if let Some(bytes) = report.index_db_bytes {
        println!("index_db_bytes: {bytes}");
    }
    println!("case\truns\tmin_ms\tmedian_ms\tmean_ms\tmax_ms");
    for run in &report.runs {
        println!(
            "{}\t{}\t{:.1}\t{:.1}\t{:.1}\t{:.1}",
            run.name, run.samples, run.min_ms, run.median_ms, run.mean_ms, run.max_ms
        );
    }
}

fn current_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    if exe.ends_with("deps") {
        bail!("cannot benchmark from test harness");
    }
    Ok(exe)
}

fn indexed_file_count(repo: &Path) -> Result<u64> {
    let output = Command::new(current_binary()?)
        .args(["status", repo.to_str().context("non-utf8 repo path")?])
        .output()?;
    if !output.status.success() {
        return Ok(0);
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    Ok(value.get("files").and_then(|v| v.as_u64()).unwrap_or(0))
}

fn largest_supported_file(repo: &Path) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in WalkDir::new(repo).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let rel = display_rel(repo, path);
        if crate::should_skip(&rel) || !is_supported_source(path) {
            continue;
        }
        let size = entry.metadata().ok()?.len();
        if best
            .as_ref()
            .map(|(best_size, _)| size > *best_size)
            .unwrap_or(true)
        {
            best = Some((size, path.to_path_buf()));
        }
    }
    best.map(|(_, path)| path)
}

fn is_supported_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("rs" | "py" | "ts" | "tsx" | "js" | "jsx")
    )
}

fn display_rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}
