use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

const MAX_HISTORY: usize = 50;

pub struct HistoryStore {
    path: Option<PathBuf>,
    entries: Vec<PathBuf>,
}

impl HistoryStore {
    #[cfg(not(test))]
    pub fn load_default() -> Self {
        let path = history_path();
        let entries = path
            .as_ref()
            .and_then(|p| load_entries(p).ok())
            .unwrap_or_default();
        HistoryStore { path, entries }
    }

    #[cfg(test)]
    pub fn memory(entries: Vec<PathBuf>) -> Self {
        HistoryStore {
            path: None,
            entries: normalize_entries(entries),
        }
    }

    #[cfg(test)]
    pub fn load_from(path: PathBuf) -> Self {
        let entries = load_entries(&path).unwrap_or_default();
        HistoryStore {
            path: Some(path),
            entries,
        }
    }

    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    pub fn record(&mut self, path: PathBuf) {
        self.entries.retain(|p| p != &path);
        self.entries.insert(0, path);
        self.entries.truncate(MAX_HISTORY);
        let _ = self.save();
    }

    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = String::new();
        for entry in &self.entries {
            text.push_str(&entry.to_string_lossy());
            text.push('\n');
        }
        std::fs::write(path, text)
    }
}

fn load_entries(path: &Path) -> io::Result<Vec<PathBuf>> {
    let text = std::fs::read_to_string(path)?;
    Ok(normalize_entries(
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect(),
    ))
}

fn normalize_entries(entries: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for entry in entries {
        if seen.insert(entry.clone()) {
            out.push(entry);
        }
        if out.len() == MAX_HISTORY {
            break;
        }
    }
    out
}

#[cfg(not(test))]
fn history_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("LUCOA_STATE_DIR") {
        return Some(PathBuf::from(dir).join("open-history"));
    }

    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("lucoa").join("open-history"))
    }

    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(PathBuf::from).map(|p| {
            p.join("Library")
                .join("Application Support")
                .join("lucoa")
                .join("open-history")
        })
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local").join("state"))
            })
            .map(|p| p.join("lucoa").join("open-history"))
    }
}

pub fn fuzzy_matches(candidate: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let mut chars = candidate.chars().flat_map(char::to_lowercase);
    for needle in query.chars().flat_map(char::to_lowercase) {
        if !chars.any(|c| c == needle) {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn history_deduplicates_caps_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        let mut store = HistoryStore {
            path: Some(path.clone()),
            entries: Vec::new(),
        };
        for i in 0..55 {
            store.record(PathBuf::from(format!("file-{i}")));
        }
        store.record(PathBuf::from("file-50"));

        let reloaded = HistoryStore::load_from(path);
        assert_eq!(reloaded.entries.len(), 50);
        assert_eq!(reloaded.entries[0], PathBuf::from("file-50"));
        assert_eq!(
            reloaded
                .entries
                .iter()
                .filter(|p| p.as_os_str() == "file-50")
                .count(),
            1
        );
    }

    #[test]
    fn fuzzy_matching_is_ordered_and_case_insensitive() {
        assert!(fuzzy_matches("/tmp/Alpha.log", "al"));
        assert!(fuzzy_matches("/tmp/Alpha.log", "ALOG"));
        assert!(!fuzzy_matches("/tmp/Alpha.log", "laa"));
    }
}
