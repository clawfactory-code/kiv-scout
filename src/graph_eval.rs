use super::*;
use crate::graph::{DependencyEdge, semantic_dependency_edges, semantic_edge_digest};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Deserialize)]
struct Fixture {
    language: String,
    edges: Vec<ExpectedEdge>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct ExpectedEdge {
    source_path: String,
    raw_target: String,
    expected_target: Option<String>,
    kind: String,
    line: usize,
    resolution: String,
    resolver: String,
    #[serde(default)]
    candidate_paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct EdgeIdentity {
    source_path: String,
    raw_target: String,
    line: usize,
}

#[derive(Clone, Debug)]
struct LabeledEdge {
    language: String,
    edge: ExpectedEdge,
}

#[derive(Clone, Debug)]
struct EmittedEdge {
    language: String,
    edge: ExpectedEdge,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct Metrics {
    labeled_supported: usize,
    emitted_exact: usize,
    correct_exact: usize,
    wrong_exact: usize,
    duplicate_exact: usize,
    observations: usize,
    unresolved: usize,
    labeled_ambiguous: usize,
    correct_ambiguous: usize,
    precision: Option<f64>,
    recall: Option<f64>,
    unresolved_rate: Option<f64>,
    ambiguity_accuracy: Option<f64>,
}

#[derive(Debug, Serialize)]
struct OfflineReport {
    schema_version: u8,
    kiv_version: &'static str,
    fixture_metrics: BTreeMap<String, Metrics>,
    determinism: DeterminismReport,
}

#[derive(Debug, Serialize)]
struct DeterminismReport {
    runs: usize,
    unique_semantic_digests: usize,
}

fn edge_identity(edge: &ExpectedEdge) -> EdgeIdentity {
    EdgeIdentity {
        source_path: edge.source_path.clone(),
        raw_target: edge.raw_target.clone(),
        line: edge.line,
    }
}

fn expected_from_actual(edge: &DependencyEdge) -> ExpectedEdge {
    ExpectedEdge {
        source_path: edge.source_path.clone(),
        raw_target: edge.raw_target.clone(),
        expected_target: edge.target_path.clone(),
        kind: edge.kind.clone(),
        line: edge.line,
        resolution: edge.resolution.clone(),
        resolver: edge.resolver.clone(),
        candidate_paths: edge.candidate_paths.clone(),
    }
}

fn ratio(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator != 0).then(|| numerator as f64 / denominator as f64)
}

fn calculate_metrics(labels: &[LabeledEdge], emitted: &[EmittedEdge]) -> BTreeMap<String, Metrics> {
    let languages = labels
        .iter()
        .map(|item| item.language.clone())
        .chain(emitted.iter().map(|item| item.language.clone()))
        .collect::<BTreeSet<_>>();
    let mut output = BTreeMap::new();

    for language in languages {
        let language_labels = labels
            .iter()
            .filter(|item| item.language == language)
            .collect::<Vec<_>>();
        let language_emitted = emitted
            .iter()
            .filter(|item| item.language == language)
            .collect::<Vec<_>>();
        let exact_labels = language_labels
            .iter()
            .filter(|item| item.edge.resolution == "exact")
            .map(|item| (edge_identity(&item.edge), item.edge.expected_target.clone()))
            .collect::<BTreeMap<_, _>>();
        let emitted_exact = language_emitted
            .iter()
            .filter(|item| item.edge.resolution == "exact")
            .collect::<Vec<_>>();
        let mut credited = BTreeSet::new();
        let mut correct_exact = 0;
        let mut seen_exact = BTreeSet::new();
        let mut duplicate_exact = 0;
        for item in &emitted_exact {
            let semantic = item.edge.clone();
            if !seen_exact.insert(semantic) {
                duplicate_exact += 1;
            }
            let identity = edge_identity(&item.edge);
            if exact_labels.get(&identity) == Some(&item.edge.expected_target)
                && credited.insert(identity)
            {
                correct_exact += 1;
            }
        }
        let labeled_ambiguous = language_labels
            .iter()
            .filter(|item| item.edge.resolution == "ambiguous")
            .count();
        let correct_ambiguous = language_labels
            .iter()
            .filter(|label| label.edge.resolution == "ambiguous")
            .filter(|label| {
                language_emitted.iter().any(|actual| {
                    edge_identity(&actual.edge) == edge_identity(&label.edge)
                        && actual.edge.resolution == "ambiguous"
                        && actual.edge.candidate_paths == label.edge.candidate_paths
                })
            })
            .count();
        let unresolved = language_emitted
            .iter()
            .filter(|item| item.edge.resolution == "unresolved")
            .count();
        let observations = language_emitted.len();
        let labeled_supported = exact_labels.len();
        let emitted_exact_count = emitted_exact.len();
        output.insert(
            language,
            Metrics {
                labeled_supported,
                emitted_exact: emitted_exact_count,
                correct_exact,
                wrong_exact: emitted_exact_count.saturating_sub(correct_exact),
                duplicate_exact,
                observations,
                unresolved,
                labeled_ambiguous,
                correct_ambiguous,
                precision: ratio(correct_exact, emitted_exact_count),
                recall: ratio(correct_exact, labeled_supported),
                unresolved_rate: ratio(unresolved, observations),
                ambiguity_accuracy: ratio(correct_ambiguous, labeled_ambiguous),
            },
        );
    }
    output
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/graph")
}

fn fixture_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for language in ["rust", "python", "typescript", "javascript"] {
        for case in ["exact", "ambiguous", "unresolved"] {
            dirs.push(fixture_root().join(language).join(case));
        }
    }
    dirs
}

fn load_fixture(root: &Path) -> Fixture {
    serde_json::from_slice(&fs::read(root.join("expected-edges.json")).unwrap()).unwrap()
}

fn load_indexed_files(root: &Path) -> Vec<IndexedFile> {
    let mut files = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let rel = entry.path().strip_prefix(root).ok()?;
            let path = rel.to_string_lossy().replace('\\', "/");
            let lang = language_for(&path);
            matches!(
                lang.as_str(),
                "rust" | "python" | "typescript" | "javascript"
            )
            .then(|| (entry.into_path(), path, lang))
        })
        .map(|(absolute, path, lang)| {
            let text = fs::read_to_string(absolute).unwrap();
            IndexedFile {
                hash: hash_text(&text),
                symbols: extract_symbols(&lang, &text),
                imports: extract_import_refs(&lang, &text),
                path,
                lang,
                text,
            }
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    files
}

fn graph_for_files(files: &[IndexedFile]) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    write_index(&mut conn, Path::new("/semantic-evaluation"), files).unwrap();
    conn
}

fn fixture_results() -> (Vec<LabeledEdge>, Vec<EmittedEdge>) {
    let mut labels = Vec::new();
    let mut emitted = Vec::new();
    for root in fixture_dirs() {
        let fixture = load_fixture(&root);
        let conn = graph_for_files(&load_indexed_files(&root));
        let actual = semantic_dependency_edges(&conn).unwrap();
        let expected = fixture.edges.clone();
        let actual_expected = actual.iter().map(expected_from_actual).collect::<Vec<_>>();
        assert_eq!(
            actual_expected,
            expected,
            "fixture mismatch in {}",
            root.display()
        );
        labels.extend(expected.into_iter().map(|edge| LabeledEdge {
            language: fixture.language.clone(),
            edge,
        }));
        emitted.extend(actual.iter().map(|edge| EmittedEdge {
            language: fixture.language.clone(),
            edge: expected_from_actual(edge),
        }));
    }
    (labels, emitted)
}

fn shuffled<T>(mut values: Vec<T>, seed: u64) -> Vec<T> {
    let mut state = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    for upper in (1..values.len()).rev() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        values.swap(upper, state as usize % (upper + 1));
    }
    values
}

fn determinism_digests(runs: usize) -> BTreeSet<String> {
    let mut files = Vec::new();
    for (case_index, root) in fixture_dirs().into_iter().enumerate() {
        // Fixture paths intentionally collide across cases. Prefix the whole fixture (not each
        // individual file) so its relative exact/ambiguous relationships remain intact.
        for mut file in load_indexed_files(&root) {
            file.path = format!("case-{case_index}/{}", file.path);
            files.push(file);
        }
    }
    let mut digests = BTreeSet::new();
    for run in 0..runs {
        let mut ordered = shuffled(files.clone(), run as u64);
        for (index, file) in ordered.iter_mut().enumerate() {
            file.imports = shuffled(file.imports.clone(), (run * 101 + index) as u64);
        }
        let conn = graph_for_files(&ordered);
        digests.insert(semantic_edge_digest(&conn).unwrap());
    }
    digests
}

#[derive(Debug, Deserialize)]
struct IncrementalScenario {
    language: String,
    source: ScenarioFile,
    target: ScenarioFile,
    renamed_target: ScenarioFile,
    expected: ScenarioExpectedStates,
}

#[derive(Debug, Deserialize)]
struct ScenarioFile {
    path: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct ScenarioExpectedStates {
    initial: ScenarioExpectedEdge,
    added: ScenarioExpectedEdge,
    deleted: ScenarioExpectedEdge,
    renamed: ScenarioExpectedEdge,
}

#[derive(Debug, Deserialize)]
struct ScenarioExpectedEdge {
    resolution: String,
    target_path: Option<String>,
}

fn indexed_scenario_file(file: &ScenarioFile, lang: &str) -> IndexedFile {
    IndexedFile {
        path: file.path.clone(),
        lang: lang.to_string(),
        hash: hash_text(&file.text),
        text: file.text.clone(),
        symbols: extract_symbols(lang, &file.text),
        imports: extract_import_refs(lang, &file.text),
    }
}

fn clean_digest(files: &[IndexedFile]) -> String {
    semantic_edge_digest(&graph_for_files(files)).unwrap()
}

fn assert_scenario_edge(conn: &Connection, expected: &ScenarioExpectedEdge, context: &str) {
    let edges = semantic_dependency_edges(conn).unwrap();
    assert_eq!(edges.len(), 1, "{context}: expected one observation");
    assert_eq!(edges[0].resolution, expected.resolution, "{context}");
    assert_eq!(edges[0].target_path, expected.target_path, "{context}");
}

#[test]
fn contract_fixtures_meet_offline_release_gates() {
    let (labels, emitted) = fixture_results();
    let metrics = calculate_metrics(&labels, &emitted);
    for (language, result) in &metrics {
        assert_eq!(result.wrong_exact, 0, "wrong exact edge in {language}");
        if result.labeled_supported > 0 {
            assert_eq!(result.precision, Some(1.0), "precision gate in {language}");
            assert_eq!(result.recall, Some(1.0), "recall gate in {language}");
        }
        if result.labeled_ambiguous > 0 {
            assert_eq!(
                result.ambiguity_accuracy,
                Some(1.0),
                "ambiguity gate in {language}"
            );
        }
    }
    let digests = determinism_digests(20);
    assert_eq!(
        digests.len(),
        1,
        "semantic graph changed across build orders"
    );
    let report = OfflineReport {
        schema_version: 1,
        kiv_version: env!("CARGO_PKG_VERSION"),
        fixture_metrics: metrics,
        determinism: DeterminismReport {
            runs: 20,
            unique_semantic_digests: digests.len(),
        },
    };
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

#[test]
fn incremental_target_add_delete_and_rename_equal_full_rebuilds() {
    for language in ["rust", "python", "typescript", "javascript"] {
        let scenario_path = fixture_root()
            .join(language)
            .join("incremental/scenario.json");
        let scenario: IncrementalScenario =
            serde_json::from_slice(&fs::read(&scenario_path).unwrap()).unwrap();
        assert_eq!(scenario.language, language);
        let source = indexed_scenario_file(&scenario.source, language);
        let target = indexed_scenario_file(&scenario.target, language);
        let renamed = indexed_scenario_file(&scenario.renamed_target, language);

        let mut conn = graph_for_files(std::slice::from_ref(&source));
        assert_scenario_edge(&conn, &scenario.expected.initial, "initial");

        write_index_delta(
            &mut conn,
            Path::new("/semantic-evaluation"),
            std::slice::from_ref(&target),
            &[],
        )
        .unwrap();
        assert_scenario_edge(&conn, &scenario.expected.added, "add");
        assert_eq!(
            semantic_edge_digest(&conn).unwrap(),
            clean_digest(&[source.clone(), target.clone()]),
            "{language}: target add diverged from full rebuild"
        );

        write_index_delta(
            &mut conn,
            Path::new("/semantic-evaluation"),
            &[],
            std::slice::from_ref(&target.path),
        )
        .unwrap();
        assert_scenario_edge(&conn, &scenario.expected.deleted, "delete");
        assert_eq!(
            semantic_edge_digest(&conn).unwrap(),
            clean_digest(std::slice::from_ref(&source)),
            "{language}: target delete diverged from full rebuild"
        );

        write_index_delta(
            &mut conn,
            Path::new("/semantic-evaluation"),
            std::slice::from_ref(&target),
            &[],
        )
        .unwrap();
        write_index_delta(
            &mut conn,
            Path::new("/semantic-evaluation"),
            std::slice::from_ref(&renamed),
            std::slice::from_ref(&target.path),
        )
        .unwrap();
        assert_scenario_edge(&conn, &scenario.expected.renamed, "rename");
        assert_eq!(
            semantic_edge_digest(&conn).unwrap(),
            clean_digest(&[source, renamed]),
            "{language}: target rename diverged from full rebuild"
        );
    }
}

fn labeled(language: &str, source: &str, target: Option<&str>, resolution: &str) -> LabeledEdge {
    LabeledEdge {
        language: language.to_string(),
        edge: ExpectedEdge {
            source_path: source.to_string(),
            raw_target: "./dependency".to_string(),
            expected_target: target.map(str::to_string),
            kind: "import".to_string(),
            line: 1,
            resolution: resolution.to_string(),
            resolver: "test".to_string(),
            candidate_paths: Vec::new(),
        },
    }
}

fn emitted(label: &LabeledEdge) -> EmittedEdge {
    EmittedEdge {
        language: label.language.clone(),
        edge: label.edge.clone(),
    }
}

#[test]
fn metric_zero_denominators_are_explicit() {
    let metrics = calculate_metrics(&[], &[]);
    assert!(metrics.is_empty());
    assert_eq!(ratio(0, 0), None);
}

#[test]
fn wrong_exact_edges_fail_precision_even_when_a_target_is_emitted() {
    let label = labeled("rust", "src/a.rs", Some("src/right.rs"), "exact");
    let mut wrong = emitted(&label);
    wrong.edge.expected_target = Some("src/wrong.rs".to_string());
    let metrics = calculate_metrics(&[label], &[wrong]);
    assert_eq!(metrics["rust"].correct_exact, 0);
    assert_eq!(metrics["rust"].wrong_exact, 1);
    assert_eq!(metrics["rust"].precision, Some(0.0));
    assert_eq!(metrics["rust"].recall, Some(0.0));
}

#[test]
fn duplicate_rows_are_not_double_credited() {
    let label = labeled("python", "pkg/a.py", Some("pkg/dependency.py"), "exact");
    let row = emitted(&label);
    let metrics = calculate_metrics(&[label], &[row.clone(), row]);
    assert_eq!(metrics["python"].correct_exact, 1);
    assert_eq!(metrics["python"].emitted_exact, 2);
    assert_eq!(metrics["python"].duplicate_exact, 1);
    assert_eq!(metrics["python"].precision, Some(0.5));
}

#[test]
fn metrics_stay_separate_per_language() {
    let rust = labeled("rust", "src/a.rs", Some("src/dependency.rs"), "exact");
    let python = labeled("python", "pkg/a.py", Some("pkg/dependency.py"), "exact");
    let mut wrong_python = emitted(&python);
    wrong_python.edge.expected_target = Some("pkg/wrong.py".to_string());
    let metrics = calculate_metrics(&[rust.clone(), python], &[emitted(&rust), wrong_python]);
    assert_eq!(metrics["rust"].precision, Some(1.0));
    assert_eq!(metrics["python"].precision, Some(0.0));
}

#[derive(Debug, Deserialize)]
struct CorpusManifest {
    schema_version: u8,
    repositories: Vec<CorpusRepository>,
}

#[derive(Debug, Deserialize)]
struct CorpusRepository {
    repo: String,
    language: String,
    commit: String,
    checkout_dir: String,
    oracle: String,
}

#[derive(Debug, Deserialize)]
struct CorpusOracle {
    repo: String,
    language: String,
    observations: Vec<CorpusObservation>,
}

#[derive(Debug, Deserialize)]
struct CorpusObservation {
    source_path: String,
    raw_target: String,
    line: usize,
    expected_target: Option<String>,
    resolution: String,
}

#[test]
#[ignore = "requires public repositories checked out at their pinned commits"]
fn public_corpus_matches_pinned_oracles() {
    let metadata_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/graph");
    let manifest: CorpusManifest =
        serde_json::from_slice(&fs::read(metadata_root.join("repos.json")).unwrap()).unwrap();
    assert_eq!(manifest.schema_version, 1);
    let corpus_root = PathBuf::from(
        env::var_os("KIV_GRAPH_CORPUS_ROOT").expect("set KIV_GRAPH_CORPUS_ROOT per corpus README"),
    );
    let mut report = BTreeMap::new();
    for repository in manifest.repositories {
        assert_eq!(repository.commit.len(), 40, "pin must be a full commit SHA");
        assert!(!Path::new(&repository.checkout_dir).is_absolute());
        assert!(!Path::new(&repository.oracle).is_absolute());
        let checkout = corpus_root.join(&repository.checkout_dir);
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&checkout)
            .output()
            .unwrap();
        assert!(
            head.status.success(),
            "{} is not a Git checkout",
            checkout.display()
        );
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            repository.commit
        );
        let oracle: CorpusOracle =
            serde_json::from_slice(&fs::read(metadata_root.join(&repository.oracle)).unwrap())
                .unwrap();
        assert_eq!(oracle.repo, repository.repo);
        assert_eq!(oracle.language, repository.language);
        let conn = graph_for_files(&load_indexed_files(&checkout));
        let actual = semantic_dependency_edges(&conn).unwrap();
        let labels = oracle
            .observations
            .iter()
            .map(|observation| LabeledEdge {
                language: repository.language.clone(),
                edge: ExpectedEdge {
                    source_path: observation.source_path.clone(),
                    raw_target: observation.raw_target.clone(),
                    expected_target: observation.expected_target.clone(),
                    kind: String::new(),
                    line: observation.line,
                    resolution: observation.resolution.clone(),
                    resolver: String::new(),
                    candidate_paths: Vec::new(),
                },
            })
            .collect::<Vec<_>>();
        let identities = labels
            .iter()
            .map(|label| edge_identity(&label.edge))
            .collect::<BTreeSet<_>>();
        let emitted = actual
            .iter()
            .filter(|edge| {
                identities.contains(&EdgeIdentity {
                    source_path: edge.source_path.clone(),
                    raw_target: edge.raw_target.clone(),
                    line: edge.line,
                })
            })
            .map(|edge| EmittedEdge {
                language: repository.language.clone(),
                edge: expected_from_actual(edge),
            })
            .collect::<Vec<_>>();
        let metrics = calculate_metrics(&labels, &emitted);
        let language_metrics = metrics.get(&repository.language).unwrap();
        assert_eq!(
            language_metrics.wrong_exact, 0,
            "{} wrong edge",
            repository.repo
        );
        assert_eq!(
            language_metrics.recall,
            Some(1.0),
            "{} recall",
            repository.repo
        );
        report.insert(repository.repo, language_metrics.clone());
    }
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
