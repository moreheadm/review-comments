mod error;
mod git;
pub mod map;
mod model;
mod resolve;
mod store;
mod validate;

pub use error::{Error, Result};
pub use model::{CommitOptions, ShowOptions};

use serde_json::Value;
use std::path::{Path, PathBuf};

/// A repository accessed exclusively through Git (and jj for jj revision resolution).
pub struct Repository {
    root: PathBuf,
}

impl Repository {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let root = git::repository_root(path.as_ref())?;
        Ok(Self { root })
    }

    pub fn commit(&self, options: CommitOptions, input: &str) -> Result<Value> {
        store::commit(&self.root, options, input)
    }

    pub fn show(&self, options: ShowOptions) -> Result<Value> {
        store::show(&self.root, options)
    }

    pub fn log(&self, branch: &str) -> Result<Value> {
        store::log(&self.root, branch)
    }

    pub fn check(&self, branch: &str) -> Result<Value> {
        store::check(&self.root, branch)
    }

    pub fn branches(&self) -> Result<Value> {
        store::branches(&self.root)
    }
}

/// Generate a UUID version 7 identifier.
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}
