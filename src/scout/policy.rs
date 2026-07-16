use anyhow::{Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Explicit repository-owned architecture policy. An absent or disabled value
/// deliberately has no effect; Kiv never infers layers or rules.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub(crate) struct ArchitectureConfig {
    #[serde(default)]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) layers: Vec<ArchitectureLayerConfig>,
    #[serde(default)]
    pub(crate) rules: Vec<ArchitectureRuleConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct ArchitectureLayerConfig {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) include: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct ArchitectureRuleConfig {
    pub(crate) from: String,
    #[serde(default)]
    pub(crate) deny: Vec<String>,
}

/// Evidence from an already-resolved exact graph edge. The type intentionally
/// has no resolution/confidence field: callers must filter graph observations
/// before constructing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExactDependencyEvidence {
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) raw_target: String,
    pub(crate) kind: String,
    pub(crate) line: usize,
    pub(crate) resolver: String,
}

pub(crate) struct BoundaryCheckInput<'a> {
    /// Indexed files in the requested check scope (all files, or one --path).
    pub(crate) checked_paths: &'a [String],
    /// Exact outgoing dependency edges in the same scope.
    pub(crate) exact_edges: &'a [ExactDependencyEvidence],
    /// Ambiguous/unresolved graph observations in the same scope.
    pub(crate) unresolved_observations: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct BoundaryCheckResult {
    pub(crate) checked_files: usize,
    pub(crate) exact_edges: usize,
    pub(crate) unclassified_files: usize,
    pub(crate) unresolved_observations: usize,
    pub(crate) violations: Vec<BoundaryViolation>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct BoundaryViolation {
    pub(crate) source: String,
    pub(crate) source_layer: String,
    pub(crate) target: String,
    pub(crate) target_layer: String,
    pub(crate) rule: String,
    pub(crate) evidence: BoundaryEvidence,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct BoundaryEvidence {
    pub(crate) raw_target: String,
    pub(crate) kind: String,
    pub(crate) line: usize,
    pub(crate) resolver: String,
}

#[derive(Debug)]
struct CompiledLayer {
    name: String,
    patterns: GlobSet,
}

/// A validated, deterministic policy ready to apply to exact dependency edges.
#[derive(Debug)]
pub(crate) struct ArchitecturePolicy {
    layers: Vec<CompiledLayer>,
    denied: BTreeSet<(String, String)>,
}

impl ArchitecturePolicy {
    /// Returns no policy for absent or disabled configuration. Disabled policy
    /// contents are ignored so a repository can stage configuration safely.
    pub(crate) fn from_config(config: Option<&ArchitectureConfig>) -> Result<Option<Self>> {
        let Some(config) = config.filter(|config| config.enabled) else {
            return Ok(None);
        };

        let mut declared = BTreeSet::new();
        for layer in &config.layers {
            validate_layer_name(&layer.name)?;
            if !declared.insert(layer.name.clone()) {
                bail!("architecture layer name is duplicated: {:?}", layer.name);
            }
        }

        let mut layers = Vec::with_capacity(config.layers.len());
        for layer in &config.layers {
            let mut builder = GlobSetBuilder::new();
            for pattern in &layer.include {
                let glob = GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .backslash_escape(false)
                    .build()
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "invalid architecture glob {:?} for layer {:?}: {error}",
                            pattern,
                            layer.name
                        )
                    })?;
                builder.add(glob);
            }
            let patterns = builder.build().map_err(|error| {
                anyhow::anyhow!(
                    "failed to compile architecture globs for layer {:?}: {error}",
                    layer.name
                )
            })?;
            layers.push(CompiledLayer {
                name: layer.name.clone(),
                patterns,
            });
        }
        // Classification and error output must not depend on TOML declaration order.
        layers.sort_by(|left, right| left.name.cmp(&right.name));

        let mut denied = BTreeSet::new();
        for rule in &config.rules {
            if !declared.contains(&rule.from) {
                bail!(
                    "architecture rule references unknown source layer {:?}",
                    rule.from
                );
            }
            if rule.deny.is_empty() {
                bail!(
                    "architecture rule from layer {:?} has an empty deny list",
                    rule.from
                );
            }
            for target in &rule.deny {
                if !declared.contains(target) {
                    bail!(
                        "architecture rule from {:?} denies unknown layer {:?}",
                        rule.from,
                        target
                    );
                }
                denied.insert((rule.from.clone(), target.clone()));
            }
        }

        Ok(Some(Self { layers, denied }))
    }

    /// Assign a normalized repo-relative path to zero or one configured layer.
    /// Matching more than one layer is an actionable configuration error.
    pub(crate) fn classify(&self, path: &str) -> Result<Option<&str>> {
        let path = normalize_repo_path(path)?;
        let matches = self
            .layers
            .iter()
            .filter(|layer| layer.patterns.is_match(&path))
            .map(|layer| layer.name.as_str())
            .collect::<Vec<_>>();

        match matches.as_slice() {
            [] => Ok(None),
            [name] => Ok(Some(*name)),
            _ => bail!(
                "architecture layer overlap for path {:?}: matched layers {}",
                path,
                matches
                    .iter()
                    .map(|name| format!("{name:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// Evaluate a caller-supplied set of exact graph edges. Classification is
    /// performed before traversal so overlaps fail even if no denied edge uses
    /// the overlapping file.
    pub(crate) fn evaluate(&self, input: BoundaryCheckInput<'_>) -> Result<BoundaryCheckResult> {
        let mut classifications = BTreeMap::<String, Option<String>>::new();
        for path in input.checked_paths {
            let normalized = normalize_repo_path(path)?;
            let layer = self.classify(&normalized)?.map(str::to_owned);
            classifications.insert(normalized, layer);
        }

        // Edges may target a file outside the selected source scope. Classify
        // both endpoints, and surface any overlap in either one.
        for edge in input.exact_edges {
            for path in [&edge.source, &edge.target] {
                let normalized = normalize_repo_path(path)?;
                let layer = self.classify(&normalized)?.map(str::to_owned);
                classifications.entry(normalized).or_insert(layer);
            }
        }

        let checked_files = input
            .checked_paths
            .iter()
            .map(|path| normalize_repo_path(path))
            .collect::<Result<BTreeSet<_>>>()?
            .len();
        let unclassified_files = input
            .checked_paths
            .iter()
            .map(|path| normalize_repo_path(path))
            .collect::<Result<BTreeSet<_>>>()?
            .into_iter()
            .filter(|path| classifications.get(path).is_none_or(Option::is_none))
            .count();

        let mut violations = Vec::new();
        for edge in input.exact_edges {
            let source = normalize_repo_path(&edge.source)?;
            let target = normalize_repo_path(&edge.target)?;
            let Some(source_layer) = classifications.get(&source).and_then(Option::as_deref) else {
                continue;
            };
            let Some(target_layer) = classifications.get(&target).and_then(Option::as_deref) else {
                continue;
            };
            if !self
                .denied
                .contains(&(source_layer.to_owned(), target_layer.to_owned()))
            {
                continue;
            }
            violations.push(BoundaryViolation {
                source,
                source_layer: source_layer.to_owned(),
                target,
                target_layer: target_layer.to_owned(),
                rule: format!("{source_layer} denies {target_layer}"),
                evidence: BoundaryEvidence {
                    raw_target: edge.raw_target.clone(),
                    kind: edge.kind.clone(),
                    line: edge.line,
                    resolver: edge.resolver.clone(),
                },
            });
        }
        violations.sort_by(|left, right| {
            (
                &left.source,
                &left.target,
                left.evidence.line,
                &left.evidence.raw_target,
            )
                .cmp(&(
                    &right.source,
                    &right.target,
                    right.evidence.line,
                    &right.evidence.raw_target,
                ))
        });

        Ok(BoundaryCheckResult {
            checked_files,
            exact_edges: input.exact_edges.len(),
            unclassified_files,
            unresolved_observations: input.unresolved_observations,
            violations,
        })
    }
}

impl BoundaryCheckResult {
    pub(crate) fn render_markdown(&self) -> String {
        let mut output = format!(
            "# Architecture boundary check\n\n- Checked files: {}\n- Exact edges: {}\n- Unclassified files: {}\n- Unresolved observations: {}\n- Violations: {}\n",
            self.checked_files,
            self.exact_edges,
            self.unclassified_files,
            self.unresolved_observations,
            self.violations.len()
        );

        if self.violations.is_empty() {
            output.push_str("\nNo architecture boundary violations.\n");
            return output;
        }

        output.push_str("\n## Violations\n");
        for violation in &self.violations {
            output.push_str(&format!(
                "\n- `{}` ({}) -> `{}` ({}) — {}\n  - import `{}`; kind `{}`; line {}; resolver `{}`\n",
                violation.source,
                violation.source_layer,
                violation.target,
                violation.target_layer,
                violation.rule,
                violation.evidence.raw_target,
                violation.evidence.kind,
                violation.evidence.line,
                violation.evidence.resolver
            ));
        }
        output
    }
}

fn validate_layer_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        bail!("architecture layer names must not be empty");
    };
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        bail!(
            "invalid architecture layer name {:?}: expected a case-sensitive identifier",
            name
        );
    }
    Ok(())
}

fn normalize_repo_path(path: &str) -> Result<String> {
    let path = path.replace('\\', "/");
    let path = path.strip_prefix("./").unwrap_or(&path);
    if path.is_empty() || path.starts_with('/') {
        bail!("architecture paths must be non-empty and repo-relative: {path:?}");
    }

    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    bail!("architecture path escapes the repository: {path:?}");
                }
            }
            value => parts.push(value),
        }
    }
    if parts.is_empty() {
        bail!("architecture paths must name a file: {path:?}");
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ArchitectureConfig {
        toml::from_str(
            r#"
enabled = true

[[layers]]
name = "domain"
include = ["src/domain/**"]

[[layers]]
name = "application"
include = ["src/application/**"]

[[layers]]
name = "infrastructure"
include = ["src/infrastructure/**"]

[[rules]]
from = "domain"
deny = ["infrastructure"]
"#,
        )
        .unwrap()
    }

    fn edge(source: &str, target: &str) -> ExactDependencyEvidence {
        ExactDependencyEvidence {
            source: source.to_owned(),
            target: target.to_owned(),
            raw_target: "../infrastructure/database".to_owned(),
            kind: "import".to_owned(),
            line: 8,
            resolver: "typescript-relative".to_owned(),
        }
    }

    #[test]
    fn absent_or_disabled_configuration_has_no_policy() {
        assert!(ArchitecturePolicy::from_config(None).unwrap().is_none());
        assert!(
            ArchitecturePolicy::from_config(Some(&ArchitectureConfig::default()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn evaluates_denied_allowed_and_unclassified_exact_edges() {
        let policy = ArchitecturePolicy::from_config(Some(&config()))
            .unwrap()
            .unwrap();
        let checked_paths = vec![
            "src/domain/order.ts".to_owned(),
            "src/application/create_order.ts".to_owned(),
            "scripts/release.ts".to_owned(),
        ];
        let exact_edges = vec![
            edge("src/domain/order.ts", "src/infrastructure/database.ts"),
            edge(
                "src/application/create_order.ts",
                "src/infrastructure/database.ts",
            ),
            edge("scripts/release.ts", "src/infrastructure/database.ts"),
        ];

        let result = policy
            .evaluate(BoundaryCheckInput {
                checked_paths: &checked_paths,
                exact_edges: &exact_edges,
                unresolved_observations: 2,
            })
            .unwrap();

        assert_eq!(result.checked_files, 3);
        assert_eq!(result.exact_edges, 3);
        assert_eq!(result.unclassified_files, 1);
        assert_eq!(result.unresolved_observations, 2);
        assert_eq!(result.violations.len(), 1);
        let violation = &result.violations[0];
        assert_eq!(violation.source_layer, "domain");
        assert_eq!(violation.target_layer, "infrastructure");
        assert_eq!(violation.evidence.line, 8);
        assert!(
            result
                .render_markdown()
                .contains("domain denies infrastructure")
        );
        assert!(serde_json::to_value(result).unwrap()["violations"].is_array());
    }

    #[test]
    fn overlapping_matches_report_path_and_all_layers_deterministically() {
        let config: ArchitectureConfig = toml::from_str(
            r#"
enabled = true
[[layers]]
name = "zeta"
include = ["src/**"]
[[layers]]
name = "alpha"
include = ["src/domain/**"]
"#,
        )
        .unwrap();
        let policy = ArchitecturePolicy::from_config(Some(&config))
            .unwrap()
            .unwrap();
        let error = policy
            .classify("src/domain/order.rs")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "architecture layer overlap for path \"src/domain/order.rs\": matched layers \"alpha\", \"zeta\""
        );
    }

    #[test]
    fn validation_rejects_duplicate_names_empty_denies_and_unknown_references() {
        let mut duplicate = config();
        duplicate.layers.push(duplicate.layers[0].clone());
        assert!(
            ArchitecturePolicy::from_config(Some(&duplicate))
                .unwrap_err()
                .to_string()
                .contains("duplicated")
        );

        let mut empty = config();
        empty.rules[0].deny.clear();
        assert!(
            ArchitecturePolicy::from_config(Some(&empty))
                .unwrap_err()
                .to_string()
                .contains("empty deny")
        );

        let mut unknown = config();
        unknown.rules[0].deny = vec!["missing".to_owned()];
        assert!(
            ArchitecturePolicy::from_config(Some(&unknown))
                .unwrap_err()
                .to_string()
                .contains("unknown layer")
        );

        let mut unknown_source = config();
        unknown_source.rules[0].from = "missing".to_owned();
        assert!(
            ArchitecturePolicy::from_config(Some(&unknown_source))
                .unwrap_err()
                .to_string()
                .contains("unknown source layer")
        );
    }

    #[test]
    fn validation_reports_the_layer_and_pattern_for_malformed_globs() {
        let mut malformed = config();
        malformed.layers[0].include = vec!["src/domain/[".to_owned()];
        let error = ArchitecturePolicy::from_config(Some(&malformed))
            .unwrap_err()
            .to_string();
        assert!(error.contains("domain"));
        assert!(error.contains("src/domain/["));
    }

    #[test]
    fn layer_names_and_rule_references_are_case_sensitive() {
        let config: ArchitectureConfig = toml::from_str(
            r#"
enabled = true
[[layers]]
name = "Domain"
include = ["upper/**"]
[[layers]]
name = "domain"
include = ["lower/**"]
[[rules]]
from = "Domain"
deny = ["domain"]
"#,
        )
        .unwrap();
        let policy = ArchitecturePolicy::from_config(Some(&config))
            .unwrap()
            .unwrap();
        assert_eq!(policy.classify("upper/a.rs").unwrap(), Some("Domain"));
        assert_eq!(policy.classify("lower/a.rs").unwrap(), Some("domain"));
    }

    #[test]
    fn declaration_order_does_not_change_results() {
        let first = config();
        let mut second = first.clone();
        second.layers.reverse();
        second.rules.reverse();
        let paths = vec!["src/domain/order.ts".to_owned()];
        let edges = vec![edge(
            "src/domain/order.ts",
            "src/infrastructure/database.ts",
        )];
        let input = || BoundaryCheckInput {
            checked_paths: &paths,
            exact_edges: &edges,
            unresolved_observations: 0,
        };
        let first_result = ArchitecturePolicy::from_config(Some(&first))
            .unwrap()
            .unwrap()
            .evaluate(input())
            .unwrap();
        let second_result = ArchitecturePolicy::from_config(Some(&second))
            .unwrap()
            .unwrap()
            .evaluate(input())
            .unwrap();
        assert_eq!(first_result, second_result);
    }

    #[test]
    fn normalizes_separators_and_rejects_paths_outside_repo() {
        let policy = ArchitecturePolicy::from_config(Some(&config()))
            .unwrap()
            .unwrap();
        assert_eq!(
            policy.classify(".\\src\\domain\\order.ts").unwrap(),
            Some("domain")
        );
        assert!(policy.classify("../outside.rs").is_err());
    }
}
