//! Layer 3 (Phase C) of the corruption defense system: AST validation.
//!
//! After an edit is applied to a buffer, this module checks whether the
//! edit introduced new tree-sitter parse errors. If the error count increased
//! compared to the pre-edit state, the edit is rejected.
//!
//! # Design
//!
//! ```text
//! before edit:   buffer.syntax_error_count() → pre_edit_errors
//! apply edit → reparse (in-memory) → validate
//! after edit:    buffer.syntax_error_count() → post_edit_errors
//!
//! if post_edit_errors > pre_edit_errors:
//!     → reject (EditSessionOutput::Error — model sees and can retry)
//! ```
//!
//! This approach avoids false positives from files that already had syntax
//! errors before the agent's edit — only *newly introduced* errors trigger
//! the rejection.
//!
//! # Integration with the corruption retry pipeline
//!
//! `CompletionError::AstValidationFailed` exists in `thread.rs` and is
//! recognised by the corruption retry loop, but this module currently
//! returns an `EditSessionOutput::Error` instead. Tool-level errors are
//! architecturally separate from model completion errors: the model sees
//! the tool error message and can self-correct on its next turn without
//! going through the formal retry-with-model-fallback pipeline.
//!
//! If full integration is desired (so that AST validation failures
//! increment `corruption_attempt` and can trigger model fallback), the
//! edit-file tool's `run` method would need to propagate
//! `CompletionError::AstValidationFailed` as an `anyhow::Error` rather
//! than an `EditSessionOutput::Error`. This is deferred to Phase D.

use language::BufferSnapshot;

/// Result of an AST validation check.
#[derive(Debug, Clone)]
pub struct AstValidationResult {
    /// Number of syntax errors before the edit.
    pub pre_edit_error_count: usize,
    /// Number of syntax errors after the edit.
    pub post_edit_error_count: usize,
}

impl AstValidationResult {
    /// Returns `true` if the edit introduced new syntax errors.
    pub fn has_new_errors(&self) -> bool {
        self.post_edit_error_count > self.pre_edit_error_count
    }

    /// Returns a human-readable description of the validation result.
    pub fn description(&self) -> String {
        if self.has_new_errors() {
            format!(
                "edit introduced {} new syntax error(s) ({} → {})",
                self.post_edit_error_count - self.pre_edit_error_count,
                self.pre_edit_error_count,
                self.post_edit_error_count,
            )
        } else {
            format!(
                "edit did not introduce syntax errors ({} error(s))",
                self.post_edit_error_count,
            )
        }
    }
}

/// Capture the pre-edit syntax error count from a buffer snapshot.
///
/// Call this **before** any edits are applied to the buffer. The returned
/// value should be passed to [`validate_post_edit`] after the edit is
/// saved and the buffer has been reparsed.
pub fn capture_pre_edit_error_count(snapshot: &BufferSnapshot) -> usize {
    snapshot.syntax_error_count()
}

/// Validate the buffer's syntax after an edit has been applied in-memory.
///
/// Compares the post-edit error count against the pre-edit count captured
/// by [`capture_pre_edit_error_count`]. Call this **before** saving the
/// buffer to disk so corrupted edits never reach the filesystem.
/// Returns an [`AstValidationResult`] describing the outcome.
pub fn validate_post_edit(
    pre_edit_error_count: usize,
    snapshot: &BufferSnapshot,
) -> AstValidationResult {
    let post_edit_error_count = snapshot.syntax_error_count();
    AstValidationResult {
        pre_edit_error_count,
        post_edit_error_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validation_result_no_new_errors() {
        let result = AstValidationResult {
            pre_edit_error_count: 2,
            post_edit_error_count: 2,
        };
        assert!(!result.has_new_errors());
        assert!(
            result.description().contains("did not introduce"),
            "description: {}",
            result.description()
        );
    }

    #[test]
    fn test_validation_result_with_new_errors() {
        let result = AstValidationResult {
            pre_edit_error_count: 0,
            post_edit_error_count: 3,
        };
        assert!(result.has_new_errors());
        let desc = result.description();
        assert!(desc.contains("3 new syntax error"), "description: {desc}");
        assert!(desc.contains("0 → 3"), "description: {desc}");
    }

    #[test]
    fn test_validation_result_fewer_errors() {
        // An edit that fixes errors is fine — we don't reject improvements.
        let result = AstValidationResult {
            pre_edit_error_count: 5,
            post_edit_error_count: 2,
        };
        assert!(!result.has_new_errors());
    }

    #[test]
    fn test_validation_result_zero_to_zero() {
        let result = AstValidationResult {
            pre_edit_error_count: 0,
            post_edit_error_count: 0,
        };
        assert!(!result.has_new_errors());
    }
}
