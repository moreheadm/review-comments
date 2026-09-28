#[derive(Clone, Debug, Default)]
pub struct CommitOptions {
    pub branch: String,
    pub reviewed: Vec<String>,
    pub review_parents: Vec<String>,
    pub create: bool,
    /// `Some("none")` represents the explicit `--expect-tip none` CLI value.
    pub expect_tip: Option<String>,
    pub force: bool,
    pub author: Option<String>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ShowOptions {
    pub branch: String,
    pub at: Option<String>,
    pub path: Option<String>,
    pub since: Option<String>,
    pub include_deleted: bool,
}
