use crate::error::{Error, Result};
use jiff::Timestamp;
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub(crate) struct Action {
    pub value: Value,
    pub line: usize,
}

pub(crate) fn parse_jsonl(input: &str) -> Result<Vec<Action>> {
    let mut actions = Vec::new();
    for (idx, line) in input.split_terminator('\n').enumerate() {
        let line_no = idx + 1;
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() {
            return Err(Error::invalid_action(
                line_no,
                "blank lines are not allowed",
            ));
        }
        let value: Value = serde_json::from_str(line)
            .map_err(|e| Error::invalid_action(line_no, format!("invalid JSON: {e}")))?;
        if !value.is_object() {
            return Err(Error::invalid_action(
                line_no,
                "each action must be a JSON object",
            ));
        }
        actions.push(Action {
            value,
            line: line_no,
        });
    }
    Ok(actions)
}

pub(crate) fn complete_actions(actions: &mut [Action], author: &Value) -> Result<()> {
    for action in actions {
        let object = action.value.as_object_mut().ok_or_else(|| {
            Error::invalid_action(action.line, "each action must be a JSON object")
        })?;
        if !object.contains_key("id") {
            object.insert("id".into(), Value::String(crate::new_id()));
        }
        if !object.contains_key("author") {
            object.insert("author".into(), author.clone());
        }
        if !object.contains_key("created_at") {
            object.insert(
                "created_at".into(),
                Value::String(Timestamp::now().to_string()),
            );
        }
    }
    Ok(())
}

pub(crate) fn validate_write_actions<F>(
    actions: &[Action],
    old_actions: &[Value],
    reviewed: &HashSet<String>,
    mut anchor_ok: F,
) -> Result<()>
where
    F: FnMut(&str, &str, u32, u32) -> Result<bool>,
{
    let mut ids = HashSet::new();
    let mut known_targets: HashMap<String, String> = HashMap::new();
    for value in old_actions {
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            if valid_uuid7(id) {
                ids.insert(id.to_owned());
                if let Some(kind) = value.get("type").and_then(Value::as_str) {
                    if kind == "comment" || kind == "reply" {
                        known_targets.insert(id.to_owned(), kind.to_owned());
                    }
                }
            }
        }
    }
    for action in actions {
        let object = action.value.as_object().expect("parse_jsonl objects");
        let id = required_string(object, "id", action.line)?;
        if !valid_uuid7(id) {
            return Err(Error::invalid_action(
                action.line,
                "id must be a UUID version 7",
            ));
        }
        if !ids.insert(id.to_owned()) {
            return Err(Error::new(
                "duplicate_id",
                format!(
                    "line {} uses an ID already present on the branch or in this input",
                    action.line
                ),
                json!({"line": action.line, "id": id}),
            ));
        }
        let kind = required_string(object, "type", action.line)?;
        if !matches!(kind, "comment" | "reply" | "delete") {
            return Err(Error::invalid_action(
                action.line,
                format!("unknown action type {kind:?}"),
            ));
        }
        validate_author(object.get("author"), action.line)?;
        validate_timestamp(object.get("created_at"), action.line)?;
        match kind {
            "comment" => {
                let commit = required_string(object, "commit", action.line)?;
                if !reviewed.contains(commit) {
                    return Err(Error::new(
                        "commit_not_reviewed",
                        format!(
                            "line {} comments on {}, which is not a reviewed commit",
                            action.line, commit
                        ),
                        json!({"line": action.line, "commit": commit}),
                    ));
                }
                if object.get("body").and_then(Value::as_str).is_none() {
                    return Err(Error::invalid_action(
                        action.line,
                        "comment body must be a string",
                    ));
                }
                if let Some(anchor) = object.get("anchor") {
                    let anchor = anchor.as_object().ok_or_else(|| {
                        Error::invalid_action(action.line, "anchor must be an object")
                    })?;
                    let path = required_string(anchor, "path", action.line)?;
                    let start = required_u32(anchor, "start_line", action.line)?;
                    let end = required_u32(anchor, "end_line", action.line)?;
                    if start == 0 || end < start || !anchor_ok(commit, path, start, end)? {
                        return Err(Error::new(
                            "bad_anchor",
                            format!("line {} has an invalid anchor in {path:?}", action.line),
                            json!({"line": action.line, "commit": commit, "path": path, "start_line": start, "end_line": end}),
                        ));
                    }
                }
                known_targets.insert(id.to_owned(), "comment".into());
            }
            "reply" => {
                let target = required_string(object, "in_reply_to", action.line)?;
                if !matches!(
                    known_targets.get(target).map(String::as_str),
                    Some("comment" | "reply")
                ) {
                    return Err(Error::new(
                        "unknown_target",
                        format!(
                            "line {} replies to unknown comment or reply {target}",
                            action.line
                        ),
                        json!({"line": action.line, "target": target}),
                    ));
                }
                if object.get("body").and_then(Value::as_str).is_none() {
                    return Err(Error::invalid_action(
                        action.line,
                        "reply body must be a string",
                    ));
                }
                known_targets.insert(id.to_owned(), "reply".into());
            }
            "delete" => {
                let target = required_string(object, "target", action.line)?;
                if !matches!(
                    known_targets.get(target).map(String::as_str),
                    Some("comment" | "reply")
                ) {
                    return Err(Error::new(
                        "unknown_target",
                        format!(
                            "line {} deletes unknown comment or reply {target}",
                            action.line
                        ),
                        json!({"line": action.line, "target": target}),
                    ));
                }
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}

pub(crate) fn valid_uuid7(value: &str) -> bool {
    Uuid::parse_str(value)
        .map(|id| id.get_version_num() == 7)
        .unwrap_or(false)
}

pub(crate) fn validate_author(value: Option<&Value>, line: usize) -> Result<()> {
    let author = value
        .and_then(Value::as_object)
        .ok_or_else(|| Error::invalid_action(line, "author must be an object"))?;
    if author.get("name").and_then(Value::as_str).is_none() {
        return Err(Error::invalid_action(line, "author.name must be a string"));
    }
    if author.get("email").is_some() && author.get("email").and_then(Value::as_str).is_none() {
        return Err(Error::invalid_action(line, "author.email must be a string"));
    }
    Ok(())
}

pub(crate) fn validate_timestamp(value: Option<&Value>, line: usize) -> Result<()> {
    let text = value.and_then(Value::as_str).ok_or_else(|| {
        Error::invalid_action(line, "created_at must be an RFC 3339 timestamp in UTC")
    })?;
    if !(text.ends_with('Z') || text.ends_with("+00:00") || text.ends_with("-00:00"))
        || Timestamp::from_str(text).is_err()
    {
        return Err(Error::invalid_action(
            line,
            "created_at must be an RFC 3339 timestamp in UTC",
        ));
    }
    Ok(())
}

pub(crate) fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    line: usize,
) -> Result<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::invalid_action(line, format!("{field} must be a string")))
}

pub(crate) fn required_u32(object: &Map<String, Value>, field: &str, line: usize) -> Result<u32> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            Error::invalid_action(line, format!("{field} must be an unsigned 32-bit integer"))
        })
}
