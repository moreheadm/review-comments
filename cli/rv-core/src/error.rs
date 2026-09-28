use serde_json::{json, Value};
use std::{error::Error as StdError, fmt};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct Error {
    pub code: String,
    pub message: String,
    pub details: Value,
}

impl Error {
    pub fn new(code: impl Into<String>, message: impl Into<String>, details: Value) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details,
        }
    }

    pub fn simple(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, message, json!({}))
    }

    pub fn exit_code(&self) -> i32 {
        match self.code.as_str() {
            "usage" => 2,
            "invalid_action"
            | "duplicate_id"
            | "commit_not_reviewed"
            | "bad_anchor"
            | "unknown_target"
            | "not_a_review_commit"
            | "empty_commit" => 3,
            "branch_not_found" | "revision_not_found" | "ambiguous_revision" => 4,
            "tip_moved" | "would_orphan" => 5,
            "not_a_repository" | "git_failed" | "jj_failed" => 6,
            _ => 1,
        }
    }

    pub fn git(message: impl Into<String>) -> Self {
        Self::simple("git_failed", message)
    }

    pub fn invalid_action(line: usize, message: impl Into<String>) -> Self {
        Self::new("invalid_action", message, json!({"line": line}))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error[{}]: {}", self.code, self.message)
    }
}
impl StdError for Error {}
