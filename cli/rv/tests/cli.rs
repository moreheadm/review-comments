use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

struct Repo(tempfile::TempDir);
impl Repo {
    fn new() -> Self {
        let repo = Self(tempfile::tempdir().unwrap());
        repo.git(&["init", "-q"]);
        repo.git(&["config", "user.name", "Test Author"]);
        repo.git(&["config", "user.email", "test@example.com"]);
        std::fs::write(repo.path().join("file.txt"), "one\ntwo\nthree\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-qm", "initial"]);
        repo
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(self.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    fn rv(&self, args: &[&str], input: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rv"))
            .current_dir(self.path())
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
        child.wait_with_output().unwrap()
    }
    fn ok(&self, args: &[&str], input: &str) -> Value {
        let output = self.rv(args, input);
        assert!(
            output.status.success(),
            "rv {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["rv"], 1);
        value
    }
}

#[test]
fn usage_errors_are_json_and_id_needs_no_repository() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rv"))
        .current_dir(dir.path())
        .args(["show", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "usage");
    let output = Command::new(env!("CARGO_BIN_EXE_rv"))
        .current_dir(dir.path())
        .args(["id", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let id: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(id["id"].as_str().unwrap().as_bytes()[14], b'7');
}

#[test]
fn full_review_loop_mapping_since_delete_and_fsck() {
    let repo = Repo::new();
    let original = repo.git(&["rev-parse", "HEAD"]);
    let body = json!({"type":"comment","commit":original,"anchor":{"path":"file.txt","start_line":2,"end_line":2},"body":"Check this line","extension":{"preserved":true}}).to_string()+"\n";
    let first = repo.ok(
        &[
            "commit",
            "-b",
            "task",
            "--create",
            "--reviewed",
            &original,
            "--json",
        ],
        &body,
    );
    let tip = first["commit"].as_str().unwrap();
    let comment = first["actions"][0]["id"].as_str().unwrap();
    let object = repo.git(&["cat-file", "-p", tip]);
    assert!(object.contains("\nreview-parents \n"));
    std::fs::write(repo.path().join("file.txt"), "zero\none\ntwo\nthree\n").unwrap();
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "insert line"]);
    let newer = repo.git(&["rev-parse", "HEAD"]);
    let shown = repo.ok(
        &[
            "show", "-b", "task", "--at", &newer, "--path", "file.txt", "--json",
        ],
        "",
    );
    assert_eq!(shown["threads"][0]["mapped"]["start_line"], 3);
    assert_eq!(shown["threads"][0]["mapped"]["status"], "exact");
    assert_eq!(shown["threads"][0]["anchor"]["commit"], original);
    let actions = format!(
        "{}\n{}\n",
        json!({"type":"reply","in_reply_to":comment,"body":"Done"}),
        json!({"type":"delete","target":comment})
    );
    repo.ok(&["commit", "-b", "task", "--json"], &actions);
    let shown = repo.ok(&["show", "-b", "task", "--since", tip, "--json"], "");
    assert_eq!(shown["threads"][0]["deleted"], true);
    assert!(shown["threads"][0].get("body").is_none());
    assert_eq!(shown["threads"][0]["replies"][0]["body"], "Done");
    assert_eq!(shown["threads"][0]["replies"][0]["new"], true);
    let restored = repo.ok(&["show", "-b", "task", "--include-deleted", "--json"], "");
    assert_eq!(restored["threads"][0]["body"], "Check this line");
    assert_eq!(repo.ok(&["check", "-b", "task", "--json"], "")["ok"], true);
    repo.ok(&["log", "-b", "task", "--json"], "");
    repo.ok(&["branches", "--json"], "");
    repo.git(&["fsck", "--strict"]);
}

#[test]
fn explicit_create_and_expect_tip_are_enforced() {
    let repo = Repo::new();
    let original = repo.git(&["rev-parse", "HEAD"]);
    let output = repo.rv(
        &["commit", "-b", "missing", "--reviewed", &original, "--json"],
        "",
    );
    assert_eq!(output.status.code(), Some(4));
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "branch_not_found");
    repo.ok(
        &[
            "commit",
            "-b",
            "task",
            "--create",
            "--reviewed",
            &original,
            "--expect-tip",
            "none",
            "--json",
        ],
        "",
    );
    let output = repo.rv(
        &[
            "commit",
            "-b",
            "task",
            "--reviewed",
            &original,
            "--expect-tip",
            "none",
            "--json",
        ],
        "",
    );
    assert_eq!(output.status.code(), Some(5));
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "tip_moved");
}

#[test]
fn invalid_input_never_creates_a_ref() {
    let repo = Repo::new();
    let original = repo.git(&["rev-parse", "HEAD"]);
    for input in ["not json\n".to_owned(), json!({"type":"coment","body":"typo"}).to_string()+"\n", json!({"type":"comment","commit":original,"body":"bad anchor","anchor":{"path":"file.txt","start_line":0,"end_line":2}}).to_string()+"\n"] {
        let output = repo.rv(&["commit", "-b", "task", "--create", "--reviewed", &original, "--json"], &input);
        assert_eq!(output.status.code(), Some(3), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["rv"], 1);
        assert!(repo.git(&["for-each-ref", "refs/reviews/"]).is_empty());
    }
}

#[test]
fn git_ranges_expand_but_show_requires_one_commit() {
    let repo = Repo::new();
    let initial = repo.git(&["rev-parse", "HEAD"]);
    for text in ["four\n", "five\n"] {
        std::fs::write(repo.path().join("extra"), text).unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-qm", "next"]);
    }
    let range = format!("{initial}..HEAD");
    let recorded = repo.ok(
        &[
            "commit",
            "-b",
            "range",
            "--create",
            "--reviewed",
            &range,
            "--json",
        ],
        "",
    );
    assert_eq!(recorded["reviewed"].as_array().unwrap().len(), 2);
    let output = repo.rv(&["show", "-b", "range", "--at", &range, "--json"], "");
    assert_eq!(output.status.code(), Some(4));
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "ambiguous_revision");
}

#[test]
fn jj_snapshots_change_ids_and_ranges() {
    if Command::new("jj").arg("--version").output().is_err() {
        eprintln!("skipping jj test: jj not installed");
        return;
    }
    let repo = Repo::new();
    let jj = |args: &[&str]| {
        let output = Command::new("jj")
            .current_dir(repo.path())
            .args([
                "--config",
                "user.name=Test",
                "--config",
                "user.email=test@example.com",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "jj {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    jj(&["git", "init", "--colocate"]);
    let oid = jj(&["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    let change = jj(&["log", "-r", "@", "--no-graph", "-T", "change_id"]);
    let input = json!({"type":"comment", "commit":oid, "body":"snapshot"}).to_string() + "\n";
    let first = repo.ok(
        &[
            "commit",
            "-b",
            "jj",
            "--create",
            "--reviewed",
            &change,
            "--json",
        ],
        &input,
    );
    assert_eq!(first["reviewed"][0], oid);
    let shown = repo.ok(&["show", "-b", "jj", "--at", "@", "--json"], "");
    assert_eq!(shown["at"], oid);
    let tip = first["commit"].as_str().unwrap();
    repo.ok(&["show", "-b", "jj", "--since", tip, "--json"], "");
    repo.ok(
        &[
            "commit",
            "-b",
            "jj",
            "--expect-tip",
            tip,
            "--reviewed",
            "@",
            "--json",
        ],
        "",
    );
    jj(&["new", "-m", "second"]);
    let ranged = repo.ok(&["commit", "-b", "jj", "--reviewed", "@-::@", "--json"], "");
    assert_eq!(ranged["reviewed"].as_array().unwrap().len(), 2);
    assert_eq!(repo.ok(&["check", "-b", "jj", "--json"], "")["ok"], true);
}

#[test]
fn text_is_plain_and_noninteractive() {
    let repo = Repo::new();
    let output = repo.rv(&["id"], "");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text.trim().len(), 36);
    assert!(!text.contains('\u{1b}'));
    let output = repo.rv(&["show", "-b", "missing"], "");
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("error[branch_not_found]:"));
}
