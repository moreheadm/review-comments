mod read;

use crate::{
    error::{Error, Result},
    git::{stderr_text, CatFile, Git},
    model::CommitOptions,
    resolve::{resolve_revision, resolve_revisions},
    validate,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const REVIEW_PREFIX: &str = "refs/reviews/";
const MAX_CAS_ATTEMPTS: usize = 5;

#[derive(Clone, Debug)]
pub(super) struct TreeEntry {
    pub(super) mode: String,
    pub(super) kind: String,
    pub(super) oid: String,
    pub(super) path: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(super) struct CommitObject {
    pub(super) oid: String,
    pub(super) tree: String,
    pub(super) parents: Vec<String>,
    pub(super) review_header: Option<String>,
    pub(super) author: Option<String>,
    pub(super) change_id: Option<String>,
}

pub(crate) fn branches(root: &Path) -> Result<Value> {
    read::branches(root)
}
pub(crate) fn log(root: &Path, branch: &str) -> Result<Value> {
    read::log(root, branch)
}
pub(crate) fn show(root: &Path, options: crate::model::ShowOptions) -> Result<Value> {
    read::show(root, options)
}
pub(crate) fn check(root: &Path, branch: &str) -> Result<Value> {
    read::check(root, branch)
}

pub(super) fn read_object(cat: &mut CatFile, oid: &str, expected: Option<&str>) -> Result<Vec<u8>> {
    let (kind, body) = cat.get(oid)?;
    if let Some(expected) = expected {
        if kind != expected {
            return Err(Error::git(format!(
                "object {oid} has type {kind}, expected {expected}"
            )));
        }
    }
    Ok(body)
}

pub(super) fn read_commit(cat: &mut CatFile, oid: &str) -> Result<CommitObject> {
    let body = read_object(cat, oid, Some("commit"))?;
    let separator = body
        .windows(2)
        .position(|w| w == b"\n\n")
        .ok_or_else(|| Error::git(format!("malformed commit object {oid}")))?;
    let headers = std::str::from_utf8(&body[..separator])
        .map_err(|_| Error::git(format!("non-UTF-8 commit headers in {oid}")))?;
    let mut tree = None;
    let mut parents = Vec::new();
    let mut review_header = None;
    let mut author = None;
    let mut change_id = None;
    for line in headers.lines() {
        if line.starts_with(' ') {
            continue;
        }
        let (key, value) = match line.split_once(' ') {
            Some(parts) => parts,
            None => (line, ""),
        };
        match key {
            "tree" => tree = Some(value.to_owned()),
            "parent" => parents.push(value.to_owned()),
            "review-parents" => review_header = Some(value.to_owned()),
            "author" => author = Some(value.to_owned()),
            "change-id" => change_id = Some(value.to_owned()),
            _ => {}
        }
    }
    Ok(CommitObject {
        oid: oid.to_owned(),
        tree: tree.ok_or_else(|| Error::git(format!("commit {oid} has no tree header")))?,
        parents,
        review_header,
        author,
        change_id,
    })
}

pub(super) fn parse_tree(cat: &mut CatFile, oid: &str, oid_len: usize) -> Result<Vec<TreeEntry>> {
    let body = read_object(cat, oid, Some("tree"))?;
    let mut entries = Vec::new();
    let mut pos = 0;
    while pos < body.len() {
        let mode_end = body[pos..]
            .iter()
            .position(|b| *b == b' ')
            .map(|p| pos + p)
            .ok_or_else(|| Error::git(format!("malformed tree object {oid}")))?;
        let name_start = mode_end + 1;
        let name_end = body[name_start..]
            .iter()
            .position(|b| *b == 0)
            .map(|p| name_start + p)
            .ok_or_else(|| Error::git(format!("malformed tree object {oid}")))?;
        let oid_start = name_end + 1;
        let oid_end = oid_start
            .checked_add(oid_len / 2)
            .filter(|end| *end <= body.len())
            .ok_or_else(|| Error::git(format!("truncated tree object {oid}")))?;
        let mode_raw = std::str::from_utf8(&body[pos..mode_end])
            .map_err(|_| Error::git(format!("invalid tree mode in {oid}")))?;
        let mode = if mode_raw == "40000" {
            "040000".to_owned()
        } else {
            mode_raw.to_owned()
        };
        let kind = (if mode == "040000" {
            "tree"
        } else if mode == "160000" {
            "commit"
        } else {
            "blob"
        })
        .to_owned();
        entries.push(TreeEntry {
            mode,
            kind,
            oid: hex(&body[oid_start..oid_end]),
            path: body[name_start..name_end].to_vec(),
        });
        pos = oid_end;
    }
    Ok(entries)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

pub(super) fn map_entries(entries: Vec<TreeEntry>) -> BTreeMap<Vec<u8>, TreeEntry> {
    entries
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect()
}

pub(super) fn root_entries(
    cat: &mut CatFile,
    commit: &CommitObject,
    oid_len: usize,
) -> Result<Vec<TreeEntry>> {
    parse_tree(cat, &commit.tree, oid_len)
}

pub(super) fn reviews_entries(
    cat: &mut CatFile,
    commit: &CommitObject,
    oid_len: usize,
) -> Result<BTreeMap<Vec<u8>, TreeEntry>> {
    let roots = root_entries(cat, commit, oid_len)?;
    match roots.iter().find(|entry| entry.path == b"reviews") {
        Some(entry) if entry.kind == "tree" => {
            Ok(map_entries(parse_tree(cat, &entry.oid, oid_len)?))
        }
        Some(_) => Err(Error::git(format!(
            "commit {} has a non-tree reviews entry",
            commit.oid
        ))),
        None => Ok(BTreeMap::new()),
    }
}

pub(super) fn tree_path_entry(
    cat: &mut CatFile,
    commit: &CommitObject,
    oid_len: usize,
    path: &str,
) -> Result<Option<TreeEntry>> {
    let bytes = path.as_bytes();
    if bytes.is_empty()
        || bytes.starts_with(b"/")
        || bytes
            .split(|b| *b == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Ok(None);
    }
    let parts: Vec<&[u8]> = bytes.split(|b| *b == b'/').collect();
    let mut current_tree = commit.tree.clone();
    for (idx, part) in parts.iter().enumerate() {
        let entries = parse_tree(cat, &current_tree, oid_len)?;
        let Some(entry) = entries.into_iter().find(|entry| entry.path == *part) else {
            return Ok(None);
        };
        if idx + 1 == parts.len() {
            return Ok(Some(entry));
        }
        if entry.kind != "tree" {
            return Ok(None);
        }
        current_tree = entry.oid;
    }
    Ok(None)
}

pub(super) fn line_count(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    let newline_count = bytes.iter().filter(|b| **b == b'\n').count() as u64;
    if bytes.last() == Some(&b'\n') {
        newline_count
    } else {
        newline_count + 1
    }
}

pub(super) fn anchor_valid(
    cat: &mut CatFile,
    oid_len: usize,
    commit_id: &str,
    path: &str,
    start: u32,
    end: u32,
) -> Result<bool> {
    let commit = read_commit(cat, commit_id)?;
    let Some(entry) = tree_path_entry(cat, &commit, oid_len, path)? else {
        return Ok(false);
    };
    if entry.kind != "blob" {
        return Ok(false);
    }
    let bytes = read_object(cat, &entry.oid, Some("blob"))?;
    Ok(start > 0 && end >= start && end as u64 <= line_count(&bytes))
}

pub(super) fn encode_tree_input(entries: &BTreeMap<Vec<u8>, TreeEntry>) -> Vec<u8> {
    let mut values: Vec<&TreeEntry> = entries.values().collect();
    values.sort_by(|a, b| tree_sort_key(&a.path).cmp(&tree_sort_key(&b.path)));
    let mut bytes = Vec::new();
    for entry in values {
        bytes.extend_from_slice(entry.mode.as_bytes());
        bytes.push(b' ');
        bytes.extend_from_slice(entry.kind.as_bytes());
        bytes.push(b' ');
        bytes.extend_from_slice(entry.oid.as_bytes());
        bytes.push(b'\t');
        bytes.extend_from_slice(&entry.path);
        bytes.push(0);
    }
    bytes
}

fn tree_sort_key(path: &[u8]) -> Vec<u8> {
    let mut result = path.to_vec();
    result.push(b'/');
    result
}

pub(super) fn check_ref(git: &Git, branch: &str) -> Result<String> {
    if branch.is_empty() {
        return Err(Error::new(
            "usage",
            "a review branch name is required",
            json!({"branch": branch}),
        ));
    }
    let reference = format!("{REVIEW_PREFIX}{branch}");
    let output = git.output(["check-ref-format", reference.as_str()])?;
    if !output.status.success() {
        return Err(Error::new(
            "usage",
            format!("invalid review branch name {branch:?}"),
            json!({"branch": branch}),
        ));
    }
    Ok(reference)
}

pub(super) fn tip(git: &Git, reference: &str) -> Result<Option<String>> {
    let output = git.output(["rev-parse", "--verify", "--quiet", reference])?;
    if output.status.success() {
        let oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if oid.is_empty() {
            return Err(Error::git("git returned an empty ref value"));
        }
        Ok(Some(oid))
    } else if output.status.code() == Some(1) {
        Ok(None)
    } else {
        Err(Error::git(format!(
            "could not read ref {reference}: {}",
            stderr_text(&output.stderr)
        )))
    }
}

fn resolve_review_parent(root: &Path, value: &str) -> Result<String> {
    if value.is_empty() {
        return Err(Error::new(
            "not_a_review_commit",
            "review parent cannot be empty",
            json!({}),
        ));
    }
    let git = Git::new(root);
    let refname = format!("{REVIEW_PREFIX}{value}");
    if let Some(found) = tip(&git, &refname)? {
        return Ok(found);
    }
    let spec = format!("{value}^{{commit}}");
    let output = git.output([
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        spec.as_str(),
    ])?;
    if !output.status.success() {
        return Err(Error::new(
            "not_a_review_commit",
            format!("review parent {value:?} is not a review branch or commit"),
            json!({"review_parent": value}),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn parse_identity(value: &str) -> Option<(&str, &str)> {
    let open = value.find('<')?;
    let close = value[open + 1..].find('>')? + open + 1;
    Some((value[..open].trim_end(), value[open + 1..close].trim()))
}

fn identity(root: &Path, override_author: Option<&str>) -> Result<(String, String)> {
    let text = if let Some(value) = override_author {
        value.to_owned()
    } else if let Ok(value) = std::env::var("RV_AUTHOR") {
        value
    } else {
        let output = Git::new(root).run(["var", "GIT_AUTHOR_IDENT"])?;
        String::from_utf8_lossy(&output).trim().to_owned()
    };
    let (name, email) = parse_identity(&text).ok_or_else(|| {
        Error::new(
            "usage",
            "author must have the form Name <email>",
            json!({"author": text}),
        )
    })?;
    if name.is_empty()
        || email.is_empty()
        || name.chars().any(|c| matches!(c, '\n' | '\r' | '<' | '>'))
        || email.chars().any(|c| matches!(c, '\n' | '\r' | '<' | '>'))
    {
        return Err(Error::new(
            "usage",
            "author must have the form Name <email>",
            json!({"author": text}),
        ));
    }
    Ok((name.to_owned(), email.to_owned()))
}

fn action_author(name: &str, email: &str) -> Value {
    json!({"name": name, "email": email})
}

fn resolved_reviewed(root: &Path, args: &[String]) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for argument in args {
        for oid in resolve_revisions(root, argument)? {
            if seen.insert(oid.clone()) {
                result.push(oid);
            }
        }
    }
    Ok(result)
}

pub(super) fn parent_review_entries(
    cat: &mut CatFile,
    parents: &[String],
    oid_len: usize,
) -> Result<(BTreeMap<Vec<u8>, TreeEntry>, BTreeMap<Vec<u8>, TreeEntry>)> {
    let mut reviews: BTreeMap<Vec<u8>, TreeEntry> = BTreeMap::new();
    let mut top: BTreeMap<Vec<u8>, TreeEntry> = BTreeMap::new();
    for parent in parents {
        let commit = read_commit(cat, parent)?;
        for entry in root_entries(cat, &commit, oid_len)? {
            if entry.path == b"reviews" {
                continue;
            }
            if let Some(previous) = top.get(&entry.path) {
                if previous.oid != entry.oid
                    || previous.kind != entry.kind
                    || previous.mode != entry.mode
                {
                    return Err(Error::new(
                        "not_a_review_commit",
                        "review parents have conflicting top-level tree entries",
                        json!({"path": String::from_utf8_lossy(&entry.path)}),
                    ));
                }
            } else {
                top.insert(entry.path.clone(), entry);
            }
        }
        for (path, entry) in reviews_entries(cat, &commit, oid_len)? {
            if let Some(previous) = reviews.get(&path) {
                if previous.oid != entry.oid
                    || previous.kind != entry.kind
                    || previous.mode != entry.mode
                {
                    return Err(Error::new(
                        "not_a_review_commit",
                        "review parents contain conflicting review files",
                        json!({"path": String::from_utf8_lossy(&path)}),
                    ));
                }
            } else {
                reviews.insert(path, entry);
            }
        }
    }
    Ok((reviews, top))
}

fn collect_values(cat: &mut CatFile, entries: &BTreeMap<Vec<u8>, TreeEntry>) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    for entry in entries.values() {
        if entry.kind != "blob" {
            continue;
        }
        let content = read_object(cat, &entry.oid, Some("blob"))?;
        for line in content
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
        {
            if let Ok(value) = serde_json::from_slice::<Value>(line) {
                if value.is_object() {
                    values.push(value);
                }
            }
        }
    }
    Ok(values)
}

fn check_review_commit_valid(root: &Path, oid: &str) -> Result<()> {
    let git = Git::new(root);
    let mut cat = git.cat_file()?;
    let violations = read::check_graph(&git, &mut cat, oid, git.oid_len()?)?;
    if let Some(first) = violations.first() {
        return Err(Error::new(
            "not_a_review_commit",
            format!(
                "review parent {oid} is invalid: {}",
                first["detail"].as_str().unwrap_or("storage rule violation")
            ),
            json!({"review_parent": oid, "violation": first}),
        ));
    }
    Ok(())
}

pub(crate) fn commit(root: &Path, options: CommitOptions, input: &str) -> Result<Value> {
    let git = Git::new(root);
    let reference = check_ref(&git, &options.branch)?;
    let mut actions = validate::parse_jsonl(input)?;
    let (author_name, author_email) = identity(root, options.author.as_deref())?;
    validate::complete_actions(&mut actions, &action_author(&author_name, &author_email))?;
    let reviewed = resolved_reviewed(root, &options.reviewed)?;
    let reviewed_set: HashSet<String> = reviewed.iter().cloned().collect();
    let explicit_parent_values = !options.review_parents.is_empty();
    let mut explicit_parents = Vec::new();
    let mut parent_seen = HashSet::new();
    for value in &options.review_parents {
        let oid = resolve_review_parent(root, value)?;
        if parent_seen.insert(oid.clone()) {
            explicit_parents.push(oid);
        }
    }
    if reviewed.iter().any(|oid| parent_seen.contains(oid)) {
        return Err(Error::new(
            "invalid_action",
            "a commit cannot be both a review parent and a reviewed parent",
            json!({}),
        ));
    }
    let expected_tip = match options.expect_tip.as_deref() {
        Some("none") => Some(None),
        Some(value) => Some(Some(crate::resolve::resolve_git_commit(root, value)?)),
        None => None,
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let author_ident = format!("{author_name} <{author_email}> {now} +0000");
    let mut action_file = Vec::new();
    for action in &actions {
        serde_json::to_writer(&mut action_file, &action.value).map_err(|e| {
            Error::new(
                "internal",
                format!("could not encode action: {e}"),
                json!({}),
            )
        })?;
        action_file.push(b'\n');
    }
    let blob = if actions.is_empty() {
        None
    } else {
        Some(git.hash_blob(&action_file)?)
    };
    let review_id = crate::new_id();

    for attempt in 1..=MAX_CAS_ATTEMPTS {
        let current = tip(&git, &reference)?;
        if let Some(expected) = &expected_tip {
            if current != *expected {
                return Err(Error::new(
                    "tip_moved",
                    "review branch tip does not match --expect-tip",
                    json!({"expected": expected, "actual": current}),
                ));
            }
        }
        if current.is_none() && !options.create {
            return Err(Error::new(
                "branch_not_found",
                format!(
                    "review branch {:?} does not exist (use --create to create it)",
                    options.branch
                ),
                json!({"branch": options.branch}),
            ));
        }
        let review_parents = if explicit_parent_values {
            explicit_parents.clone()
        } else {
            current.iter().cloned().collect()
        };
        if explicit_parent_values {
            if let Some(tip_oid) = current.as_deref() {
                let mut reachable = false;
                for parent in &review_parents {
                    if review_ancestor(root, parent, tip_oid)? {
                        reachable = true;
                        break;
                    }
                }
                if !options.force && !reachable {
                    return Err(Error::new(
                        "would_orphan",
                        "the current review tip is not reachable from the requested review parents",
                        json!({"current_tip": tip_oid, "review_parents": review_parents}),
                    ));
                }
            }
        }
        for parent in &review_parents {
            check_review_commit_valid(root, parent)?;
        }
        if review_parents
            .iter()
            .any(|parent| reviewed_set.contains(parent))
        {
            return Err(Error::simple(
                "invalid_action",
                "a commit cannot be both a review parent and a reviewed parent",
            ));
        }
        if actions.is_empty() && review_parents.len() < 2 && reviewed.is_empty() {
            return Err(Error::simple(
                "empty_commit",
                "empty input requires a merge or at least one reviewed commit",
            ));
        }
        let oid_len = git.oid_len()?;
        let mut cat = git.cat_file()?;
        let (mut review_entries, mut top_entries) =
            parent_review_entries(&mut cat, &review_parents, oid_len)?;
        let old_actions = collect_values(&mut cat, &review_entries)?;
        // Individually valid branches can still collide when merged. Shared
        // files were already deduplicated by tree union; duplicate action IDs
        // here therefore violate the cumulative-tree uniqueness invariant.
        let mut inherited_ids = HashSet::new();
        for action in &old_actions {
            if let Some(id) = action.get("id").and_then(Value::as_str) {
                if !inherited_ids.insert(id) {
                    return Err(Error::new(
                        "duplicate_id",
                        "review parents contain colliding action IDs",
                        json!({"id":id}),
                    ));
                }
            }
        }
        validate::validate_write_actions(
            &actions,
            &old_actions,
            &reviewed_set,
            |commit, path, start, end| anchor_valid(&mut cat, oid_len, commit, path, start, end),
        )?;
        if let Some(blob) = &blob {
            let name = format!("{review_id}.jsonl").into_bytes();
            if review_entries.contains_key(&name) {
                return Err(Error::new(
                    "duplicate_id",
                    "generated review ID already exists on the branch",
                    json!({"review_id": review_id}),
                ));
            }
            review_entries.insert(
                name.clone(),
                TreeEntry {
                    mode: "100644".into(),
                    kind: "blob".into(),
                    oid: blob.clone(),
                    path: name,
                },
            );
        }
        let reviews_tree = git.mktree(&encode_tree_input(&review_entries))?;
        top_entries.insert(
            b"reviews".to_vec(),
            TreeEntry {
                mode: "040000".into(),
                kind: "tree".into(),
                oid: reviews_tree,
                path: b"reviews".to_vec(),
            },
        );
        let root_tree = git.mktree(&encode_tree_input(&top_entries))?;
        let mut all_parents = review_parents.clone();
        all_parents.extend(reviewed.iter().cloned());
        let mut commit_data = Vec::new();
        commit_data.extend_from_slice(format!("tree {root_tree}\n").as_bytes());
        for parent in &all_parents {
            commit_data.extend_from_slice(format!("parent {parent}\n").as_bytes());
        }
        commit_data.extend_from_slice(
            format!("author {author_ident}\ncommitter {author_ident}\n").as_bytes(),
        );
        commit_data
            .extend_from_slice(format!("review-parents {}\n", review_parents.join(" ")).as_bytes());
        commit_data.push(b'\n');
        commit_data.extend_from_slice(format!("Review {review_id}").as_bytes());
        if let Some(message) = options.message.as_deref().filter(|m| !m.is_empty()) {
            commit_data.extend_from_slice(b"\n\n");
            commit_data.extend_from_slice(message.as_bytes());
        }
        commit_data.push(b'\n');
        let new_commit = git.hash_commit(&commit_data)?;
        let zero_oid = "0".repeat(oid_len);
        let old_value = current.as_deref().unwrap_or(&zero_oid);
        match git.update_ref(&reference, &new_commit, old_value) {
            Ok(()) => {
                let action_summaries: Vec<Value> = actions
                    .iter()
                    .map(|a| json!({"id": a.value.get("id"), "type": a.value.get("type")}))
                    .collect();
                return Ok(json!({
                    "rv": 1, "branch": options.branch, "commit": new_commit,
                    "previous_tip": current, "review_id": review_id,
                    "review_parents": review_parents, "reviewed": reviewed,
                    "actions": action_summaries, "attempts": attempt
                }));
            }
            Err(error) => {
                let after = tip(&git, &reference)?;
                if after != current {
                    if !explicit_parent_values
                        && expected_tip.is_none()
                        && attempt < MAX_CAS_ATTEMPTS
                    {
                        continue;
                    }
                    return Err(Error::new(
                        "tip_moved",
                        "review branch tip moved while recording the review",
                        json!({"expected": current, "actual": after}),
                    ));
                }
                return Err(error);
            }
        }
    }
    Err(Error::new(
        "tip_moved",
        "review branch moved during all compare-and-swap attempts",
        json!({"attempts": MAX_CAS_ATTEMPTS}),
    ))
}

fn review_ancestor(root: &Path, descendant: &str, ancestor: &str) -> Result<bool> {
    let git = Git::new(root);
    let mut cat = git.cat_file()?;
    let mut pending = vec![descendant.to_owned()];
    let mut seen = HashSet::new();
    while let Some(current) = pending.pop() {
        if current == ancestor {
            return Ok(true);
        }
        if !seen.insert(current.clone()) {
            continue;
        }
        let commit = read_commit(&mut cat, &current)?;
        if let Some(value) = commit.review_header {
            pending.extend(read::parse_review_parent_ids(&value));
        }
    }
    Ok(false)
}
