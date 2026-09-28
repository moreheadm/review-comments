use clap::{Parser, Subcommand};
use rv_core::{CommitOptions, Repository, ShowOptions};
use serde_json::{json, Value};
use std::io::{self, Read, Write};

#[derive(Parser)]
#[command(name = "rv", version, about = "Git-native local code reviews")]
struct Cli {
    /// Print one stable JSON object instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate JSONL actions and append a review commit.
    Commit {
        #[arg(short = 'b', long)]
        branch: String,
        /// JSONL input file, or '-' for stdin (the default).
        file: Option<String>,
        #[arg(long, action = clap::ArgAction::Append)]
        reviewed: Vec<String>,
        #[arg(long = "review-parent", action = clap::ArgAction::Append)]
        review_parents: Vec<String>,
        #[arg(long)]
        create: bool,
        #[arg(long, value_name = "COMMIT|none")]
        expect_tip: Option<String>,
        #[arg(long)]
        force: bool,
        #[arg(long, value_name = "Name <email>")]
        author: Option<String>,
        #[arg(short = 'm')]
        message: Option<String>,
    },
    /// Show comment threads, optionally placed in another commit.
    Show {
        #[arg(short = 'b', long)]
        branch: String,
        #[arg(long)]
        at: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        include_deleted: bool,
    },
    /// List review commits, newest first.
    Log {
        #[arg(short = 'b', long)]
        branch: String,
    },
    /// Check the complete review DAG against storage invariants.
    Check {
        #[arg(short = 'b', long)]
        branch: String,
    },
    /// List refs under refs/reviews/.
    Branches,
    /// Generate a UUIDv7 without opening a repository.
    Id,
}

fn emit_error(json_mode: bool, code: &str, message: &str, details: Value, exit: i32) -> ! {
    let value = json!({"rv":1,"error":{"code":code,"message":message,"details":details}});
    let mut stderr = io::stderr().lock();
    if json_mode {
        let _ = writeln!(stderr, "{value}");
    } else {
        let _ = writeln!(stderr, "error[{code}]: {message}");
    }
    std::process::exit(exit)
}

fn execute(cli: Cli) -> rv_core::Result<Value> {
    if matches!(cli.command, Command::Id) {
        return Ok(json!({"rv":1,"id":rv_core::new_id()}));
    }
    let repo = Repository::open(".")?;
    match cli.command {
        Command::Commit {
            branch,
            file,
            reviewed,
            review_parents,
            create,
            expect_tip,
            force,
            author,
            message,
        } => {
            let input = match file.as_deref() {
                None | Some("-") => {
                    let mut input = String::new();
                    if let Err(error) = io::stdin().read_to_string(&mut input) {
                        emit_error(
                            cli.json,
                            "invalid_action",
                            &format!("cannot read JSONL input: {error}"),
                            json!({}),
                            3,
                        );
                    }
                    input
                }
                Some(path) => match std::fs::read_to_string(path) {
                    Ok(input) => input,
                    Err(error) => emit_error(
                        cli.json,
                        "usage",
                        &format!("cannot read {path}: {error}"),
                        json!({"path":path}),
                        2,
                    ),
                },
            };
            repo.commit(
                CommitOptions {
                    branch,
                    reviewed,
                    review_parents,
                    create,
                    expect_tip,
                    force,
                    author,
                    message,
                },
                &input,
            )
        }
        Command::Show {
            branch,
            at,
            path,
            since,
            include_deleted,
        } => repo.show(ShowOptions {
            branch,
            at,
            path,
            since,
            include_deleted,
        }),
        Command::Log { branch } => repo.log(&branch),
        Command::Check { branch } => repo.check(&branch),
        Command::Branches => repo.branches(),
        Command::Id => unreachable!(),
    }
}

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let json_mode = args.iter().any(|arg| arg == "--json");
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) if error.exit_code() == 0 => {
            if json_mode {
                let _ = writeln!(
                    io::stdout().lock(),
                    "{}",
                    json!({"rv":1,"help":error.to_string()})
                );
                return;
            }
            error.exit();
        }
        Err(error) => emit_error(json_mode, "usage", &error.to_string(), json!({}), 2),
    };
    match execute(cli) {
        Ok(result) => {
            let text = if json_mode {
                result.to_string()
            } else {
                render(&result)
            };
            if let Err(error) = writeln!(io::stdout().lock(), "{text}") {
                // A downstream consumer closing a pipe is not a repository failure.
                if error.kind() != io::ErrorKind::BrokenPipe {
                    emit_error(json_mode, "internal", &error.to_string(), json!({}), 1);
                }
            }
        }
        Err(error) => emit_error(
            json_mode,
            &error.code,
            &error.message,
            error.details.clone(),
            error.exit_code(),
        ),
    }
}

fn string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn render(value: &Value) -> String {
    if let Some(id) = value.get("id") {
        return string(id);
    }
    if let Some(threads) = value.get("threads").and_then(Value::as_array) {
        let mut lines = vec![format!(
            "{} at {}",
            string(&value["branch"]),
            string(&value["tip"])
        )];
        if !value["at"].is_null() {
            lines.push(format!("comments placed in {}", string(&value["at"])));
        }
        for thread in threads {
            let anchor = &thread["anchor"];
            let location = thread
                .get("mapped")
                .filter(|v| v.is_object())
                .unwrap_or(anchor);
            lines.push(String::new());
            if location["path"].is_string() {
                lines.push(format!(
                    "{}:{}-{} (made on {}{}{})",
                    string(&location["path"]),
                    string(&location["start_line"]),
                    string(&location["end_line"]),
                    string(&anchor["commit"]),
                    if location.get("status").is_some() {
                        "; "
                    } else {
                        ""
                    },
                    location.get("status").map(string).unwrap_or_default()
                ));
            } else {
                lines.push(format!("{} · whole commit", string(&anchor["commit"])));
            }
            render_node(thread, 1, &mut lines);
            // Retain all anchor/mapping metadata in text, including change IDs and
            // original ranges, without imposing a second output data model.
            lines.push(format!("  anchor: {}", anchor));
            if let Some(mapped) = thread.get("mapped") {
                lines.push(format!("  mapped: {mapped}"));
            }
        }
        if let Some(orphans) = value.get("orphans").and_then(Value::as_array) {
            if !orphans.is_empty() {
                lines.push("\nOrphans:".into());
                for orphan in orphans {
                    render_node(orphan, 1, &mut lines);
                }
            }
        }
        return lines.join("\n");
    }
    if value.get("review_id").is_some() {
        return format!(
            "recorded review {} on {} as {}: {} actions, reviewed {}; {}",
            string(&value["review_id"]),
            string(&value["branch"]),
            string(&value["commit"]),
            value["actions"].as_array().map_or(0, Vec::len),
            value["reviewed"]
                .as_array()
                .map(|a| a.iter().map(string).collect::<Vec<_>>().join(", "))
                .unwrap_or_default(),
            format!(
                "previous_tip={}, review_parents={}, actions={}, attempts={}",
                value["previous_tip"], value["review_parents"], value["actions"], value["attempts"]
            )
        );
    }
    if value.get("ok").is_some() {
        if value["ok"] == true {
            return "branch is valid".into();
        }
        return value["violations"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|v| {
                format!(
                    "{} {}: {}",
                    string(&v["rule"]),
                    string(&v["commit"]),
                    string(&v["detail"])
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(branches) = value["branches"].as_array() {
        return branches
            .iter()
            .map(|b| {
                format!(
                    "{} {} ({} reviews)",
                    string(&b["name"]),
                    string(&b["tip"]),
                    b["reviews"]
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(commits) = value["commits"].as_array() {
        let mut lines = vec![format!(
            "{} at {}",
            string(&value["branch"]),
            string(&value["tip"])
        )];
        for commit in commits {
            lines.push(fields(commit));
        }
        return lines.join("\n\n");
    }
    fields(value)
}

fn fields(value: &Value) -> String {
    match value.as_object() {
        Some(object) => object
            .iter()
            .filter(|(key, _)| key.as_str() != "rv")
            .map(|(key, value)| format!("{key}: {}", string(value)))
            .collect::<Vec<_>>()
            .join("\n"),
        None => string(value),
    }
}

fn render_node(node: &Value, depth: usize, lines: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    let author = &node["author"];
    let name = author
        .get("name")
        .map(string)
        .unwrap_or_else(|| string(author));
    lines.push(format!(
        "{indent}{name}, {} [{}]{}{}",
        string(&node["created_at"]),
        string(&node["id"]),
        if node["new"] == true { " [new]" } else { "" },
        if node["deleted"] == true {
            " [deleted]"
        } else {
            ""
        }
    ));
    if let Some(email) = author.get("email") {
        lines.push(format!("{indent}  <{}>", string(email)));
    }
    if let Some(body) = node.get("body").and_then(Value::as_str) {
        for line in body.lines() {
            lines.push(format!("{indent}  {line}"));
        }
    }
    if let Some(replies) = node.get("replies").and_then(Value::as_array) {
        for reply in replies {
            render_node(reply, depth + 1, lines);
        }
    }
}
