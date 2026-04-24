use crate::config::IndexConfig;
use ignore::WalkBuilder;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// A trigram is a 3-byte sequence used for fast substring search.
type Trigram = [u8; 3];

/// Background workspace indexer.
/// Builds a trigram index over all text files for sub-millisecond search.
pub struct WorkspaceIndex {
    root: PathBuf,
    inner: Arc<RwLock<IndexState>>,
    max_depth: usize,
    max_file_size: usize,
    binary_sniff_bytes: usize,
    skip_extensions: HashSet<String>,
}

struct IndexState {
    /// Trigram -> set of file IDs that contain it
    trigrams: HashMap<Trigram, Vec<u32>>,
    /// File ID -> relative path
    files: Vec<String>,
    /// File ID -> last modified time (for incremental updates)
    mtimes: Vec<u64>,
    /// Total indexed files
    file_count: usize,
    /// Time of last full index
    last_indexed: Option<Instant>,
}

impl IndexState {
    fn new() -> Self {
        Self {
            trigrams: HashMap::new(),
            files: Vec::new(),
            mtimes: Vec::new(),
            file_count: 0,
            last_indexed: None,
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct IndexStats {
    pub files: usize,
    pub trigrams: usize,
    pub age_ms: Option<u64>,
}

#[derive(Debug, serde::Serialize)]
pub struct SearchResult {
    pub file: String,
    pub score: u32,
}

impl WorkspaceIndex {
    pub fn new(root: PathBuf, cfg: &IndexConfig) -> Self {
        Self {
            root,
            inner: Arc::new(RwLock::new(IndexState::new())),
            max_depth: cfg.max_depth,
            max_file_size: cfg.max_file_size,
            binary_sniff_bytes: cfg.binary_sniff_bytes,
            skip_extensions: cfg.skip_set(),
        }
    }

    /// Rebuild the index in the background.
    pub fn spawn_reindex(&self) {
        let root = self.root.clone();
        let inner = Arc::clone(&self.inner);
        let max_depth = self.max_depth;
        let max_file_size = self.max_file_size;
        let binary_sniff_bytes = self.binary_sniff_bytes;
        let skip_ext = self.skip_extensions.clone();

        tokio::task::spawn_blocking(move || {
            let start = Instant::now();
            let mut files = Vec::new();
            let mut mtimes = Vec::new();
            let mut trigrams: HashMap<Trigram, Vec<u32>> = HashMap::new();

            let walker = WalkBuilder::new(&root)
                .hidden(true)
                .git_ignore(true)
                .max_depth(Some(max_depth))
                .build();

            for entry in walker.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }

                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if skip_ext.contains(&ext) {
                    continue;
                }

                let content = match std::fs::read(path) {
                    Ok(c) if c.len() <= max_file_size => c,
                    _ => continue,
                };

                if content
                    .iter()
                    .take(binary_sniff_bytes)
                    .any(|&b| b == 0)
                {
                    continue;
                }

                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();

                let file_id = files.len() as u32;
                files.push(rel);

                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| {
                        m.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                    })
                    .unwrap_or(0);
                mtimes.push(mtime);

                extract_trigrams(&content, file_id, &mut trigrams);
            }

            let file_count = files.len();
            let trigram_count = trigrams.len();

            let state = IndexState {
                trigrams,
                files,
                mtimes,
                file_count,
                last_indexed: Some(start),
            };

            let inner_clone = inner.clone();
            tokio::runtime::Handle::current().block_on(async {
                let mut guard = inner_clone.write().await;
                *guard = state;
            });

            eprintln!(
                "index: {} files, {} trigrams in {:?}",
                file_count,
                trigram_count,
                start.elapsed()
            );
        });
    }

    /// Search the trigram index for files likely containing the query.
    pub async fn search(&self, query: &str, max: usize) -> Vec<SearchResult> {
        let query_trigrams = query_to_trigrams(query.as_bytes());
        if query_trigrams.is_empty() {
            return Vec::new();
        }

        let guard = self.inner.read().await;

        // For each file, count how many query trigrams it contains
        let mut scores: HashMap<u32, u32> = HashMap::new();
        for tri in &query_trigrams {
            if let Some(file_ids) = guard.trigrams.get(tri) {
                for &fid in file_ids {
                    *scores.entry(fid).or_insert(0) += 1;
                }
            }
        }

        let threshold = (query_trigrams.len() as u32).saturating_sub(1).max(1);

        let mut results: Vec<SearchResult> = scores
            .into_iter()
            .filter(|(_, score)| *score >= threshold)
            .map(|(fid, score)| SearchResult {
                file: guard.files.get(fid as usize).cloned().unwrap_or_default(),
                score,
            })
            .collect();

        results.sort_by(|a, b| b.score.cmp(&a.score));
        results.truncate(max);
        results
    }

    pub async fn stats(&self) -> IndexStats {
        let guard = self.inner.read().await;
        IndexStats {
            files: guard.file_count,
            trigrams: guard.trigrams.len(),
            age_ms: guard.last_indexed.map(|t| t.elapsed().as_millis() as u64),
        }
    }
}

fn extract_trigrams(content: &[u8], file_id: u32, trigrams: &mut HashMap<Trigram, Vec<u32>>) {
    if content.len() < 3 {
        return;
    }
    let mut seen = std::collections::HashSet::new();
    for window in content.windows(3) {
        let tri: Trigram = [window[0], window[1], window[2]];
        if seen.insert(tri) {
            trigrams.entry(tri).or_default().push(file_id);
        }
    }
}

fn query_to_trigrams(query: &[u8]) -> Vec<Trigram> {
    if query.len() < 3 {
        return Vec::new();
    }
    query
        .windows(3)
        .map(|w| [w[0], w[1], w[2]])
        .collect()
}

