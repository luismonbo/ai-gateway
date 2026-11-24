//! Per-key usage counters, kept in memory and saved to a JSON file.
//!
//! The file is `{"stats": {"<key id>": KeyStats}}`, the same shape older
//! versions wrote. Saves replace the file atomically, so a crash mid-save
//! never leaves half a file behind.

use crate::auth::KeyId;
use crate::proxy::usage::Usage;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, error};

/// Longest `upstream/model` label kept; longer ones are cut.
const MAX_LABEL_BYTES: usize = 256;
/// Distinct labels kept per key; answers from further models count under
/// `upstream/(other)`.
const MAX_LABELS_PER_KEY: usize = 100;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct KeyStats {
    pub request_count: u64,
    pub error_count: u64,
    pub total_latency_ms: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    /// Answers per `upstream/model`.
    pub models_used: BTreeMap<String, u64>,
    /// Unix milliseconds of the last request.
    pub last_request_timestamp: u64,
}

impl KeyStats {
    /// Both entries' counts added together, keeping the later timestamp.
    fn merged(self, other: KeyStats) -> KeyStats {
        let models_used = other
            .models_used
            .into_iter()
            .fold(self.models_used, |models, (model, count)| {
                counted(models, &model, count)
            });
        KeyStats {
            request_count: self.request_count.saturating_add(other.request_count),
            error_count: self.error_count.saturating_add(other.error_count),
            total_latency_ms: self.total_latency_ms.saturating_add(other.total_latency_ms),
            total_prompt_tokens: self
                .total_prompt_tokens
                .saturating_add(other.total_prompt_tokens),
            total_completion_tokens: self
                .total_completion_tokens
                .saturating_add(other.total_completion_tokens),
            models_used,
            last_request_timestamp: self
                .last_request_timestamp
                .max(other.last_request_timestamp),
        }
    }
}

/// The label to count an answer under. The model part comes from the
/// client or the upstream, so both its length and the number of labels per
/// key are capped: a client inventing model names can't grow the stats.
fn bounded_label(models: &BTreeMap<String, u64>, label: &str) -> String {
    let label = &label[..label.floor_char_boundary(MAX_LABEL_BYTES)];
    if models.len() < MAX_LABELS_PER_KEY || models.contains_key(label) {
        return label.to_string();
    }
    let upstream = label
        .split_once('/')
        .map_or(label, |(upstream, _)| upstream);
    format!("{upstream}/(other)")
}

/// `models` with `count` more answers from `model`. It takes the map by
/// value, so no one else can see it change.
fn counted(models: BTreeMap<String, u64>, model: &str, count: u64) -> BTreeMap<String, u64> {
    let mut models = models;
    let total = models.entry(model.to_string()).or_insert(0);
    *total = total.saturating_add(count);
    models
}

#[derive(Serialize)]
struct FileOut<'a> {
    stats: &'a BTreeMap<KeyId, KeyStats>,
}

/// Keys are read as plain strings: older files used raw API keys.
#[derive(Deserialize)]
struct FileIn {
    #[serde(default)]
    stats: BTreeMap<String, KeyStats>,
}

/// What [`StatsStore::load`] found.
#[derive(Debug, PartialEq)]
pub enum LoadOutcome {
    Missing,
    /// `rekeyed` entries were stored under raw keys and now use key IDs.
    Loaded {
        keys: usize,
        rekeyed: usize,
    },
    /// The file was unusable and was renamed so nothing overwrites it.
    Quarantined {
        moved_to: PathBuf,
        reason: String,
    },
}

/// Why the gateway can't use `STATS_PATH`; startup stops on any of these.
#[derive(Debug, thiserror::Error)]
pub enum StatsLoadError {
    /// The file is unusable and couldn't be moved aside either.
    #[error("stats file {} is unusable ({reason}) and could not be moved aside: {source}", .path.display())]
    Unmovable {
        path: PathBuf,
        reason: String,
        #[source]
        source: io::Error,
    },
    #[error(
        "STATS_PATH {} is a directory; point it at a file such as {}",
        .path.display(),
        .path.join("stats.json").display()
    )]
    IsDirectory { path: PathBuf },
    #[error(
        "STATS_PATH {} can't be written ({source}); make its directory writable by the gateway's user",
        .path.display()
    )]
    Unwritable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Default)]
pub struct StatsStore {
    stats: Mutex<BTreeMap<KeyId, KeyStats>>,
    /// Saves run one at a time, so two never share the temp file.
    save_lock: Mutex<()>,
}

impl StatsStore {
    /// Loads `path`, then saves straight away. An unwritable `STATS_PATH`
    /// then stops startup instead of failing every later save, and raw keys
    /// from old files leave the disk at once.
    pub fn open(path: &Path) -> Result<(StatsStore, LoadOutcome), StatsLoadError> {
        let (store, outcome) = StatsStore::load(path)?;
        store
            .save(path)
            .map_err(|source| StatsLoadError::Unwritable {
                path: path.to_path_buf(),
                source,
            })?;
        Ok((store, outcome))
    }

    /// Reads `path`. A missing file means starting empty; an unusable one is
    /// renamed to `<path>.corrupt-<unix secs>` and the store starts empty. A
    /// directory is an error and is left where it is.
    pub fn load(path: &Path) -> Result<(StatsStore, LoadOutcome), StatsLoadError> {
        if path.is_dir() {
            return Err(StatsLoadError::IsDirectory {
                path: path.to_path_buf(),
            });
        }
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok((StatsStore::default(), LoadOutcome::Missing));
            }
            Err(e) => return quarantine(path, format!("could not read it: {e}")),
        };
        let file: FileIn = match serde_json::from_slice(&bytes) {
            Ok(file) => file,
            // Only the position: serde's message can quote the file's text,
            // and old files hold raw keys.
            Err(e) => {
                let reason = format!(
                    "not valid stats JSON: {:?} error at line {}, column {}",
                    e.classify(),
                    e.line(),
                    e.column()
                );
                return quarantine(path, reason);
            }
        };
        let (stats, rekeyed) = rekey(file.stats);
        let outcome = LoadOutcome::Loaded {
            keys: stats.len(),
            rekeyed,
        };
        let store = StatsStore {
            stats: Mutex::new(stats),
            save_lock: Mutex::new(()),
        };
        Ok((store, outcome))
    }

    /// Writes a snapshot to `<path>.tmp`, syncs it to disk and renames it
    /// over `path`. Requests aren't blocked while the file is written.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let _one_at_a_time = self
            .save_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let snapshot = self.lock().clone();
        let json = serde_json::to_vec_pretty(&FileOut { stats: &snapshot })?;
        let tmp = with_suffix(path, ".tmp");
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&json)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    }

    pub fn record_request(&self, key: &KeyId, latency: Duration, is_error: bool) {
        let latency_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX);
        self.update(key, |stats| KeyStats {
            request_count: stats.request_count.saturating_add(1),
            error_count: stats.error_count.saturating_add(u64::from(is_error)),
            total_latency_ms: stats.total_latency_ms.saturating_add(latency_ms),
            last_request_timestamp: unix_millis(),
            ..stats
        });
    }

    /// Counts one answer from `model_label` (`upstream/model`) and adds its
    /// tokens when the upstream reported them.
    pub fn record_completion(&self, key: &KeyId, model_label: &str, usage: Option<Usage>) {
        let usage = usage.unwrap_or_default();
        self.update(key, |stats| {
            let label = bounded_label(&stats.models_used, model_label);
            KeyStats {
                total_prompt_tokens: stats
                    .total_prompt_tokens
                    .saturating_add(usage.prompt_tokens),
                total_completion_tokens: stats
                    .total_completion_tokens
                    .saturating_add(usage.completion_tokens),
                models_used: counted(stats.models_used, &label, 1),
                ..stats
            }
        });
    }

    pub fn get(&self, key: &KeyId) -> Option<KeyStats> {
        self.lock().get(key).cloned()
    }

    /// Every key's stats, sorted by key ID.
    pub fn all(&self) -> Vec<(KeyId, KeyStats)> {
        self.lock()
            .iter()
            .map(|(key, stats)| (key.clone(), stats.clone()))
            .collect()
    }

    /// Replaces `key`'s entry with `change(old entry)`.
    fn update(&self, key: &KeyId, change: impl FnOnce(KeyStats) -> KeyStats) {
        let mut stats = self.lock();
        let current = stats.remove(key).unwrap_or_default();
        stats.insert(key.clone(), change(current));
    }

    /// The counters stay consistent even if a holder panicked, so a
    /// poisoned lock is still usable.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<KeyId, KeyStats>> {
        self.stats.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Entries under anything but a key ID (older files used raw keys) are
/// re-keyed by hashing and merged into that ID's entry. Returns the stats
/// and how many entries were re-keyed.
fn rekey(entries: BTreeMap<String, KeyStats>) -> (BTreeMap<KeyId, KeyStats>, usize) {
    let rekeyed = entries
        .keys()
        .filter(|name| KeyId::parse(name).is_none())
        .count();
    let mut stats: BTreeMap<KeyId, KeyStats> = BTreeMap::new();
    for (name, entry) in entries {
        let id = KeyId::parse(&name).unwrap_or_else(|| KeyId::from_raw_key(&name));
        let combined = match stats.remove(&id) {
            Some(existing) => existing.merged(entry),
            None => entry,
        };
        stats.insert(id, combined);
    }
    (stats, rekeyed)
}

fn quarantine(path: &Path, reason: String) -> Result<(StatsStore, LoadOutcome), StatsLoadError> {
    let moved_to = with_suffix(path, &format!(".corrupt-{}", unix_secs()));
    match fs::rename(path, &moved_to) {
        Ok(()) => Ok((
            StatsStore::default(),
            LoadOutcome::Quarantined { moved_to, reason },
        )),
        Err(source) => Err(StatsLoadError::Unmovable {
            path: path.to_path_buf(),
            reason,
            source,
        }),
    }
}

/// `path` with `suffix` appended to its file name: `stats.json.tmp`.
/// Works on the raw OS string, so a non-UTF-8 path keeps its exact bytes.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn unix_millis() -> u64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX)
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Saves `stats` to `path` every `every`, starting one interval from now.
/// Each save runs on a blocking thread; failures are logged and retried at
/// the next interval.
pub fn spawn_flusher(
    stats: Arc<StatsStore>,
    path: PathBuf,
    every: Duration,
) -> actix_web::rt::task::JoinHandle<()> {
    actix_web::rt::spawn(async move {
        loop {
            actix_web::rt::time::sleep(every).await;
            let (stats, target) = (Arc::clone(&stats), path.clone());
            match actix_web::rt::task::spawn_blocking(move || stats.save(&target)).await {
                Ok(Ok(())) => debug!(path = %path.display(), "saved stats"),
                Ok(Err(e)) => error!(path = %path.display(), "could not save stats: {e}"),
                Err(e) => error!("the stats save task failed: {e}"),
            }
        }
    })
}

#[cfg(test)]
mod tests;
