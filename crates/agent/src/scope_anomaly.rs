//! Layer 2 (Phase C) of the corruption defense system: Scope anomaly detection.
//!
//! This module detects when a model's edit is wildly out of proportion to the
//! requested task. If the user asks for a simple rename but the model touches
//! 7 files and changes 1200 lines, that's a corruption signal.
//!
//! # Design
//!
//! ```text
//! User prompt: "rename variable x to y"
//! Expected scope: 1 file, few lines changed
//!
//! Actual model output: 7 files touched, +1285/-843 lines
//!
//! → ScopeAnomaly detected → CompletionError::ScopeAnomaly → corruption retry
//! ```
//!
//! # Integration with the corruption retry pipeline
//!
//! `CompletionError::ScopeAnomaly` is recognized by the corruption retry loop
//! in `thread.rs`. When detected, it increments `corruption_attempt` and can
//! trigger model fallback after `MAX_CORRUPTION_RETRY_ATTEMPTS`.

use std::collections::HashSet;

/// Expected edit scope estimated from the user's prompt.
#[derive(Debug, Clone)]
pub struct ExpectedEditScope {
    /// Estimated max number of files that should be touched.
    /// `None` means unlimited (scope couldn't be confidently estimated).
    pub expected_file_count: Option<usize>,
    /// Estimated max number of lines that should change.
    /// `None` means unlimited (scope couldn't be confidently estimated).
    pub expected_line_count: Option<usize>,
}

/// Actual edit scope observed from tool results.
#[derive(Debug, Clone, Default)]
pub struct ActualEditScope {
    /// Number of distinct files that were edited.
    pub file_count: usize,
    /// Number of lines added + removed.
    pub line_count: usize,
}

/// Result of comparing expected vs actual scope.
#[derive(Debug, Clone)]
pub struct ScopeAnomalyResult {
    /// Whether the actual scope exceeds expected scope.
    pub is_anomaly: bool,
    /// Ratio of actual/expected line count (f32::INFINITY if expected was 0 or None).
    pub line_ratio: f32,
    /// Ratio of actual/expected file count (f32::INFINITY if expected was 0 or None).
    pub file_ratio: f32,
    /// Human-readable reason for the anomaly.
    pub reason: String,
}

impl ExpectedEditScope {
    /// Estimate expected edit scope from a user prompt using keyword heuristics.
    ///
    /// Returns `None` for both fields when the scope can't be confidently estimated
    /// (e.g., "refactor the authentication module"). This is intentional — false
    /// negatives are far better than false positives here.
    pub fn from_prompt(prompt: &str) -> Self {
        let prompt_lower = prompt.to_lowercase();

        // Trivial tasks: rename, fix typo, add comment, change variable
        if prompt_lower.contains("rename")
            || prompt_lower.contains("fix typo")
            || prompt_lower.contains("add comment")
            || prompt_lower.contains("change variable")
            || prompt_lower.contains("update variable")
        {
            return Self {
                expected_file_count: Some(1),
                expected_line_count: Some(10),
            };
        }

        // Small fixes: fix bug, add import, update config, change default
        if prompt_lower.contains("fix bug")
            || prompt_lower.contains("add import")
            || prompt_lower.contains("update config")
            || prompt_lower.contains("change default")
            || prompt_lower.contains("add parameter")
            || prompt_lower.contains("update parameter")
        {
            return Self {
                expected_file_count: Some(2),
                expected_line_count: Some(30),
            };
        }

        // Medium features: add feature, implement, refactor
        if prompt_lower.contains("add feature")
            || prompt_lower.contains("implement")
            || prompt_lower.contains("refactor")
        {
            return Self {
                expected_file_count: Some(5),
                expected_line_count: Some(200),
            };
        }

        // Large/unknown: rewrite, migrate, long prompt, or no recognized signals
        // Default to unlimited — we can't confidently say this should be small
        Self {
            expected_file_count: None,
            expected_line_count: None,
        }
    }

    /// Estimate expected edit scope from an agent plan (more accurate than prompt).
    ///
    /// If the agent has already produced a plan (e.g., "I'll edit parser.rs line 84"),
    /// extract file names and line references from the plan text.
    pub fn from_agent_plan(plan_text: &str) -> Self {
        let mut file_mentions = HashSet::new();
        let mut line_count_estimate = 0usize;

        // Look for file path mentions (e.g., `parser.rs`, `src/auth/login.rs`)
        // Simple heuristic: words ending in common file extensions
        for word in plan_text.split_whitespace() {
            let word = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '/');
            if word.ends_with(".rs")
                || word.ends_with(".ts")
                || word.ends_with(".js")
                || word.ends_with(".py")
                || word.ends_with(".go")
                || word.ends_with(".java")
                || word.ends_with(".c")
                || word.ends_with(".cpp")
                || word.ends_with(".h")
                || word.ends_with(".tsx")
                || word.ends_with(".jsx")
                || word.contains('/')
            {
                file_mentions.insert(word.to_string());
            }
        }

        // Look for line number mentions (e.g., "line 84", "lines 10-15")
        let plan_lower = plan_text.to_lowercase();
        if let Some(line_idx) = plan_lower.find("line") {
            // Try to extract a number after "line"
            let after_line = &plan_lower[line_idx + 4..];
            for word in after_line.split_whitespace().take(2) {
                let word = word.trim_matches(|c: char| !c.is_numeric());
                if let Ok(num) = word.parse::<usize>() {
                    line_count_estimate = line_count_estimate.max(num);
                }
                // Handle ranges like "10-15"
                if let Some(dash_idx) = word.find('-') {
                    if let Ok(end) = word[dash_idx + 1..].parse::<usize>() {
                        line_count_estimate = line_count_estimate.max(end);
                    }
                }
            }
        }

        // If we found file mentions, use those
        let expected_file_count = if file_mentions.is_empty() {
            None
        } else {
            Some(file_mentions.len())
        };

        // If we found line references, use those; otherwise estimate based on file count
        let expected_line_count = if line_count_estimate > 0 {
            Some(line_count_estimate)
        } else if let Some(file_count) = expected_file_count {
            // Rough estimate: 50 lines per file if no specific line mentions
            Some(file_count * 50)
        } else {
            None
        };

        Self {
            expected_file_count,
            expected_line_count,
        }
    }
}

impl ActualEditScope {
    /// Build actual edit scope from tool results.
    ///
    /// Examines tool results for `edit_file` and `write_file` tools, extracts
    /// the diff and input_path from the serialized `EditSessionOutput`, and
    /// counts lines changed and distinct files touched.
    pub fn from_tool_results(
        tool_results: impl Iterator<Item = (String, serde_json::Value)>,
    ) -> Self {
        let mut files_touched = HashSet::new();
        let mut total_line_count = 0usize;

        for (tool_name, output) in tool_results {
            // Only look at edit_file and write_file tools
            if tool_name != "edit_file" && tool_name != "write_file" {
                continue;
            }

            // Extract input_path from the output
            if let Some(input_path) = output.get("input_path").and_then(|v| v.as_str()) {
                files_touched.insert(input_path.to_string());
            }

            // Extract diff and count lines changed
            if let Some(diff) = output.get("diff").and_then(|v| v.as_str()) {
                total_line_count += count_diff_lines(diff);
            }
        }

        Self {
            file_count: files_touched.len(),
            line_count: total_line_count,
        }
    }
}

/// Count the number of lines changed in a unified diff (lines starting with + or -).
/// Excludes diff headers (lines starting with @@, ---, +++, etc.)
fn count_diff_lines(diff: &str) -> usize {
    diff.lines()
        .filter(|line| {
            (line.starts_with('+') || line.starts_with('-'))
                && !line.starts_with("+++")
                && !line.starts_with("---")
                && !line.starts_with("@@")
        })
        .count()
}

/// Compare expected vs actual scope and determine if there's an anomaly.
pub fn detect_scope_anomaly(
    expected: &ExpectedEditScope,
    actual: &ActualEditScope,
) -> ScopeAnomalyResult {
    // If we couldn't estimate scope, don't flag as anomaly
    let (Some(expected_files), Some(expected_lines)) =
        (expected.expected_file_count, expected.expected_line_count)
    else {
        return ScopeAnomalyResult {
            is_anomaly: false,
            line_ratio: f32::INFINITY,
            file_ratio: f32::INFINITY,
            reason: "scope could not be estimated".to_string(),
        };
    };

    // Calculate ratios
    let file_ratio = if expected_files == 0 {
        if actual.file_count > 0 {
            f32::INFINITY
        } else {
            1.0
        }
    } else {
        actual.file_count as f32 / expected_files as f32
    };

    let line_ratio = if expected_lines == 0 {
        if actual.line_count > 0 {
            f32::INFINITY
        } else {
            1.0
        }
    } else {
        actual.line_count as f32 / expected_lines as f32
    };

    // Determine if this is an anomaly
    // Thresholds: 3x files or 5x lines are considered anomalous
    // These are conservative to avoid false positives
    const FILE_RATIO_THRESHOLD: f32 = 3.0;
    const LINE_RATIO_THRESHOLD: f32 = 5.0;

    let is_anomaly = file_ratio >= FILE_RATIO_THRESHOLD || line_ratio >= LINE_RATIO_THRESHOLD;

    let reason = if is_anomaly {
        format!(
            "edit scope exceeded expectations: {} files touched (expected ~{}, ratio {:.1}x), \
             {} lines changed (expected ~{}, ratio {:.1}x)",
            actual.file_count, expected_files, file_ratio,
            actual.line_count, expected_lines, line_ratio,
        )
    } else {
        format!(
            "edit scope within expectations: {} files (expected ~{}), {} lines (expected ~{})",
            actual.file_count, expected_files, actual.line_count, expected_lines,
        )
    };

    ScopeAnomalyResult {
        is_anomaly,
        line_ratio,
        file_ratio,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_prompt_trivial_rename() {
        let scope = ExpectedEditScope::from_prompt("rename variable x to y");
        assert_eq!(scope.expected_file_count, Some(1));
        assert_eq!(scope.expected_line_count, Some(10));
    }

    #[test]
    fn test_from_prompt_fix_typo() {
        let scope = ExpectedEditScope::from_prompt("Fix typo in README");
        assert_eq!(scope.expected_file_count, Some(1));
        assert_eq!(scope.expected_line_count, Some(10));
    }

    #[test]
    fn test_from_prompt_small_fix() {
        let scope = ExpectedEditScope::from_prompt("fix bug in parser");
        assert_eq!(scope.expected_file_count, Some(2));
        assert_eq!(scope.expected_line_count, Some(30));
    }

    #[test]
    fn test_from_prompt_medium_feature() {
        let scope = ExpectedEditScope::from_prompt("implement user authentication");
        assert_eq!(scope.expected_file_count, Some(5));
        assert_eq!(scope.expected_line_count, Some(200));
    }

    #[test]
    fn test_from_prompt_unknown_scope() {
        let scope = ExpectedEditScope::from_prompt("refactor the authentication module");
        assert_eq!(scope.expected_file_count, None);
        assert_eq!(scope.expected_line_count, None);
    }

    #[test]
    fn test_from_agent_plan_with_files() {
        let plan = "I'll edit parser.rs and update config.toml";
        let scope = ExpectedEditScope::from_agent_plan(plan);
        assert_eq!(scope.expected_file_count, Some(2));
        assert!(scope.expected_line_count.is_some());
    }

    #[test]
    fn test_from_agent_plan_with_lines() {
        let plan = "I'll modify parser.rs around line 84";
        let scope = ExpectedEditScope::from_agent_plan(plan);
        assert_eq!(scope.expected_file_count, Some(1));
        assert_eq!(scope.expected_line_count, Some(84));
    }

    #[test]
    fn test_count_diff_lines() {
        let diff = r#"--- a/file.rs
+++ b/file.rs
@@ -10,7 +10,8 @@
 context line
-old line
+new line
+added line
 context line
"#;
        assert_eq!(count_diff_lines(diff), 3); // 1 removed + 2 added
    }

    #[test]
    fn test_detect_scope_anomaly_within_limits() {
        let expected = ExpectedEditScope {
            expected_file_count: Some(2),
            expected_line_count: Some(50),
        };
        let actual = ActualEditScope {
            file_count: 2,
            line_count: 40,
        };
        let result = detect_scope_anomaly(&expected, &actual);
        assert!(!result.is_anomaly);
    }

    #[test]
    fn test_detect_scope_anomaly_exceeded() {
        let expected = ExpectedEditScope {
            expected_file_count: Some(1),
            expected_line_count: Some(10),
        };
        let actual = ActualEditScope {
            file_count: 7,
            line_count: 1200,
        };
        let result = detect_scope_anomaly(&expected, &actual);
        assert!(result.is_anomaly);
        assert!(result.reason.contains("exceeded expectations"));
    }

    #[test]
    fn test_detect_scope_anomaly_unlimited() {
        let expected = ExpectedEditScope {
            expected_file_count: None,
            expected_line_count: None,
        };
        let actual = ActualEditScope {
            file_count: 100,
            line_count: 5000,
        };
        let result = detect_scope_anomaly(&expected, &actual);
        assert!(!result.is_anomaly);
    }
}
