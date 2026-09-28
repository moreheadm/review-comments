use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Maps reviewed line ranges between commits using Git's own histogram diff.
pub struct Mapper {
    repository_root: PathBuf,
    git_dir: Option<PathBuf>,
    resolved_commits: HashMap<String, String>,
    entries: HashMap<(String, String), Option<TreeEntry>>,
    diff_cache: HashMap<(String, String), BlobDiff>,
    rename_cache: HashMap<(String, String, String), Option<String>>,
}

impl Mapper {
    pub fn new(repository_root: impl AsRef<Path>) -> Self {
        Self {
            repository_root: repository_root.as_ref().to_path_buf(),
            git_dir: None,
            resolved_commits: HashMap::new(),
            entries: HashMap::new(),
            diff_cache: HashMap::new(),
            rename_cache: HashMap::new(),
        }
    }

    pub fn map(
        &mut self,
        original_commit: &str,
        target_commit: &str,
        path: &str,
        start_line: u32,
        end_line: u32,
    ) -> std::result::Result<Value, String> {
        if start_line == 0 || end_line < start_line {
            return Err(format!("invalid line range {start_line}-{end_line}"));
        }
        let original_commit = self.resolve_commit(original_commit)?;
        let target_commit = self.resolve_commit(target_commit)?;
        let original = self
            .tree_entry(&original_commit, path)?
            .ok_or_else(|| format!("path {path:?} does not exist in commit {original_commit}"))?;

        let mut mapped_path = path.to_owned();
        let target = match self.tree_entry(&target_commit, path)? {
            Some(entry) => Some(entry),
            None => {
                let renamed = self.rename_destination(&original_commit, &target_commit, path)?;
                match renamed {
                    Some(new_path) => {
                        mapped_path = new_path;
                        self.tree_entry(&target_commit, &mapped_path)?
                    }
                    None => None,
                }
            }
        };

        let Some(target) = target else {
            return Ok(json!({"commit": target_commit, "status": "file_deleted"}));
        };

        if original.oid == target.oid {
            return Ok(mapped_location(
                &target_commit,
                &mapped_path,
                start_line,
                end_line,
                "exact",
            ));
        }

        // A submodule entry names a commit rather than a file blob. It has no
        // line-oriented contents, so a changed submodule is binary for mapping.
        if original.kind != "blob" || target.kind != "blob" {
            return Ok(json!({
                "commit": target_commit,
                "path": mapped_path,
                "status": "binary"
            }));
        }

        match self.blob_diff(&original.oid, &target.oid)? {
            BlobDiff::Binary => Ok(json!({
                "commit": target_commit,
                "path": mapped_path,
                "status": "binary"
            })),
            BlobDiff::Hunks(hunks) => {
                let touched = hunk_touches_range(&hunks, start_line, end_line);
                let deleted = all_lines_deleted(&hunks, start_line, end_line);
                if deleted {
                    return Ok(json!({
                        "commit": target_commit,
                        "path": mapped_path,
                        "status": "deleted"
                    }));
                }
                let mapped_start = map_endpoint(start_line, true, &hunks)?;
                let mapped_end = map_endpoint(end_line, false, &hunks)?;
                if mapped_start > mapped_end {
                    return Err(format!(
                        "could not map line range {start_line}-{end_line} through Git hunks"
                    ));
                }
                let status = if touched { "changed" } else { "exact" };
                Ok(mapped_location(
                    &target_commit,
                    &mapped_path,
                    mapped_start,
                    mapped_end,
                    status,
                ))
            }
        }
    }

    fn resolve_commit(&mut self, commit: &str) -> std::result::Result<String, String> {
        if let Some(oid) = self.resolved_commits.get(commit) {
            return Ok(oid.clone());
        }
        let expression = format!("{commit}^{{commit}}");
        let output = self.git(&["rev-parse", "--verify", "--end-of-options", &expression])?;
        let oid = output_text(&output.stdout)?.trim().to_owned();
        if oid.is_empty() {
            return Err(format!(
                "Git returned an empty object ID for commit {commit:?}"
            ));
        }
        self.resolved_commits.insert(commit.to_owned(), oid.clone());
        Ok(oid)
    }

    fn tree_entry(
        &mut self,
        commit: &str,
        wanted_path: &str,
    ) -> std::result::Result<Option<TreeEntry>, String> {
        let key = (commit.to_owned(), wanted_path.to_owned());
        if let Some(entry) = self.entries.get(&key) {
            return Ok(entry.clone());
        }
        let output = self.git(&["ls-tree", "-r", "-z", "--full-tree", commit])?;
        for record in output.stdout.split(|byte| *byte == 0) {
            if record.is_empty() {
                continue;
            }
            let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
                return Err("could not parse NUL-delimited git ls-tree output".into());
            };
            if &record[tab + 1..] != wanted_path.as_bytes() {
                continue;
            }
            let metadata = std::str::from_utf8(&record[..tab])
                .map_err(|_| "git ls-tree returned non-UTF-8 metadata")?;
            let mut fields = metadata.split_whitespace();
            let _mode = fields.next().ok_or("missing mode in git ls-tree output")?;
            let kind = fields.next().ok_or("missing type in git ls-tree output")?;
            let oid = fields
                .next()
                .ok_or("missing object ID in git ls-tree output")?;
            if fields.next().is_some() {
                return Err("unexpected fields in git ls-tree output".into());
            }
            let entry = Some(TreeEntry {
                kind: kind.to_owned(),
                oid: oid.to_owned(),
            });
            self.entries.insert(key, entry.clone());
            return Ok(entry);
        }
        self.entries.insert(key, None);
        Ok(None)
    }

    fn rename_destination(
        &mut self,
        original_commit: &str,
        target_commit: &str,
        path: &str,
    ) -> std::result::Result<Option<String>, String> {
        let key = (
            original_commit.to_owned(),
            target_commit.to_owned(),
            path.to_owned(),
        );
        if let Some(value) = self.rename_cache.get(&key) {
            return Ok(value.clone());
        }
        let cache_path = self.rename_cache_path(original_commit, target_commit, path);
        if let Some(entry) = read_json::<RenameCacheEntry>(&cache_path) {
            if entry.version == CACHE_VERSION
                && entry.original_commit == original_commit
                && entry.target_commit == target_commit
                && entry.source_path == path
            {
                if let Some(destination) = &entry.destination {
                    if self.tree_entry(target_commit, destination)?.is_none() {
                        // Stale or malformed cache data is a miss, never a mapping result.
                    } else {
                        self.rename_cache
                            .insert(key.clone(), Some(destination.clone()));
                        return Ok(Some(destination.clone()));
                    }
                } else {
                    self.rename_cache.insert(key, None);
                    return Ok(None);
                }
            }
        }

        let output = self.git(&[
            "diff-tree",
            "-r",
            "-z",
            "-M",
            "--name-status",
            original_commit,
            target_commit,
        ])?;
        let tokens: Vec<&[u8]> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|token| !token.is_empty())
            .collect();
        let mut destination = None;
        let mut index = 0;
        while index < tokens.len() {
            let status = std::str::from_utf8(tokens[index])
                .map_err(|_| "git diff-tree returned a non-UTF-8 status")?;
            index += 1;
            if status.starts_with('R') || status.starts_with('C') {
                if index + 1 >= tokens.len() {
                    return Err("truncated rename record from git diff-tree".into());
                }
                let old_path = tokens[index];
                let new_path = tokens[index + 1];
                index += 2;
                if old_path == path.as_bytes() && status.starts_with('R') {
                    destination = Some(String::from_utf8(new_path.to_vec()).map_err(|_| {
                        "renamed path is not valid UTF-8 and cannot be represented in mapping JSON"
                    })?);
                    break;
                }
            } else {
                if index >= tokens.len() {
                    return Err("truncated path record from git diff-tree".into());
                }
                index += 1;
            }
        }

        self.rename_cache.insert(key, destination.clone());
        let entry = RenameCacheEntry {
            version: CACHE_VERSION,
            original_commit: original_commit.to_owned(),
            target_commit: target_commit.to_owned(),
            source_path: path.to_owned(),
            destination: destination.clone(),
        };
        self.write_cache(&cache_path, &entry);
        Ok(destination)
    }

    fn blob_diff(
        &mut self,
        old_blob: &str,
        new_blob: &str,
    ) -> std::result::Result<BlobDiff, String> {
        let key = (old_blob.to_owned(), new_blob.to_owned());
        if let Some(diff) = self.diff_cache.get(&key) {
            return Ok(diff.clone());
        }
        let cache_path = self.diff_cache_path(old_blob, new_blob);
        if let Some(entry) = read_json::<DiffCacheEntry>(&cache_path) {
            if entry.version == CACHE_VERSION
                && entry.old_blob == old_blob
                && entry.new_blob == new_blob
                && valid_blob_diff(&entry.diff)
            {
                self.diff_cache.insert(key, entry.diff.clone());
                return Ok(entry.diff);
            }
        }

        let output = self.git(&[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--histogram",
            "--inter-hunk-context=0",
            "-U0",
            old_blob,
            new_blob,
        ])?;
        let diff = if is_binary_diff(&output.stdout) {
            BlobDiff::Binary
        } else {
            BlobDiff::Hunks(parse_hunks(&output.stdout)?)
        };
        self.diff_cache.insert(key, diff.clone());
        let entry = DiffCacheEntry {
            version: CACHE_VERSION,
            old_blob: old_blob.to_owned(),
            new_blob: new_blob.to_owned(),
            diff: diff.clone(),
        };
        self.write_cache(&cache_path, &entry);
        Ok(diff)
    }

    fn rename_cache_path(&mut self, original: &str, target: &str, path: &str) -> PathBuf {
        let hash = stable_hash(&[original.as_bytes(), target.as_bytes(), path.as_bytes()]);
        self.cache_directory()
            .join(format!("rename-{hash:016x}.json"))
    }

    fn diff_cache_path(&mut self, old_blob: &str, new_blob: &str) -> PathBuf {
        self.cache_directory()
            .join(format!("diff-{old_blob}-{new_blob}.json"))
    }

    fn cache_directory(&mut self) -> PathBuf {
        self.resolve_git_dir()
            .map(|path| path.join("rv").join("cache"))
            .unwrap_or_else(|| self.repository_root.join(".git").join("rv").join("cache"))
    }

    fn resolve_git_dir(&mut self) -> Option<PathBuf> {
        if let Some(path) = &self.git_dir {
            return Some(path.clone());
        }
        let output = self.git(&["rev-parse", "--absolute-git-dir"]).ok()?;
        let text = output_text(&output.stdout).ok()?;
        let path = PathBuf::from(text.trim());
        if path.as_os_str().is_empty() {
            return None;
        }
        self.git_dir = Some(path.clone());
        Some(path)
    }

    fn write_cache<T: Serialize>(&mut self, path: &Path, value: &T) {
        let Ok(bytes) = serde_json::to_vec(value) else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".tmp-{}-{suffix}", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut file = File::create(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temp, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
    }

    fn git(&self, args: &[&str]) -> std::result::Result<Output, String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repository_root)
            .env("LC_ALL", "C")
            .args(args)
            .output()
            .map_err(|error| format!("could not run git: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "git {} failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(output)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TreeEntry {
    kind: String,
    oid: String,
}

const CACHE_VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Hunk {
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum BlobDiff {
    Hunks(Vec<Hunk>),
    Binary,
}

#[derive(Serialize, Deserialize)]
struct DiffCacheEntry {
    version: u8,
    old_blob: String,
    new_blob: String,
    diff: BlobDiff,
}

#[derive(Serialize, Deserialize)]
struct RenameCacheEntry {
    version: u8,
    original_commit: String,
    target_commit: String,
    source_path: String,
    destination: Option<String>,
}

fn mapped_location(commit: &str, path: &str, start: u32, end: u32, status: &str) -> Value {
    json!({
        "commit": commit,
        "path": path,
        "start_line": start,
        "end_line": end,
        "status": status
    })
}

fn parse_hunks(diff: &[u8]) -> std::result::Result<Vec<Hunk>, String> {
    let mut hunks = Vec::new();
    for line in diff.split(|byte| *byte == b'\n') {
        if !line.starts_with(b"@@ ") {
            continue;
        }
        // File contents (and optional function context) need not be UTF-8.
        // Only the ASCII range header participates in line mapping.
        let header_end = line
            .windows(3)
            .enumerate()
            .skip(3)
            .find(|(_, bytes)| *bytes == b" @@")
            .map(|(index, _)| index + 3)
            .ok_or("malformed Git hunk header")?;
        let line = std::str::from_utf8(&line[..header_end])
            .map_err(|_| "Git returned a non-ASCII hunk range")?;
        let close = line[3..]
            .find(" @@")
            .map(|offset| offset + 3)
            .ok_or_else(|| format!("malformed Git hunk header: {line}"))?;
        let ranges = &line[3..close];
        let mut ranges = ranges.split_whitespace();
        let old = ranges
            .next()
            .ok_or_else(|| format!("missing old range in Git hunk header: {line}"))?;
        let new = ranges
            .next()
            .ok_or_else(|| format!("missing new range in Git hunk header: {line}"))?;
        if ranges.next().is_some() {
            return Err(format!("extra range data in Git hunk header: {line}"));
        }
        let (old_start, old_count) = parse_range(old, '-')?;
        let (new_start, new_count) = parse_range(new, '+')?;
        let old_start = if old_count == 0 {
            old_start.checked_add(1).ok_or("old hunk start overflow")?
        } else {
            old_start
        };
        let new_start = if new_count == 0 {
            new_start.checked_add(1).ok_or("new hunk start overflow")?
        } else {
            new_start
        };
        hunks.push(Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
        });
    }
    if !valid_hunks(&hunks) {
        return Err("Git returned invalid or unordered hunk ranges".into());
    }
    Ok(hunks)
}

fn parse_range(range: &str, sign: char) -> std::result::Result<(u32, u32), String> {
    let value = range
        .strip_prefix(sign)
        .ok_or_else(|| format!("bad {sign} range in Git hunk header: {range}"))?;
    let (start, count) = match value.split_once(',') {
        Some((start, count)) => (
            start,
            count
                .parse::<u32>()
                .map_err(|_| format!("bad hunk count: {range}"))?,
        ),
        None => (value, 1),
    };
    let start = start
        .parse::<u32>()
        .map_err(|_| format!("bad hunk start: {range}"))?;
    Ok((start, count))
}

fn is_binary_diff(diff: &[u8]) -> bool {
    diff.split(|byte| *byte == b'\n').any(|line| {
        line.starts_with(b"Binary files ")
            || line.starts_with(b"Binary file ")
            || line == b"GIT binary patch"
    })
}

fn valid_blob_diff(diff: &BlobDiff) -> bool {
    match diff {
        BlobDiff::Binary => true,
        BlobDiff::Hunks(hunks) => valid_hunks(hunks),
    }
}

fn valid_hunks(hunks: &[Hunk]) -> bool {
    let mut old_end = 0u64;
    let mut new_end = 0u64;
    for (index, hunk) in hunks.iter().enumerate() {
        if hunk.old_start == 0 || hunk.new_start == 0 {
            return false;
        }
        let old_start = u64::from(hunk.old_start);
        let new_start = u64::from(hunk.new_start);
        if index > 0 && (old_start < old_end || new_start < new_end) {
            return false;
        }
        old_end = old_start + u64::from(hunk.old_count);
        new_end = new_start + u64::from(hunk.new_count);
    }
    true
}

fn hunk_touches_range(hunks: &[Hunk], start: u32, end: u32) -> bool {
    let start = u64::from(start);
    let end = u64::from(end);
    hunks.iter().any(|hunk| {
        let old_start = u64::from(hunk.old_start);
        let old_end = old_start + u64::from(hunk.old_count);
        if hunk.old_count == 0 {
            // Insertions touch only when they land between two anchored lines.
            start < old_start && old_start <= end
        } else {
            old_start <= end && start < old_end
        }
    })
}

fn all_lines_deleted(hunks: &[Hunk], start: u32, end: u32) -> bool {
    let range_len = u64::from(end) - u64::from(start) + 1;
    let start = u64::from(start);
    let end = u64::from(end);
    let deleted = hunks
        .iter()
        .filter(|hunk| hunk.new_count == 0 && hunk.old_count > 0)
        .map(|hunk| {
            let low = start.max(u64::from(hunk.old_start));
            let high = end.min(u64::from(hunk.old_start) + u64::from(hunk.old_count) - 1);
            high.saturating_sub(low)
                .saturating_add(u64::from(high >= low))
        })
        .sum::<u64>();
    deleted == range_len
}

fn map_endpoint(line: u32, first: bool, hunks: &[Hunk]) -> std::result::Result<u32, String> {
    let line64 = i64::from(line);
    let mut offset = 0i64;
    for hunk in hunks {
        let old_start = i64::from(hunk.old_start);
        let old_end = old_start + i64::from(hunk.old_count);
        if line64 < old_start {
            return shifted(line64, offset);
        }
        if line64 < old_end {
            if hunk.new_count > 0 {
                let mapped = if first {
                    i64::from(hunk.new_start)
                } else {
                    i64::from(hunk.new_start) + i64::from(hunk.new_count) - 1
                };
                return to_line(mapped);
            }
            // A deleted leading endpoint maps to the first surviving line
            // after the gap; a deleted trailing endpoint maps to the line
            // immediately before it. All-deleted ranges are classified first.
            let mapped = if first {
                i64::from(hunk.new_start)
            } else {
                i64::from(hunk.new_start) - 1
            };
            return to_line(mapped);
        }
        offset = i64::from(hunk.new_start) + i64::from(hunk.new_count) - old_end;
    }
    shifted(line64, offset)
}

fn shifted(line: i64, offset: i64) -> std::result::Result<u32, String> {
    to_line(line + offset)
}

fn to_line(line: i64) -> std::result::Result<u32, String> {
    u32::try_from(line)
        .ok()
        .filter(|line| *line > 0)
        .ok_or_else(|| "Git hunk mapping produced a line outside the valid 1-based range".into())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn stable_hash(parts: &[&[u8]]) -> u64 {
    // Stable FNV-1a across invocations; cache records still verify the full key.
    let mut hash = 0xcbf29ce484222325u64;
    for part in parts {
        for byte in (part.len() as u64).to_le_bytes().iter().chain(part.iter()) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn output_text(bytes: &[u8]) -> std::result::Result<&str, String> {
    std::str::from_utf8(bytes).map_err(|_| "Git returned non-UTF-8 output".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestRepo(PathBuf);

    impl TestRepo {
        fn new() -> Self {
            let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("rv-map-test-{}-{id}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            run_git(&path, &["init", "-q"]);
            run_git(&path, &["config", "user.name", "Mapper Test"]);
            run_git(&path, &["config", "user.email", "mapper@example.invalid"]);
            Self(path)
        }

        fn write(&self, path: &str, contents: &[u8]) {
            let full_path = self.0.join(path);
            fs::create_dir_all(full_path.parent().unwrap()).unwrap();
            fs::write(full_path, contents).unwrap();
        }

        fn commit(&self, message: &str) -> String {
            run_git(&self.0, &["add", "--all"]);
            run_git(&self.0, &["commit", "-q", "-m", message]);
            run_git(&self.0, &["rev-parse", "HEAD"])
        }

        fn mapper(&self) -> Mapper {
            Mapper::new(&self.0)
        }

        fn git_dir(&self) -> PathBuf {
            PathBuf::from(run_git(&self.0, &["rev-parse", "--absolute-git-dir"]))
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run_git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("could not run Git: {error}"));
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end_matches(['\n', '\r'])
            .to_owned()
    }

    fn status(value: &Value) -> &str {
        value["status"].as_str().unwrap()
    }

    #[test]
    fn parses_hunk_shapes_and_normalizes_zero_counts() {
        assert!(!is_binary_diff(
            b"+Binary files is a string in a text file\n"
        ));
        assert!(is_binary_diff(b"Binary files a and b differ\n"));
        let regular = b"@@ -1 +1 @@ function\n@@ -2,3 +4,2 @@\n\\ No newline at end of file\n";
        assert_eq!(
            parse_hunks(regular).unwrap(),
            vec![
                Hunk {
                    old_start: 1,
                    old_count: 1,
                    new_start: 1,
                    new_count: 1
                },
                Hunk {
                    old_start: 2,
                    old_count: 3,
                    new_start: 4,
                    new_count: 2
                },
            ]
        );
        assert_eq!(
            parse_hunks(b"@@ -9,0 +10 @@\n").unwrap(),
            vec![Hunk {
                old_start: 10,
                old_count: 0,
                new_start: 10,
                new_count: 1
            }]
        );
        assert_eq!(
            parse_hunks(b"@@ -10 +9,0 @@\n").unwrap(),
            vec![Hunk {
                old_start: 10,
                old_count: 1,
                new_start: 10,
                new_count: 0
            }]
        );
    }

    #[test]
    fn accepts_non_utf8_diff_contents_and_ignores_interhunk_configuration() {
        let hunks = parse_hunks(b"@@ -1 +1 @@ \xff\n-\xff\n+\xfe\n").unwrap();
        assert_eq!(hunks.len(), 1);
        let repo = TestRepo::new();
        repo.write("file", b"a\nb\nc\nd\ne\nf\ng\n");
        let old = repo.commit("old");
        repo.write("file", b"A\nb\nc\nd\ne\nf\nG\n");
        let new = repo.commit("new");
        run_git(&repo.0, &["config", "diff.interHunkContext", "20"]);
        let mapped = repo.mapper().map(&old, &new, "file", 4, 4).unwrap();
        assert_eq!(status(&mapped), "exact");
        assert_eq!(mapped["start_line"], 4);
    }

    #[test]
    fn maps_insertion_deletion_replacement_and_multiline_ranges() {
        let insertion = [Hunk {
            old_start: 10,
            old_count: 0,
            new_start: 10,
            new_count: 1,
        }];
        assert!(!hunk_touches_range(&insertion, 10, 10));
        assert!(hunk_touches_range(&insertion, 8, 12));
        assert_eq!(map_endpoint(20, true, &insertion).unwrap(), 21);
        assert_eq!(map_endpoint(15, true, &insertion).unwrap(), 16);

        let deletion = [Hunk {
            old_start: 3,
            old_count: 2,
            new_start: 3,
            new_count: 0,
        }];
        assert_eq!(map_endpoint(2, true, &deletion).unwrap(), 2);
        assert_eq!(map_endpoint(5, false, &deletion).unwrap(), 3);
        assert_eq!(map_endpoint(3, true, &deletion).unwrap(), 3);
        assert_eq!(map_endpoint(3, false, &deletion).unwrap(), 2);
        assert!(all_lines_deleted(&deletion, 3, 4));
        assert!(!all_lines_deleted(&deletion, 2, 4));
        assert!(hunk_touches_range(&deletion, 2, 4));

        let replacement = [Hunk {
            old_start: 4,
            old_count: 3,
            new_start: 5,
            new_count: 2,
        }];
        assert_eq!(map_endpoint(4, true, &replacement).unwrap(), 5);
        assert_eq!(map_endpoint(6, false, &replacement).unwrap(), 6);
    }

    #[test]
    fn maps_generated_git_edits_for_every_preserved_line() {
        let repo = TestRepo::new();
        let original_lines: Vec<String> = (1..=50).map(|line| format!("line-{line:02}")).collect();
        let old_commit = {
            repo.write(
                "generated.txt",
                format!("{}\n", original_lines.join("\n")).as_bytes(),
            );
            repo.commit("original generated file")
        };

        // A deterministic generated edit script: inserted lines at several
        // boundaries, deleted original lines, and a replacement at line 30.
        let mut target_lines = Vec::new();
        let mut expected = HashMap::new();
        for (index, line) in original_lines.iter().enumerate() {
            let old_line = (index + 1) as u32;
            if [5, 18, 37].contains(&old_line) {
                target_lines.push(format!("insert-before-{old_line}"));
            }
            if [8, 9, 24, 25, 26, 43].contains(&old_line) {
                continue;
            }
            if old_line == 30 {
                target_lines.push("replacement-for-line-30".to_owned());
                expected.insert(old_line, (target_lines.len() as u32, false));
            } else {
                target_lines.push(line.clone());
                expected.insert(old_line, (target_lines.len() as u32, true));
            }
        }
        let target_commit = {
            repo.write(
                "generated.txt",
                format!("{}\n", target_lines.join("\n")).as_bytes(),
            );
            repo.commit("generated edit script")
        };
        let mut mapper = repo.mapper();
        for (old_line, (new_line, unchanged)) in expected {
            let mapped = mapper
                .map(
                    &old_commit,
                    &target_commit,
                    "generated.txt",
                    old_line,
                    old_line,
                )
                .unwrap();
            assert_eq!(mapped["start_line"].as_u64(), Some(u64::from(new_line)));
            assert_eq!(mapped["end_line"].as_u64(), Some(u64::from(new_line)));
            assert_eq!(status(&mapped), if unchanged { "exact" } else { "changed" });
        }
        let multiline = mapper
            .map(&old_commit, &target_commit, "generated.txt", 20, 31)
            .unwrap();
        assert_eq!(status(&multiline), "changed");
        assert_eq!(multiline["start_line"].as_u64(), Some(20));
        assert_eq!(multiline["end_line"].as_u64(), Some(28));
    }

    #[test]
    fn handles_renames_with_unusual_paths_and_file_deletion() {
        let repo = TestRepo::new();
        let source = "dir/space ' quote\n雪.txt";
        repo.write(source, b"first\nsecond\nthird\n");
        let old_commit = repo.commit("add unusual path");
        repo.write("renamed destination.txt", b"first\nsecond\nthird\n");
        fs::remove_file(repo.0.join(source)).unwrap();
        let renamed_commit = repo.commit("rename unusual path");

        let mut mapper = repo.mapper();
        let mapped = mapper
            .map(&old_commit, &renamed_commit, source, 2, 3)
            .unwrap();
        assert_eq!(status(&mapped), "exact");
        assert_eq!(mapped["path"], "renamed destination.txt");
        assert_eq!(mapped["start_line"], 2);

        let deletion_repo = TestRepo::new();
        deletion_repo.write("gone.txt", b"a\nb\nc\nd\ne\n");
        let before = deletion_repo.commit("before delete");
        deletion_repo.write("gone.txt", b"a\ne\n");
        let after = deletion_repo.commit("delete anchor lines");
        let mut mapper = deletion_repo.mapper();
        let deleted = mapper.map(&before, &after, "gone.txt", 2, 4).unwrap();
        assert_eq!(status(&deleted), "deleted");
        assert!(deleted.get("start_line").is_none());
        let partial = mapper.map(&before, &after, "gone.txt", 1, 3).unwrap();
        assert_eq!(status(&partial), "changed");
        assert_eq!(partial["start_line"], 1);
        assert_eq!(partial["end_line"], 1);

        deletion_repo.write("gone.txt", b"a\ne\n");
        fs::remove_file(deletion_repo.0.join("gone.txt")).unwrap();
        let file_deleted_commit = deletion_repo.commit("delete file");
        let file_deleted = mapper
            .map(&before, &file_deleted_commit, "gone.txt", 1, 2)
            .unwrap();
        assert_eq!(status(&file_deleted), "file_deleted");
    }

    #[test]
    fn detects_binary_changes_and_uses_git_dir_cache_best_effort() {
        let repo = TestRepo::new();
        repo.write("data.bin", b"head\0old\n");
        let before = repo.commit("binary before");
        repo.write("data.bin", b"head\0new\n");
        let after = repo.commit("binary after");
        let mut mapper = repo.mapper();
        let mapped = mapper.map(&before, &after, "data.bin", 1, 1).unwrap();
        assert_eq!(status(&mapped), "binary");
        assert!(mapped.get("start_line").is_none());
        let cache_dir = repo.git_dir().join("rv").join("cache");
        assert!(cache_dir.is_dir());
        assert!(fs::read_dir(&cache_dir).unwrap().next().is_some());

        // Make the designated cache parent uncreatable as a directory. Mapping
        // still runs correctly and simply declines to persist cache files.
        let blocked = TestRepo::new();
        blocked.write("text.txt", b"before\n");
        let text_before = blocked.commit("text before");
        blocked.write("text.txt", b"after\n");
        let text_after = blocked.commit("text after");
        fs::write(blocked.git_dir().join("rv"), b"not a directory").unwrap();
        let mapped = blocked
            .mapper()
            .map(&text_before, &text_after, "text.txt", 1, 1)
            .unwrap();
        assert_eq!(status(&mapped), "changed");
    }

    #[test]
    fn identical_blobs_are_exact_without_line_shifting() {
        let repo = TestRepo::new();
        repo.write("plain.txt", b"one\ntwo\nthree\n");
        let old = repo.commit("first");
        repo.write("other.txt", b"unrelated\n");
        let target = repo.commit("unrelated change");
        let mapped = repo.mapper().map(&old, &target, "plain.txt", 2, 3).unwrap();
        assert_eq!(status(&mapped), "exact");
        assert_eq!(mapped["start_line"], 2);
        assert_eq!(mapped["end_line"], 3);
    }

    #[test]
    fn rejects_invalid_ranges() {
        let mut mapper = Mapper::new(".");
        assert!(mapper
            .map("x", "y", "f", 0, 0)
            .unwrap_err()
            .contains("invalid line range"));
        assert!(mapper
            .map("x", "y", "f", 2, 1)
            .unwrap_err()
            .contains("invalid line range"));
    }
}
