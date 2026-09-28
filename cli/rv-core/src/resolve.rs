use crate::{
    error::{Error, Result},
    git::{stderr_text, Git},
};
use std::{path::Path, process::Command};

pub(crate) fn resolve_revision(root: &Path, revision: &str) -> Result<String> {
    let values = resolve_revisions(root, revision)?;
    if values.len() != 1 {
        return Err(Error::new(
            "ambiguous_revision",
            format!("revision {revision:?} must resolve to exactly one commit"),
            serde_json::json!({"revision": revision, "commits": values}),
        ));
    }
    Ok(values.into_iter().next().expect("one revision"))
}

pub(crate) fn resolve_revisions(root: &Path, revision: &str) -> Result<Vec<String>> {
    if revision.is_empty() {
        return Err(revision_not_found(revision));
    }
    if jj_available(root) {
        resolve_jj(root, revision)
    } else {
        resolve_git(root, revision)
    }
}

fn jj_available(root: &Path) -> bool {
    Command::new("jj")
        .arg("root")
        .current_dir(root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn resolve_jj(root: &Path, revision: &str) -> Result<Vec<String>> {
    let output = Command::new("jj")
        .args([
            "log",
            "-r",
            revision,
            "--no-graph",
            "--reversed",
            "-T",
            "commit_id ++ \"\\n\"",
        ])
        .current_dir(root)
        .output()
        .map_err(|e| {
            Error::new(
                "jj_failed",
                format!("could not run jj: {e}"),
                serde_json::json!({"revision": revision}),
            )
        })?;
    if !output.status.success() {
        let stderr = stderr_text(&output.stderr);
        let code = if stderr.to_ascii_lowercase().contains("diverg")
            || stderr.to_ascii_lowercase().contains("ambiguous")
        {
            "ambiguous_revision"
        } else if stderr.to_ascii_lowercase().contains("no such")
            || stderr.to_ascii_lowercase().contains("not found")
            || stderr.to_ascii_lowercase().contains("doesn't exist")
        {
            "revision_not_found"
        } else {
            "jj_failed"
        };
        return Err(Error::new(
            code,
            format!("jj could not resolve {revision:?}: {stderr}"),
            serde_json::json!({"revision": revision}),
        ));
    }
    parse_oids(&output.stdout, revision)
}

/// Review-history IDs are Git objects, deliberately not imported into jj.
pub(crate) fn resolve_git_commit(root: &Path, revision: &str) -> Result<String> {
    let values = resolve_git(root, revision)?;
    if values.len() != 1 {
        return Err(Error::new(
            "ambiguous_revision",
            "expected exactly one Git commit",
            serde_json::json!({"revision":revision}),
        ));
    }
    Ok(values.into_iter().next().expect("one commit"))
}

fn resolve_git(root: &Path, revision: &str) -> Result<Vec<String>> {
    let git = Git::new(root);
    if revision.contains("..") {
        let output = git.output(["rev-list", "--reverse", "--end-of-options", revision])?;
        if !output.status.success() {
            return Err(classify_git_revision(revision, &output.stderr));
        }
        return parse_oids(&output.stdout, revision);
    }
    let spec = format!("{revision}^{{commit}}");
    let output = git.output(["rev-parse", "--verify", "--end-of-options", spec.as_str()])?;
    if !output.status.success() {
        return Err(classify_git_revision(revision, &output.stderr));
    }
    parse_oids(&output.stdout, revision)
}

fn classify_git_revision(revision: &str, stderr: &[u8]) -> Error {
    let text = stderr_text(stderr);
    let code = if text.to_ascii_lowercase().contains("ambiguous") {
        "ambiguous_revision"
    } else {
        "revision_not_found"
    };
    Error::new(
        code,
        format!("revision {revision:?} could not be resolved: {text}"),
        serde_json::json!({"revision": revision}),
    )
}

fn parse_oids(output: &[u8], revision: &str) -> Result<Vec<String>> {
    let text = std::str::from_utf8(output)
        .map_err(|_| Error::git("revision command returned non-UTF-8 output"))?;
    let values: Vec<_> = text
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if values.is_empty() {
        return Err(revision_not_found(revision));
    }
    if values
        .iter()
        .any(|id| id.len() != 40 && id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(Error::git("revision command returned an invalid object ID"));
    }
    Ok(values.into_iter().map(str::to_owned).collect())
}

fn revision_not_found(revision: &str) -> Error {
    Error::new(
        "revision_not_found",
        format!("revision {revision:?} did not resolve to a commit"),
        serde_json::json!({"revision": revision}),
    )
}
