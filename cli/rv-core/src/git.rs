use crate::error::{Error, Result};
use std::{
    ffi::OsStr,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio},
};

#[derive(Clone, Debug)]
pub(crate) struct Git {
    pub root: PathBuf,
}

impl Git {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    pub fn output<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .map_err(|e| Error::git(format!("could not run git: {e}")))
    }

    pub fn run<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.output(args)?;
        if !output.status.success() {
            return Err(Error::git(format!(
                "git failed: {}",
                stderr_text(&output.stderr)
            )));
        }
        Ok(output.stdout)
    }

    pub fn run_input<I, S>(&self, args: I, input: &[u8]) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::git(format!("could not run git: {e}")))?;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(input)
            .map_err(|e| Error::git(format!("could not write to git: {e}")))?;
        let output = child
            .wait_with_output()
            .map_err(|e| Error::git(format!("git failed: {e}")))?;
        if !output.status.success() {
            return Err(Error::git(format!(
                "git failed: {}",
                stderr_text(&output.stderr)
            )));
        }
        Ok(output.stdout)
    }

    pub fn oid_len(&self) -> Result<usize> {
        let output = self.run(["rev-parse", "--show-object-format"])?;
        match trim_lf(&output) {
            b"sha1" => Ok(40),
            b"sha256" => Ok(64),
            format => Err(Error::git(format!(
                "unsupported Git object format: {}",
                String::from_utf8_lossy(format)
            ))),
        }
    }

    pub fn hash_blob(&self, data: &[u8]) -> Result<String> {
        let out = self.run_input(["hash-object", "-w", "--stdin"], data)?;
        Ok(String::from_utf8_lossy(trim_lf(&out)).into_owned())
    }

    pub fn mktree(&self, input: &[u8]) -> Result<String> {
        let out = self.run_input(["mktree", "-z"], input)?;
        Ok(String::from_utf8_lossy(trim_lf(&out)).into_owned())
    }

    pub fn hash_commit(&self, input: &[u8]) -> Result<String> {
        let out = self.run_input(["hash-object", "-t", "commit", "-w", "--stdin"], input)?;
        Ok(String::from_utf8_lossy(trim_lf(&out)).into_owned())
    }

    pub fn update_ref(&self, name: &str, new: &str, old: &str) -> Result<()> {
        self.run(["update-ref", name, new, old])?;
        Ok(())
    }

    pub fn cat_file(&self) -> Result<CatFile> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::git(format!("could not start git cat-file --batch: {e}")))?;
        Ok(CatFile {
            stdin: child.stdin.take().expect("piped stdin"),
            stdout: BufReader::new(child.stdout.take().expect("piped stdout")),
            child,
            oid_len: self.oid_len()?,
        })
    }
}

pub(crate) struct CatFile {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    child: Child,
    oid_len: usize,
}

impl CatFile {
    pub fn get(&mut self, oid: &str) -> Result<(String, Vec<u8>)> {
        if oid.len() != self.oid_len || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::git(format!("invalid object ID: {oid}")));
        }
        self.stdin
            .write_all(oid.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| Error::git(format!("could not query git cat-file: {e}")))?;
        let mut header = Vec::new();
        self.stdout
            .read_until(b'\n', &mut header)
            .map_err(|e| Error::git(format!("could not read git cat-file response: {e}")))?;
        if header.is_empty() {
            return Err(Error::git("git cat-file --batch closed unexpectedly"));
        }
        if header.ends_with(b" missing\n") {
            return Err(Error::git(format!("Git object {oid} is missing")));
        }
        if header.last() != Some(&b'\n') {
            return Err(Error::git("malformed git cat-file header"));
        }
        let header = &header[..header.len() - 1];
        let mut fields = header.split(|b| *b == b' ');
        let returned_oid = fields.next().unwrap_or_default();
        let kind = fields.next().unwrap_or_default();
        let size = fields.next().unwrap_or_default();
        if fields.next().is_some() || returned_oid.len() != self.oid_len || kind.is_empty() {
            return Err(Error::git("malformed git cat-file header"));
        }
        let size = std::str::from_utf8(size)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or_else(|| Error::git("malformed git cat-file object size"))?;
        let mut data = vec![0; size];
        self.stdout
            .read_exact(&mut data)
            .map_err(|e| Error::git(format!("truncated git cat-file object: {e}")))?;
        let mut terminator = [0; 1];
        self.stdout
            .read_exact(&mut terminator)
            .map_err(|e| Error::git(format!("missing git cat-file object terminator: {e}")))?;
        if terminator != [b'\n'] {
            return Err(Error::git("malformed git cat-file object terminator"));
        }
        Ok((String::from_utf8_lossy(kind).into_owned(), data))
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) fn repository_root(path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| {
            Error::new(
                "not_a_repository",
                format!("could not run git: {e}"),
                serde_json::json!({"path": path}),
            )
        })?;
    if !output.status.success() {
        return Err(Error::new(
            "not_a_repository",
            format!("{} is not inside a Git repository", path.display()),
            serde_json::json!({"path": path}),
        ));
    }
    let root = trim_lf(&output.stdout);
    if root.is_empty() {
        return Err(Error::new(
            "not_a_repository",
            "Git returned an empty repository root",
            serde_json::json!({"path": path}),
        ));
    }
    Ok(PathBuf::from(String::from_utf8_lossy(root).into_owned()))
}

pub(crate) fn stderr_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

pub(crate) fn trim_lf(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}
