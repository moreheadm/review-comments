use rv_core::{new_id, CommitOptions, Repository, ShowOptions};
use serde_json::{json, Value};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

fn git(root: &Path, args: &[&str], input: &str) -> String {
    let mut child = Command::new("git")
        .current_dir(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end_matches('\n')
        .to_owned()
}
fn fixture() -> (tempfile::TempDir, Repository, String) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();
    git(path, &["init", "-q"], "");
    git(path, &["config", "user.name", "Test"], "");
    git(path, &["config", "user.email", "test@example.com"], "");
    std::fs::write(path.join("file"), "one\ntwo\n").unwrap();
    git(path, &["add", "."], "");
    git(path, &["commit", "-qm", "code"], "");
    let base = git(path, &["rev-parse", "HEAD"], "");
    let repo = Repository::open(path).unwrap();
    (temp, repo, base)
}
fn tree(root: &Path, filename: &str, content: &str) -> String {
    let blob = git(root, &["hash-object", "-w", "--stdin"], content);
    let reviews = git(
        root,
        &["mktree"],
        &format!("100644 blob {blob}\t{filename}\n"),
    );
    git(
        root,
        &["mktree"],
        &format!("040000 tree {reviews}\treviews\n"),
    )
}
fn raw_commit(root: &Path, tree: &str, parents: &[&str], header: Option<&str>) -> String {
    let mut body = format!("tree {tree}\n");
    for parent in parents {
        body += &format!("parent {parent}\n");
    }
    body += "author Test <test@example.com> 1700000000 +0000\ncommitter Test <test@example.com> 1700000000 +0000\n";
    if let Some(header) = header {
        body += &format!("review-parents {header}\n");
    }
    body += "\nfixture\n";
    git(
        root,
        &["hash-object", "-t", "commit", "-w", "--stdin"],
        &body,
    )
}
fn common(mut action: Value) -> Value {
    action["id"] = json!(new_id());
    action["author"] = json!({"name":"Test"});
    action["created_at"] = json!("2026-09-27T12:00:00Z");
    action
}
fn check_rule(root: &Path, repo: &Repository, commit: &str, rule: &str) {
    git(root, &["update-ref", "refs/reviews/bad", commit], "");
    let result = repo.check("bad").unwrap();
    assert_eq!(result["ok"], false, "{rule}: {result}");
    assert!(
        result["violations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["rule"] == rule),
        "missing {rule}: {result}"
    );
}

#[test]
fn check_reports_each_storage_invariant() {
    let (temp, repo, base) = fixture();
    let root = temp.path();
    let comment = common(json!({"type":"comment","commit":base,"body":"valid"}));
    let filename = format!("{}.jsonl", new_id());
    let valid_tree = tree(root, &filename, &(comment.to_string() + "\n"));
    let valid = raw_commit(root, &valid_tree, &[&base], Some(""));
    check_rule(
        root,
        &repo,
        &raw_commit(root, &valid_tree, &[&base], None),
        "V1",
    );
    check_rule(
        root,
        &repo,
        &raw_commit(root, &valid_tree, &[&base], Some(&valid)),
        "V2",
    );
    check_rule(
        root,
        &repo,
        &raw_commit(root, &valid_tree, &[&base], Some(&base)),
        "V3",
    );
    check_rule(
        root,
        &repo,
        &raw_commit(root, &valid_tree, &[&base, &base], Some("")),
        "V4",
    );
    let changed_tree = tree(
        root,
        &filename,
        &(common(json!({"type":"comment","commit":base,"body":"replacement"})).to_string() + "\n"),
    );
    check_rule(
        root,
        &repo,
        &raw_commit(root, &changed_tree, &[&valid], Some(&valid)),
        "V5",
    );
    let invalid_name = tree(root, "not-a-uuid.jsonl", &(comment.to_string() + "\n"));
    check_rule(
        root,
        &repo,
        &raw_commit(root, &invalid_name, &[&base], Some("")),
        "V6",
    );
    let malformed = tree(root, &filename, "not JSON\n");
    check_rule(
        root,
        &repo,
        &raw_commit(root, &malformed, &[&base], Some("")),
        "V7",
    );
    let dangling = common(json!({"type":"reply","in_reply_to":new_id(),"body":"orphan"}));
    let dangling_tree = tree(root, &filename, &(dangling.to_string() + "\n"));
    check_rule(
        root,
        &repo,
        &raw_commit(root, &dangling_tree, &[&base], Some("")),
        "V8",
    );
    let anchor = common(
        json!({"type":"comment","commit":base,"body":"bad","anchor":{"path":"missing","start_line":1,"end_line":1}}),
    );
    let anchor_tree = tree(root, &filename, &(anchor.to_string() + "\n"));
    check_rule(
        root,
        &repo,
        &raw_commit(root, &anchor_tree, &[&base], Some("")),
        "V9",
    );
}

#[test]
fn check_requires_known_action_fields_and_jsonl_termination() {
    let (temp, repo, base) = fixture();
    let root = temp.path();
    let filename = format!("{}.jsonl", new_id());
    for payload in [
        json!({"type":"comment"}),
        json!({"type":"comment","commit":base}),
        json!({"type":"comment","commit":"HEAD","body":"bad oid"}),
        json!({"type":"reply","in_reply_to":new_id()}),
        json!({"type":"reply","body":"missing target"}),
        json!({"type":"delete"}),
    ] {
        let content = common(payload).to_string() + "\n";
        let bad_tree = tree(root, &filename, &content);
        let bad = raw_commit(root, &bad_tree, &[&base], Some(""));
        check_rule(root, &repo, &bad, "V7");
        let error = repo
            .commit(
                CommitOptions {
                    branch: "bad".into(),
                    reviewed: vec![base.clone()],
                    ..Default::default()
                },
                "",
            )
            .unwrap_err();
        assert_eq!(error.code, "not_a_review_commit");
    }
    let future = common(json!({"type":"future-type","payload":true}));
    let no_lf = tree(root, &filename, &future.to_string());
    check_rule(
        root,
        &repo,
        &raw_commit(root, &no_lf, &[&base], Some("")),
        "V7",
    );
    let valid_tree = tree(root, &filename, &(future.to_string() + "\n"));
    let valid = raw_commit(root, &valid_tree, &[&base], Some(""));
    git(root, &["update-ref", "refs/reviews/future", &valid], "");
    assert_eq!(repo.check("future").unwrap()["ok"], true);
}

#[test]
fn malformed_actions_are_skipped_and_orphan_trees_are_not_duplicated() {
    let (temp, repo, base) = fixture();
    let valid = common(json!({"type":"comment","commit":base,"body":"keep"}));
    let invalid = common(
        json!({"type":"comment","commit":base,"body":"bad","anchor":{"path":"missing","start_line":1,"end_line":1}}),
    );
    let orphan = common(json!({"type":"reply","in_reply_to":new_id(),"body":"root orphan"}));
    let child = common(json!({"type":"reply","in_reply_to":orphan["id"],"body":"child orphan"}));
    let unknown = common(json!({"type":"future-action","payload":true}));
    let lines = format!("bad JSON\n{valid}\n{invalid}\n{orphan}\n{child}\n{unknown}\n");
    let review_tree = tree(temp.path(), &format!("{}.jsonl", new_id()), &lines);
    let commit = raw_commit(temp.path(), &review_tree, &[&base], Some(""));
    git(
        temp.path(),
        &["update-ref", "refs/reviews/test", &commit],
        "",
    );
    let output = repo
        .show(ShowOptions {
            branch: "test".into(),
            at: Some(base),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(output["threads"].as_array().unwrap().len(), 1);
    assert_eq!(output["threads"][0]["body"], "keep");
    assert_eq!(output["orphans"].as_array().unwrap().len(), 1);
    assert_eq!(output["orphans"][0]["replies"][0]["body"], "child orphan");
}

#[test]
fn writer_rejects_duplicate_parents_and_cross_branch_action_ids() {
    let (_temp, repo, base) = fixture();
    let action = common(json!({"type":"comment","commit":base,"body":"same ID"})).to_string();
    let mut left = None;
    for branch in ["left", "right"] {
        let result = repo
            .commit(
                CommitOptions {
                    branch: branch.into(),
                    create: true,
                    reviewed: vec![base.clone()],
                    ..Default::default()
                },
                &action,
            )
            .unwrap();
        if branch == "left" {
            left = result["commit"].as_str().map(str::to_owned);
        }
    }
    let duplicate_parent = repo
        .commit(
            CommitOptions {
                branch: "left".into(),
                reviewed: vec![left.unwrap()],
                ..Default::default()
            },
            "",
        )
        .unwrap_err();
    assert_eq!(duplicate_parent.code, "invalid_action");
    let merge = repo
        .commit(
            CommitOptions {
                branch: "left".into(),
                review_parents: vec!["left".into(), "right".into()],
                ..Default::default()
            },
            "",
        )
        .unwrap_err();
    assert_eq!(merge.code, "duplicate_id");
    assert_eq!(repo.check("left").unwrap()["ok"], true);
}

#[test]
fn merged_log_keeps_shared_ancestors_after_all_children() {
    let (temp, repo, base) = fixture();
    let first = repo
        .commit(
            CommitOptions {
                branch: "main".into(),
                create: true,
                reviewed: vec![base.clone()],
                ..Default::default()
            },
            "",
        )
        .unwrap();
    let ancestor = first["commit"].as_str().unwrap();
    git(
        temp.path(),
        &["update-ref", "refs/reviews/other", ancestor],
        "",
    );
    repo.commit(
        CommitOptions {
            branch: "main".into(),
            reviewed: vec![base.clone()],
            message: Some("left".into()),
            ..Default::default()
        },
        "",
    )
    .unwrap();
    repo.commit(
        CommitOptions {
            branch: "other".into(),
            reviewed: vec![base],
            message: Some("right".into()),
            ..Default::default()
        },
        "",
    )
    .unwrap();
    repo.commit(
        CommitOptions {
            branch: "main".into(),
            review_parents: vec!["main".into(), "other".into()],
            ..Default::default()
        },
        "",
    )
    .unwrap();
    let log = repo.log("main").unwrap();
    let commits = log["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 4);
    assert_eq!(commits.last().unwrap()["commit"], ancestor);
}
