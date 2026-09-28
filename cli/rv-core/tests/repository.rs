use rv_core::{new_id, CommitOptions, Repository, ShowOptions};
use serde_json::{json, Value};
use std::{
    path::Path,
    process::{Command, Output},
    thread,
};
use tempfile::TempDir;

fn command(root: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run git")
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = command(root, args);
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn repo(format: Option<&str>, path: &Path) -> String {
    let mut init = Command::new("git");
    init.arg("-C").arg(path).arg("init");
    if let Some(format) = format {
        init.arg(format);
    }
    let output = init.output().expect("git init");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    git(path, &["config", "user.name", "Test Author"]);
    git(path, &["config", "user.email", "test@example.com"]);
    std::fs::create_dir_all(path.join("src")).unwrap();
    std::fs::write(path.join("src/file.rs"), "first\nsecond\nthird\n").unwrap();
    git(path, &["add", "--all"]);
    git(path, &["commit", "-m", "base"]);
    git(path, &["rev-parse", "HEAD"])
}

fn action(value: Value) -> String {
    serde_json::to_string(&value).unwrap()
}

fn review(
    repo: &Repository,
    branch: &str,
    reviewed: Vec<String>,
    input: &str,
    create: bool,
) -> Value {
    repo.commit(
        CommitOptions {
            branch: branch.into(),
            reviewed,
            create,
            author: Some("Reviewer <reviewer@example.com>".into()),
            ..Default::default()
        },
        input,
    )
    .unwrap()
}

#[test]
fn writes_valid_git_objects_preserves_fields_and_exposes_all_read_commands() {
    let temp = TempDir::new().unwrap();
    let path = temp.path();
    let unusual = "src/space quote ' newline\nλ.rs";
    std::fs::create_dir_all(path.join("src")).unwrap();
    std::fs::write(path.join(unusual), "one\ntwo\nthree\n").unwrap();
    let base = repo(None, path);
    let repository = Repository::open(path).unwrap();
    let source = format!(
        r#"{{"type":"comment","extension":{{"z":1,"a":2}},"commit":"{base}","anchor":{{"path":"{}","start_line":2,"end_line":3}},"body":"Review this"}}"#,
        unusual
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    );
    let result = review(&repository, "test", vec![base.clone()], &source, true);
    let review_id = result["review_id"].as_str().unwrap();
    assert_eq!(result["attempts"], 1);
    assert_eq!(result["reviewed"][0], base);
    assert!(result["actions"][0]["id"].as_str().is_some());

    let raw = git(
        path,
        &[
            "show",
            &format!("refs/reviews/test:reviews/{review_id}.jsonl"),
        ],
    );
    let first = raw.find("\"type\"").unwrap();
    let extension = raw.find("\"extension\"").unwrap();
    let commit = raw.find("\"commit\"").unwrap();
    let anchor = raw.find("\"anchor\"").unwrap();
    assert!(
        first < extension && extension < commit && commit < anchor,
        "field order changed: {raw}"
    );
    assert!(raw.contains("\"name\":\"Reviewer\""));
    assert!(raw.contains("\"created_at\":\"20"));

    let commit_id = result["commit"].as_str().unwrap();
    let object = git(path, &["cat-file", "-p", commit_id]);
    assert!(
        object.contains("review-parents \n"),
        "root review header did not preserve trailing space: {object:?}"
    );
    let fsck = command(path, &["fsck", "--strict"]);
    assert!(
        fsck.status.success(),
        "fsck failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
    let checked = repository.check("test").unwrap();
    assert_eq!(checked["ok"], true, "{}", checked);
    assert_eq!(checked["violations"].as_array().unwrap().len(), 0);

    let shown = repository
        .show(ShowOptions {
            branch: "test".into(),
            path: Some(unusual.into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(shown["threads"].as_array().unwrap().len(), 1);
    assert_eq!(shown["threads"][0]["anchor"]["path"], unusual);
    assert_eq!(shown["threads"][0]["anchor"]["commit"], base);
    let log = repository.log("test").unwrap();
    assert_eq!(log["commits"].as_array().unwrap().len(), 1);
    assert_eq!(log["commits"][0]["reviews"][0], review_id);
    assert_eq!(log["commits"][0]["actions"].as_array().unwrap().len(), 1);
    let branches = repository.branches().unwrap();
    assert_eq!(branches["branches"][0]["name"], "test");
    assert_eq!(branches["branches"][0]["reviews"], 1);

    let mapped = repository
        .show(ShowOptions {
            branch: "test".into(),
            at: Some(base),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(mapped["threads"][0]["mapped"]["status"], "exact");
}

#[test]
fn thread_deletes_since_and_explicit_parent_semantics() {
    let temp = TempDir::new().unwrap();
    let path = temp.path();
    let base = repo(None, path);
    let repository = Repository::open(path).unwrap();
    let comment_id = new_id();
    let comment = action(
        json!({"id":comment_id,"type":"comment","author":{"name":"Dana"},"created_at":"2026-09-27T14:06:10Z","commit":base,"body":"Question"}),
    );
    let root = review(&repository, "main", vec![base.clone()], &comment, true);
    let root_commit = root["commit"].as_str().unwrap().to_owned();

    let reply_id = new_id();
    let reply = action(
        json!({"id":reply_id,"type":"reply","author":{"name":"Agent"},"created_at":"2026-09-27T14:07:10Z","in_reply_to":comment_id,"body":"Answer"}),
    );
    let replied = review(&repository, "main", vec![], &reply, false);
    let reply_commit = replied["commit"].as_str().unwrap().to_owned();

    let deleted = action(
        json!({"id":new_id(),"type":"delete","author":{"name":"Dana"},"created_at":"2026-09-27T14:08:10Z","target":comment_id}),
    );
    review(&repository, "main", vec![], &deleted, false);
    let hidden = repository
        .show(ShowOptions {
            branch: "main".into(),
            ..Default::default()
        })
        .unwrap();
    let thread = &hidden["threads"][0];
    assert_eq!(thread["deleted"], true);
    assert!(thread.get("body").is_none());
    assert_eq!(thread["replies"][0]["body"], "Answer");
    let restored = repository
        .show(ShowOptions {
            branch: "main".into(),
            include_deleted: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(restored["threads"][0]["body"], "Question");
    let recent = repository
        .show(ShowOptions {
            branch: "main".into(),
            since: Some(root_commit.clone()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(recent["threads"].as_array().unwrap().len(), 1);
    assert_eq!(recent["threads"][0]["new"], true);
    assert_eq!(recent["threads"][0]["replies"][0]["new"], true);
    assert!(repository.check("main").unwrap()["ok"].as_bool().unwrap());
    assert_eq!(
        repository.log("main").unwrap()["commits"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // A separate valid root review does not contain main's current tip.
    let other = review(
        &repository,
        "other",
        vec![base],
        &action(
            json!({"type":"comment","commit":git(path, &["rev-parse", "HEAD"]),"body":"other"}),
        ),
        true,
    );
    // `other` reviewed a code commit but has its own review root; selecting it would orphan main.
    let orphaned = repository.commit(
        CommitOptions {
            branch: "main".into(),
            review_parents: vec![other["commit"].as_str().unwrap().into()],
            force: false,
            ..Default::default()
        },
        "",
    );
    assert_eq!(orphaned.unwrap_err().code, "would_orphan");
    let expect = repository.commit(
        CommitOptions {
            branch: "main".into(),
            expect_tip: Some("none".into()),
            ..Default::default()
        },
        "",
    );
    assert_eq!(expect.unwrap_err().code, "tip_moved");
    let _ = reply_commit;
}

#[test]
fn writer_reports_validation_codes_and_fills_missing_fields() {
    let temp = TempDir::new().unwrap();
    let path = temp.path();
    let base = repo(None, path);
    let repository = Repository::open(path).unwrap();
    let empty = repository
        .commit(
            CommitOptions {
                branch: "absent".into(),
                create: true,
                ..Default::default()
            },
            "",
        )
        .unwrap_err();
    assert_eq!(empty.code, "empty_commit");

    let unknown = repository
        .commit(
            CommitOptions {
                branch: "x".into(),
                create: true,
                ..Default::default()
            },
            &action(json!({"type":"typo"})),
        )
        .unwrap_err();
    assert_eq!(unknown.code, "invalid_action");
    let unreviewed = action(json!({"type":"comment","commit":base,"body":"bad"}));
    let unreviewed = repository
        .commit(
            CommitOptions {
                branch: "x".into(),
                create: true,
                ..Default::default()
            },
            &unreviewed,
        )
        .unwrap_err();
    assert_eq!(unreviewed.code, "commit_not_reviewed");

    let invalid_anchor = action(
        json!({"type":"comment","commit":base,"body":"bad","anchor":{"path":"src/file.rs","start_line":4,"end_line":4}}),
    );
    let invalid_anchor = repository
        .commit(
            CommitOptions {
                branch: "x".into(),
                create: true,
                reviewed: vec![base.clone()],
                ..Default::default()
            },
            &invalid_anchor,
        )
        .unwrap_err();
    assert_eq!(invalid_anchor.code, "bad_anchor");

    let unknown_target = action(json!({"type":"reply","in_reply_to":new_id(),"body":"bad"}));
    let unknown_target = repository
        .commit(
            CommitOptions {
                branch: "x".into(),
                create: true,
                ..Default::default()
            },
            &unknown_target,
        )
        .unwrap_err();
    assert_eq!(unknown_target.code, "unknown_target");

    let same = new_id();
    let duplicates = format!(
        "{}\n{}",
        action(json!({"id":same,"type":"comment","commit":base,"body":"a"})),
        action(json!({"id":same,"type":"comment","commit":base,"body":"b"}))
    );
    let duplicate = repository
        .commit(
            CommitOptions {
                branch: "x".into(),
                create: true,
                reviewed: vec![base.clone()],
                ..Default::default()
            },
            &duplicates,
        )
        .unwrap_err();
    assert_eq!(duplicate.code, "duplicate_id");

    let no_branch = repository.log("not-created").unwrap_err();
    assert_eq!(no_branch.code, "branch_not_found");
    let no_review_parent = repository
        .commit(
            CommitOptions {
                branch: "x".into(),
                create: true,
                review_parents: vec![base.clone()],
                ..Default::default()
            },
            "",
        )
        .unwrap_err();
    assert_eq!(no_review_parent.code, "not_a_review_commit");
}

#[test]
fn concurrent_writers_preserve_both_actions_and_review_history() {
    let temp = TempDir::new().unwrap();
    let path = temp.path();
    let base = repo(None, path);
    let repository = Repository::open(path).unwrap();
    let comment_id = new_id();
    let root = review(
        &repository,
        "race",
        vec![base.clone()],
        &action(json!({"id":comment_id,"type":"comment","commit":base,"body":"root"})),
        true,
    );
    let start = root["commit"].as_str().unwrap().to_owned();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut workers = Vec::new();
    for body in ["left", "right"] {
        let path = path.to_path_buf();
        let barrier = barrier.clone();
        let target = comment_id.clone();
        workers.push(thread::spawn(move || {
            let repo = Repository::open(path).unwrap();
            let input =
                action(json!({"id":new_id(),"type":"reply","in_reply_to":target,"body":body}));
            barrier.wait();
            review(&repo, "race", vec![], &input, false)
        }));
    }
    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    let log = repository.log("race").unwrap();
    assert_eq!(log["commits"].as_array().unwrap().len(), 3);
    assert_eq!(
        repository
            .show(ShowOptions {
                branch: "race".into(),
                ..Default::default()
            })
            .unwrap()["threads"][0]["replies"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_ne!(repository.branches().unwrap()["branches"][0]["tip"], start);
    assert!(repository.check("race").unwrap()["ok"].as_bool().unwrap());
}

#[test]
fn supports_sha256_object_ids_when_git_provides_them() {
    let temp = TempDir::new().unwrap();
    let path = temp.path();
    let init = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["init", "--object-format=sha256"])
        .output()
        .unwrap();
    if !init.status.success() {
        return;
    }
    git(path, &["config", "user.name", "Test Author"]);
    git(path, &["config", "user.email", "test@example.com"]);
    std::fs::write(path.join("file"), "text\n").unwrap();
    git(path, &["add", "file"]);
    git(path, &["commit", "-m", "base"]);
    let base = git(path, &["rev-parse", "HEAD"]);
    assert_eq!(base.len(), 64);
    let repository = Repository::open(path).unwrap();
    let output = review(
        &repository,
        "sha256",
        vec![base.clone()],
        &action(json!({"type":"comment","commit":base,"body":"sha256"})),
        true,
    );
    assert_eq!(output["commit"].as_str().unwrap().len(), 64);
    assert!(repository.check("sha256").unwrap()["ok"].as_bool().unwrap());
    assert!(command(path, &["fsck", "--strict"]).status.success());
}
