use super::*;
use crate::model::ShowOptions;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub(super) fn parse_review_parent_ids(value: &str) -> Vec<String> {
    value.split_ascii_whitespace().map(str::to_owned).collect()
}

pub(super) fn check(root: &Path, branch: &str) -> Result<Value> {
    let git = Git::new(root);
    let reference = check_ref(&git, branch)?;
    let Some(tip_oid) = tip(&git, &reference)? else {
        return Err(Error::new(
            "branch_not_found",
            format!("review branch {branch:?} does not exist"),
            json!({"branch": branch}),
        ));
    };
    let mut cat = git.cat_file()?;
    let violations = check_graph(&git, &mut cat, &tip_oid, git.oid_len()?)?;
    Ok(json!({"rv": 1, "ok": violations.is_empty(), "violations": violations}))
}

pub(super) fn check_graph(
    git: &Git,
    cat: &mut CatFile,
    tip_oid: &str,
    oid_len: usize,
) -> Result<Vec<Value>> {
    let mut violations = Vec::new();
    let mut pending = vec![tip_oid.to_owned()];
    let mut seen = HashSet::new();
    while let Some(oid) = pending.pop() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        let commit = match read_commit(cat, &oid) {
            Ok(commit) => commit,
            Err(error) => {
                add_violation(
                    &mut violations,
                    "V3",
                    &oid,
                    format!("review parent is not a readable commit: {}", error.message),
                );
                continue;
            }
        };
        let review_parents = match commit.review_header.as_deref() {
            Some(value) => parse_review_parent_ids(value),
            None => {
                add_violation(
                    &mut violations,
                    "V1",
                    &oid,
                    "commit lacks review-parents header".into(),
                );
                Vec::new()
            }
        };
        let mut unique = HashSet::new();
        if commit
            .parents
            .iter()
            .any(|parent| !unique.insert(parent.clone()))
        {
            add_violation(
                &mut violations,
                "V4",
                &oid,
                "parent appears more than once".into(),
            );
        }
        for parent in &review_parents {
            if !commit.parents.contains(parent) {
                add_violation(
                    &mut violations,
                    "V2",
                    &oid,
                    format!("review parent {parent} is not in the parent list"),
                );
            }
            match read_commit(cat, parent) {
                Ok(parent_commit) if parent_commit.review_header.is_some() => {
                    pending.push(parent.clone())
                }
                _ => add_violation(
                    &mut violations,
                    "V3",
                    &oid,
                    format!("review parent {parent} is not a review commit"),
                ),
            }
        }
        check_commit_tree(git, cat, &commit, &review_parents, oid_len, &mut violations)?;
    }
    Ok(violations)
}

fn add_violation(violations: &mut Vec<Value>, rule: &str, commit: &str, detail: String) {
    violations.push(json!({"rule": rule, "commit": commit, "detail": detail}));
}

#[derive(Clone)]
struct StoredAction {
    value: Value,
    file: String,
    line: usize,
}

fn check_commit_tree(
    git: &Git,
    cat: &mut CatFile,
    commit: &CommitObject,
    parents: &[String],
    oid_len: usize,
    violations: &mut Vec<Value>,
) -> Result<()> {
    let current = match reviews_entries(cat, commit, oid_len) {
        Ok(entries) => entries,
        Err(error) => {
            add_violation(
                violations,
                "V5",
                &commit.oid,
                format!("cannot read reviews tree: {}", error.message),
            );
            BTreeMap::new()
        }
    };
    let inherited = match parent_review_entries(cat, parents, oid_len) {
        Ok((entries, _)) => entries,
        Err(error) => {
            add_violation(
                violations,
                "V5",
                &commit.oid,
                format!("cannot combine review-parent trees: {}", error.message),
            );
            BTreeMap::new()
        }
    };
    for (name, old) in &inherited {
        match current.get(name) {
            Some(new) if new.oid == old.oid && new.kind == old.kind && new.mode == old.mode => {}
            Some(_) => add_violation(
                violations,
                "V5",
                &commit.oid,
                format!(
                    "inherited review entry {} changed",
                    String::from_utf8_lossy(name)
                ),
            ),
            None => add_violation(
                violations,
                "V5",
                &commit.oid,
                format!(
                    "inherited review entry {} is missing",
                    String::from_utf8_lossy(name)
                ),
            ),
        }
    }
    let inherited_names: HashSet<Vec<u8>> = inherited.keys().cloned().collect();
    for (name, entry) in &current {
        if !inherited_names.contains(name) {
            let valid_name = std::str::from_utf8(name)
                .ok()
                .and_then(|s| s.strip_suffix(".jsonl"))
                .map(validate::valid_uuid7)
                .unwrap_or(false);
            if !valid_name || entry.kind != "blob" || entry.mode != "100644" {
                add_violation(
                    violations,
                    "V6",
                    &commit.oid,
                    format!(
                        "new review entry {} is not a regular UUIDv7 JSONL file",
                        String::from_utf8_lossy(name)
                    ),
                );
            }
        }
    }

    let mut actions = Vec::new();
    let mut seen_ids = HashSet::new();
    for (name, entry) in &current {
        if entry.kind != "blob" {
            continue;
        }
        let filename = String::from_utf8_lossy(name).into_owned();
        let content = match read_object(cat, &entry.oid, Some("blob")) {
            Ok(bytes) => bytes,
            Err(error) => {
                add_violation(
                    violations,
                    "V7",
                    &commit.oid,
                    format!("cannot read {filename}: {}", error.message),
                );
                continue;
            }
        };
        if std::str::from_utf8(&content).is_err() {
            add_violation(
                violations,
                "V7",
                &commit.oid,
                format!("{filename} is not UTF-8"),
            );
            continue;
        }
        let text = std::str::from_utf8(&content).expect("checked UTF-8");
        if !text.is_empty() && !text.ends_with('\n') {
            add_violation(
                violations,
                "V7",
                &commit.oid,
                format!("{filename} is missing its final line feed"),
            );
        }
        let mut lines = text.split('\n').collect::<Vec<_>>();
        if lines.last() == Some(&"") {
            lines.pop();
        }
        for (idx, line) in lines.into_iter().enumerate() {
            let parsed: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(error) => {
                    add_violation(
                        violations,
                        "V7",
                        &commit.oid,
                        format!("{filename}:{} is invalid JSON: {error}", idx + 1),
                    );
                    continue;
                }
            };
            let Some(object) = parsed.as_object() else {
                add_violation(
                    violations,
                    "V7",
                    &commit.oid,
                    format!("{filename}:{} is not an object", idx + 1),
                );
                continue;
            };
            let id = object.get("id").and_then(Value::as_str);
            let kind = object.get("type").and_then(Value::as_str);
            let common_ok = id.map(validate::valid_uuid7).unwrap_or(false)
                && kind.is_some()
                && validate::validate_author(object.get("author"), idx + 1).is_ok()
                && validate::validate_timestamp(object.get("created_at"), idx + 1).is_ok();
            if !common_ok {
                add_violation(
                    violations,
                    "V7",
                    &commit.oid,
                    format!("{filename}:{} lacks valid common action fields", idx + 1),
                );
                continue;
            }
            let required_fields_ok = match kind.unwrap() {
                "comment" => {
                    object.get("body").and_then(Value::as_str).is_some()
                        && object
                            .get("commit")
                            .and_then(Value::as_str)
                            .map(|id| {
                                id.len() == oid_len && id.bytes().all(|b| b.is_ascii_hexdigit())
                            })
                            .unwrap_or(false)
                }
                "reply" => {
                    object.get("body").and_then(Value::as_str).is_some()
                        && object.get("in_reply_to").and_then(Value::as_str).is_some()
                }
                "delete" => object.get("target").and_then(Value::as_str).is_some(),
                _ => true, // Future action types retain only common-field requirements.
            };
            if !required_fields_ok {
                add_violation(
                    violations,
                    "V7",
                    &commit.oid,
                    format!(
                        "{filename}:{} lacks required fields for {}",
                        idx + 1,
                        kind.unwrap()
                    ),
                );
                continue;
            }
            let id = id.unwrap().to_owned();
            if !seen_ids.insert(id.clone()) {
                add_violation(
                    violations,
                    "V7",
                    &commit.oid,
                    format!("action ID {id} appears more than once in the review tree"),
                );
            }
            actions.push(StoredAction {
                value: parsed,
                file: filename.clone(),
                line: idx + 1,
            });
        }
    }
    let mut target_kinds = HashMap::new();
    for action in &actions {
        let kind = action
            .value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = action.value.get("id").and_then(Value::as_str).unwrap_or("");
        if kind == "comment" || kind == "reply" {
            target_kinds.insert(id.to_owned(), kind.to_owned());
        }
    }
    for action in &actions {
        let kind = action
            .value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("");
        let object = action.value.as_object().expect("stored action object");
        let field = match kind {
            "reply" => Some("in_reply_to"),
            "delete" => Some("target"),
            _ => None,
        };
        if let Some(field) = field {
            let target = object.get(field).and_then(Value::as_str);
            if !target
                .and_then(|id| target_kinds.get(id))
                .map(|k| k == "comment" || k == "reply")
                .unwrap_or(false)
            {
                add_violation(
                    violations,
                    "V8",
                    &commit.oid,
                    format!("{}:{} has a dangling {field}", action.file, action.line),
                );
            }
        }
        if kind == "comment" {
            if let Some(anchor) = object.get("anchor") {
                let valid = (|| -> Result<bool> {
                    let Some(anchor) = anchor.as_object() else {
                        return Ok(false);
                    };
                    let Some(code_commit) = object.get("commit").and_then(Value::as_str) else {
                        return Ok(false);
                    };
                    let Some(path) = anchor.get("path").and_then(Value::as_str) else {
                        return Ok(false);
                    };
                    let Some(start) = anchor
                        .get("start_line")
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                    else {
                        return Ok(false);
                    };
                    let Some(end) = anchor
                        .get("end_line")
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                    else {
                        return Ok(false);
                    };
                    if start == 0 || end < start {
                        return Ok(false);
                    }
                    anchor_valid(cat, oid_len, code_commit, path, start, end)
                })()
                .unwrap_or(false);
                if !valid {
                    add_violation(
                        violations,
                        "V9",
                        &commit.oid,
                        format!("{}:{} has an invalid line anchor", action.file, action.line),
                    );
                }
            }
        }
    }
    let _ = git;
    Ok(())
}

pub(super) fn branches(root: &Path) -> Result<Value> {
    let git = Git::new(root);
    let output = git.run([
        "for-each-ref",
        "--format=%(refname:strip=2)%09%(objectname)",
        super::REVIEW_PREFIX,
    ])?;
    let mut cat = git.cat_file()?;
    let oid_len = git.oid_len()?;
    let mut values = Vec::new();
    for line in String::from_utf8_lossy(&output)
        .lines()
        .filter(|line| !line.is_empty())
    {
        let Some((name, oid)) = line.split_once('\t') else {
            continue;
        };
        let commit = read_commit(&mut cat, oid)?;
        let reviews = reviews_entries(&mut cat, &commit, oid_len)?.len();
        values.push(json!({"name": name, "tip": oid, "reviews": reviews}));
    }
    Ok(json!({"rv": 1, "branches": values}))
}

fn review_history(cat: &mut CatFile, tip_oid: &str) -> Result<Vec<CommitObject>> {
    let mut result = Vec::new();
    let mut pending = vec![tip_oid.to_owned()];
    let mut seen = HashSet::new();
    while let Some(oid) = pending.pop() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        let commit = read_commit(cat, &oid)?;
        if let Some(value) = &commit.review_header {
            let parents = parse_review_parent_ids(value);
            for parent in parents.iter().rev() {
                pending.push(parent.clone());
            }
        }
        result.push(commit);
    }
    // Reverse topological order keeps a shared ancestor behind both sides of
    // a merge; DFS preorder would emit it before the second side.
    let mut child_counts: HashMap<String, usize> = result
        .iter()
        .map(|commit| (commit.oid.clone(), 0))
        .collect();
    for commit in &result {
        for parent in parse_review_parent_ids(commit.review_header.as_deref().unwrap_or_default()) {
            if let Some(count) = child_counts.get_mut(&parent) {
                *count += 1;
            }
        }
    }
    let mut remaining: HashMap<String, CommitObject> = result
        .into_iter()
        .map(|commit| (commit.oid.clone(), commit))
        .collect();
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let next = remaining
            .values()
            .filter(|commit| child_counts[&commit.oid] == 0)
            .max_by_key(|commit| {
                (
                    timestamp_for_commit(commit.author.as_deref()),
                    commit.oid.clone(),
                )
            })
            .map(|commit| commit.oid.clone())
            .ok_or_else(|| Error::git("cycle in review history"))?;
        let commit = remaining.remove(&next).expect("selected commit");
        for parent in parse_review_parent_ids(commit.review_header.as_deref().unwrap_or_default()) {
            if let Some(count) = child_counts.get_mut(&parent) {
                *count -= 1;
            }
        }
        ordered.push(commit);
    }
    Ok(ordered)
}

fn added_review_entries(
    cat: &mut CatFile,
    commit: &CommitObject,
    oid_len: usize,
) -> Result<Vec<TreeEntry>> {
    let current = reviews_entries(cat, commit, oid_len)?;
    let inherited = parent_review_entries(
        cat,
        &parse_review_parent_ids(commit.review_header.as_deref().unwrap_or_default()),
        oid_len,
    )?
    .0;
    Ok(current
        .into_iter()
        .filter_map(|(name, entry)| (!inherited.contains_key(&name)).then_some(entry))
        .collect())
}

fn timestamp_for_commit(identity: Option<&str>) -> Option<String> {
    let identity = identity?;
    let close = identity.rfind('>')?;
    let seconds = identity[close + 1..]
        .split_whitespace()
        .next()?
        .parse::<i64>()
        .ok()?;
    jiff::Timestamp::from_second(seconds)
        .ok()
        .map(|t| t.to_string())
}

fn output_author(identity: Option<&str>) -> Value {
    let (name, email) = identity.and_then(super::parse_identity).unwrap_or(("", ""));
    json!({"name": name, "email": email})
}

pub(super) fn log(root: &Path, branch: &str) -> Result<Value> {
    let git = Git::new(root);
    let reference = check_ref(&git, branch)?;
    let Some(tip_oid) = tip(&git, &reference)? else {
        return Err(Error::new(
            "branch_not_found",
            format!("review branch {branch:?} does not exist"),
            json!({"branch": branch}),
        ));
    };
    let oid_len = git.oid_len()?;
    let mut cat = git.cat_file()?;
    let history = review_history(&mut cat, &tip_oid)?;
    let mut commits = Vec::new();
    for commit in history {
        let review_parents =
            parse_review_parent_ids(commit.review_header.as_deref().unwrap_or_default());
        let review_parent_set: HashSet<&str> = review_parents.iter().map(String::as_str).collect();
        let reviewed: Vec<String> = commit
            .parents
            .iter()
            .filter(|parent| !review_parent_set.contains(parent.as_str()))
            .cloned()
            .collect();
        let added = added_review_entries(&mut cat, &commit, oid_len)?;
        let mut reviews = Vec::new();
        let mut actions = Vec::new();
        for entry in added {
            let filename = String::from_utf8_lossy(&entry.path);
            if let Some(id) = filename.strip_suffix(".jsonl") {
                reviews.push(id.to_owned());
            }
            if entry.kind != "blob" {
                continue;
            }
            let body = read_object(&mut cat, &entry.oid, Some("blob"))?;
            for line in body
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
            {
                if let Ok(value) = serde_json::from_slice::<Value>(line) {
                    if value.is_object() {
                        actions.push(value);
                    }
                }
            }
        }
        commits.push(json!({
            "commit": commit.oid,
            "author": output_author(commit.author.as_deref()),
            "created_at": timestamp_for_commit(commit.author.as_deref()),
            "review_parents": review_parents,
            "reviewed": reviewed,
            "reviews": reviews,
            "actions": actions
        }));
    }
    Ok(json!({"rv": 1, "branch": branch, "tip": tip_oid, "commits": commits}))
}

#[derive(Clone)]
struct DisplayAction {
    value: Value,
    id: String,
    kind: String,
    target: Option<String>,
    new: bool,
    created_at: String,
    original_path: Option<String>,
    mapped: Option<Value>,
    anchor: Option<Value>,
}

fn common_action_valid(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let Some(id) = object.get("id").and_then(Value::as_str) else {
        return false;
    };
    if !validate::valid_uuid7(id) || object.get("type").and_then(Value::as_str).is_none() {
        return false;
    }
    let author = object.get("author").and_then(Value::as_object);
    author
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .is_some()
        && object
            .get("created_at")
            .and_then(Value::as_str)
            .map(|s| validate::validate_timestamp(Some(&Value::String(s.to_owned())), 1).is_ok())
            .unwrap_or(false)
}

pub(super) fn show(root: &Path, options: ShowOptions) -> Result<Value> {
    let git = Git::new(root);
    let reference = check_ref(&git, &options.branch)?;
    let Some(tip_oid) = tip(&git, &reference)? else {
        return Err(Error::new(
            "branch_not_found",
            format!("review branch {:?} does not exist", options.branch),
            json!({"branch": options.branch}),
        ));
    };
    let oid_len = git.oid_len()?;
    let mut cat = git.cat_file()?;
    let tip_commit = read_commit(&mut cat, &tip_oid)?;
    let entries = reviews_entries(&mut cat, &tip_commit, oid_len)?;
    let mut since_files = HashSet::new();
    if let Some(since) = options.since.as_deref() {
        let since_oid = crate::resolve::resolve_git_commit(root, since)?;
        let since_commit = read_commit(&mut cat, &since_oid)?;
        if since_commit.review_header.is_none() {
            return Err(Error::new(
                "not_a_review_commit",
                format!("--since revision {since:?} is not a review commit"),
                json!({"revision": since}),
            ));
        }
        let baseline = reviews_entries(&mut cat, &since_commit, oid_len)?;
        for name in entries.keys() {
            if !baseline.contains_key(name) {
                since_files.insert(name.clone());
            }
        }
    }
    let at = options
        .at
        .as_deref()
        .map(|revision| resolve_revision(root, revision))
        .transpose()?;
    let mut mapper = at.as_ref().map(|_| crate::map::Mapper::new(root));
    let mut changes = Vec::new();
    let mut ids = HashSet::new();
    let mut deleted_ids = HashSet::new();
    let mut new_deletions = HashSet::new();
    for (filename, entry) in &entries {
        if entry.kind != "blob" {
            continue;
        }
        let body = match read_object(&mut cat, &entry.oid, Some("blob")) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let is_new = options.since.is_some() && since_files.contains(filename);
        for line in body
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let Ok(value) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if !common_action_valid(&value) {
                continue;
            }
            let object = value
                .as_object()
                .expect("common_action_valid checks object");
            let id = object.get("id").and_then(Value::as_str).unwrap();
            if !ids.insert(id.to_owned()) {
                continue;
            }
            let kind = object.get("type").and_then(Value::as_str).unwrap();
            let created_at = object
                .get("created_at")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            match kind {
                "comment" => {
                    let Some(commit_id) = object.get("commit").and_then(Value::as_str) else {
                        continue;
                    };
                    if object.get("body").and_then(Value::as_str).is_none() {
                        continue;
                    }
                    let (anchor, original_path, start, end) = if let Some(source_anchor) =
                        object.get("anchor")
                    {
                        let Some(anchor_object) = source_anchor.as_object() else {
                            continue;
                        };
                        let Some(path) = anchor_object.get("path").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(start) = anchor_object.get("start_line").and_then(Value::as_u64)
                        else {
                            continue;
                        };
                        let Some(end) = anchor_object.get("end_line").and_then(Value::as_u64)
                        else {
                            continue;
                        };
                        if start == 0 || end < start || end > u32::MAX as u64 {
                            continue;
                        }
                        let mut anchor = source_anchor.clone();
                        anchor
                            .as_object_mut()
                            .unwrap()
                            .insert("commit".into(), Value::String(commit_id.to_owned()));
                        (anchor, Some(path.to_owned()), start as u32, end as u32)
                    } else {
                        (json!({"commit": commit_id}), None, 0, 0)
                    };
                    // A bad stored anchor must not make --at fail the entire
                    // load. Validate against the original snapshot first.
                    if let Some(path) = original_path.as_deref() {
                        if !anchor_valid(&mut cat, oid_len, commit_id, path, start, end)
                            .unwrap_or(false)
                        {
                            continue;
                        }
                    } else if commit_id.len() != oid_len
                        || !commit_id.bytes().all(|b| b.is_ascii_hexdigit())
                    {
                        continue;
                    }
                    let mapped = if let (Some(path), Some(target)) =
                        (original_path.as_deref(), at.as_deref())
                    {
                        Some(
                            mapper
                                .as_mut()
                                .unwrap()
                                .map(commit_id, target, path, start, end)
                                .map_err(|error| {
                                    Error::git(format!("line mapping failed: {error}"))
                                })?,
                        )
                    } else {
                        None
                    };
                    let change_id = read_commit(&mut cat, commit_id)
                        .ok()
                        .and_then(|c| c.change_id);
                    let mut anchor = anchor;
                    if let (Some(change_id), Some(obj)) = (change_id, anchor.as_object_mut()) {
                        obj.insert("change_id".into(), Value::String(change_id));
                    }
                    changes.push(DisplayAction {
                        value: value.clone(),
                        id: id.to_owned(),
                        kind: kind.to_owned(),
                        target: None,
                        new: is_new,
                        created_at,
                        original_path,
                        mapped,
                        anchor: Some(anchor),
                    });
                }
                "reply" => {
                    let Some(target) = object.get("in_reply_to").and_then(Value::as_str) else {
                        continue;
                    };
                    if object.get("body").and_then(Value::as_str).is_none() {
                        continue;
                    }
                    changes.push(DisplayAction {
                        value: value.clone(),
                        id: id.to_owned(),
                        kind: kind.to_owned(),
                        target: Some(target.to_owned()),
                        new: is_new,
                        created_at,
                        original_path: None,
                        mapped: None,
                        anchor: None,
                    });
                }
                "delete" => {
                    let Some(target) = object.get("target").and_then(Value::as_str) else {
                        continue;
                    };
                    deleted_ids.insert(target.to_owned());
                    if is_new {
                        new_deletions.insert(target.to_owned());
                    }
                }
                _ => {} // Unknown types remain stored but are not interpreted.
            }
        }
    }
    for action in &mut changes {
        if new_deletions.contains(&action.id) {
            action.new = true;
        }
    }

    let mut by_id = HashMap::new();
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (idx, action) in changes.iter().enumerate() {
        by_id.insert(action.id.clone(), idx);
        if action.kind == "comment" {
            roots.push(idx);
        }
    }
    for (idx, action) in changes.iter().enumerate() {
        if action.kind != "reply" {
            continue;
        }
        if let Some(parent) = action.target.as_ref().and_then(|target| by_id.get(target)) {
            if changes[*parent].kind == "comment" || changes[*parent].kind == "reply" {
                children
                    .entry(changes[*parent].id.clone())
                    .or_default()
                    .push(idx);
            }
        }
    }
    for children in children.values_mut() {
        sort_action_indices(children, &changes);
    }
    sort_action_indices(&mut roots, &changes);
    let mut visited = HashSet::new();
    let mut threads = Vec::new();
    for root_index in roots {
        let value = build_display_node(
            root_index,
            &changes,
            &children,
            &deleted_ids,
            options.include_deleted,
            &mut visited,
            true,
        );
        if options.since.is_some() && !has_new_action(&value) {
            continue;
        }
        if let Some(path) = options.path.as_deref() {
            let original = changes[root_index].original_path.as_deref();
            let mapped = changes[root_index]
                .mapped
                .as_ref()
                .and_then(|v| v.get("path"))
                .and_then(Value::as_str);
            if original != Some(path) && mapped != Some(path) {
                continue;
            }
        }
        threads.push(value);
    }
    let mut orphan_indices: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter_map(|(idx, action)| {
            (action.kind == "reply" && !visited.contains(&idx)).then_some(idx)
        })
        .collect();
    sort_action_indices(&mut orphan_indices, &changes);
    let mut orphans = Vec::new();
    for index in orphan_indices {
        if visited.contains(&index) {
            continue;
        }
        let mut orphan = build_display_node(
            index,
            &changes,
            &children,
            &deleted_ids,
            options.include_deleted,
            &mut visited,
            false,
        );
        if options.since.is_some() && !has_new_action(&orphan) {
            continue;
        }
        if let Some(obj) = orphan.as_object_mut() {
            obj.insert("orphan".into(), Value::Bool(true));
        }
        orphans.push(orphan);
    }
    let mut result = json!({"rv": 1, "branch": options.branch, "tip": tip_oid, "threads": threads, "orphans": orphans});
    if let Some(at) = at {
        result
            .as_object_mut()
            .unwrap()
            .insert("at".into(), Value::String(at));
    }
    Ok(result)
}

fn sort_action_indices(indices: &mut [usize], actions: &[DisplayAction]) {
    indices.sort_by(|a, b| {
        let a_time = actions[*a].created_at.parse::<jiff::Timestamp>().ok();
        let b_time = actions[*b].created_at.parse::<jiff::Timestamp>().ok();
        a_time
            .cmp(&b_time)
            .then_with(|| actions[*a].id.cmp(&actions[*b].id))
    });
}

fn build_display_node(
    index: usize,
    actions: &[DisplayAction],
    children: &HashMap<String, Vec<usize>>,
    deleted: &HashSet<String>,
    include_deleted: bool,
    visited: &mut HashSet<usize>,
    comment: bool,
) -> Value {
    let action = &actions[index];
    if !visited.insert(index) {
        return json!({});
    }
    let mut value = action.value.clone();
    let obj = value.as_object_mut().expect("display action is an object");
    if comment {
        if let Some(anchor) = &action.anchor {
            obj.insert("anchor".into(), anchor.clone());
        }
        if let Some(mapped) = &action.mapped {
            obj.insert("mapped".into(), mapped.clone());
        }
    }
    if deleted.contains(&action.id) {
        obj.insert("deleted".into(), Value::Bool(true));
        if !include_deleted {
            obj.remove("body");
        }
    }
    if action.new {
        obj.insert("new".into(), Value::Bool(true));
    }
    let mut replies = Vec::new();
    if let Some(indices) = children.get(&action.id) {
        for child in indices {
            if !visited.contains(child) {
                replies.push(build_display_node(
                    *child,
                    actions,
                    children,
                    deleted,
                    include_deleted,
                    visited,
                    false,
                ));
            }
        }
    }
    obj.insert("replies".into(), Value::Array(replies));
    value
}

fn has_new_action(value: &Value) -> bool {
    value.get("new").and_then(Value::as_bool) == Some(true)
        || value
            .get("replies")
            .and_then(Value::as_array)
            .map(|children| children.iter().any(has_new_action))
            .unwrap_or(false)
}
