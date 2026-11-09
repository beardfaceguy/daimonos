//! Authorized ignore-file loading for confined automatic walks (ADR-022).
//!
//! Ignore rules are caller-controlled evidence. Never open them by ambient path.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::path_policy::{FilesystemAuthority, Walk};

#[derive(Clone, Copy, PartialEq, Eq)]
enum LayerKind {
    Ignore,
    Gitignore,
    Exclude,
}

struct Layer {
    relative_dir: PathBuf,
    kind: LayerKind,
    rules: Gitignore,
}

/// Ignore rules read only through the run's pinned filesystem authority.
pub(crate) struct ConfinedIgnore {
    root: PathBuf,
    layers: Vec<Layer>,
    ignored_dirs: Vec<PathBuf>,
    git_roots: Vec<PathBuf>,
}

impl ConfinedIgnore {
    pub(crate) fn from_walk(authority: &FilesystemAuthority, start: &Path, walk: &Walk) -> Self {
        let mut directories = BTreeSet::new();
        directories.insert(PathBuf::new());
        if let Ok(start) = authority.open_read(start) {
            let mut current = PathBuf::new();
            for component in start.relative_path().components() {
                if let Component::Normal(name) = component {
                    current.push(name);
                    directories.insert(current.clone());
                }
            }
        }
        for entry in &walk.entries {
            if entry.metadata.is_dir() {
                directories.insert(entry.relative_path.clone());
            }
        }
        let mut directories: Vec<PathBuf> = directories.into_iter().collect();
        directories.sort_by(|a, b| {
            a.components()
                .count()
                .cmp(&b.components().count())
                .then_with(|| a.cmp(b))
        });
        let git_roots: Vec<PathBuf> = directories
            .iter()
            .filter(|directory| authority.open_read(&directory.join(".git")).is_ok())
            .cloned()
            .collect();
        let mut result = Self {
            root: authority.root().to_path_buf(),
            layers: Vec::new(),
            ignored_dirs: Vec::new(),
            git_roots: git_roots.clone(),
        };
        for directory in directories {
            if result.is_ignored(&directory, true) {
                result.ignored_dirs.push(directory);
                continue;
            }
            if git_roots.contains(&directory) {
                result.add_file(
                    authority,
                    &directory,
                    ".git/info/exclude",
                    LayerKind::Exclude,
                );
            }
            if git_roots.iter().any(|root| directory.starts_with(root)) {
                result.add_file(authority, &directory, ".gitignore", LayerKind::Gitignore);
            }
            result.add_file(authority, &directory, ".ignore", LayerKind::Ignore);
        }
        result
    }

    fn add_file(
        &mut self,
        authority: &FilesystemAuthority,
        directory: &Path,
        name: &str,
        kind: LayerKind,
    ) {
        let relative = directory.join(name);
        let Ok(authorized) = authority.open_read(&relative) else {
            return;
        };
        let mut content = String::new();
        if authorized.into_file().read_to_string(&mut content).is_err() {
            return;
        }
        let mut builder = GitignoreBuilder::new(self.root.join(directory));
        for line in content.lines() {
            if let Err(error) = builder.add_line(Some(self.root.join(&relative)), line) {
                tracing::debug!(?error, "invalid confined ignore pattern");
            }
        }
        if let Ok(rules) = builder.build() {
            self.layers.push(Layer {
                relative_dir: directory.to_path_buf(),
                kind,
                rules,
            });
        }
    }

    pub(crate) fn is_ignored(&self, relative: &Path, is_dir: bool) -> bool {
        if self
            .ignored_dirs
            .iter()
            .any(|directory| relative.starts_with(directory))
        {
            return true;
        }
        let path = self.root.join(relative);
        let nearest_git = self
            .git_roots
            .iter()
            .filter(|root| relative.starts_with(root) && (!is_dir || root.as_path() != relative))
            .max_by_key(|root| root.components().count());
        for kind in [LayerKind::Ignore, LayerKind::Gitignore, LayerKind::Exclude] {
            for layer in self.layers.iter().rev().filter(|layer| layer.kind == kind) {
                if !relative.starts_with(&layer.relative_dir) {
                    continue;
                }
                if kind != LayerKind::Ignore
                    && nearest_git.is_some_and(|root| !layer.relative_dir.starts_with(root))
                {
                    continue;
                }
                match layer.rules.matched_path_or_any_parents(&path, is_dir) {
                    ignore::Match::Ignore(_) => return true,
                    ignore::Match::Whitelist(_) => return false,
                    ignore::Match::None => {}
                }
            }
        }
        false
    }
}
