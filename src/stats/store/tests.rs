use super::*;
use std::time::Instant;

fn key(raw: &str) -> KeyId {
    KeyId::from_raw_key(raw)
}

fn usage(prompt_tokens: u64, completion_tokens: u64) -> Option<Usage> {
    Some(Usage {
        prompt_tokens,
        completion_tokens,
    })
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn records_requests_and_completions() {
    let store = StatsStore::default();
    let k = key("a");
    store.record_request(&k, Duration::from_millis(120), false);
    store.record_request(&k, Duration::from_millis(80), true);
    store.record_completion(&k, "primary/m1", usage(5, 2));
    store.record_completion(&k, "primary/m1", None);
    store.record_completion(&k, "backup/fb-model", usage(1, 1));

    let stats = store.get(&k).unwrap();
    assert_eq!(stats.request_count, 2);
    assert_eq!(stats.error_count, 1);
    assert_eq!(stats.total_latency_ms, 200);
    assert_eq!(stats.total_prompt_tokens, 6);
    assert_eq!(stats.total_completion_tokens, 3);
    assert_eq!(
        stats.models_used,
        BTreeMap::from([
            ("backup/fb-model".to_string(), 1),
            ("primary/m1".to_string(), 2)
        ])
    );
    assert!(stats.last_request_timestamp > 1_700_000_000_000);
    assert_eq!(store.get(&key("b")), None);
}

#[test]
fn save_then_load_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.json");
    let store = StatsStore::default();
    store.record_request(&key("a"), Duration::from_millis(10), false);
    store.record_completion(&key("b"), "primary/m1", usage(3, 4));
    store.save(&path).unwrap();

    let (loaded, outcome) = StatsStore::load(&path).unwrap();
    assert_eq!(
        outcome,
        LoadOutcome::Loaded {
            keys: 2,
            rekeyed: 0
        }
    );
    assert_eq!(loaded.all(), store.all());
}

#[test]
fn save_is_atomic_and_leaves_no_tmp_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.json");
    let store = StatsStore::default();
    store.record_request(&key("a"), Duration::from_millis(10), false);
    store.save(&path).unwrap();
    store.record_request(&key("a"), Duration::from_millis(10), false);
    store.save(&path).unwrap();

    assert_eq!(file_names(dir.path()), ["stats.json"]);
    let (loaded, _) = StatsStore::load(&path).unwrap();
    assert_eq!(loaded.get(&key("a")).unwrap().request_count, 2);
}

#[test]
fn a_missing_file_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let (store, outcome) = StatsStore::load(&dir.path().join("stats.json")).unwrap();
    assert_eq!(outcome, LoadOutcome::Missing);
    assert!(store.all().is_empty());
}

#[test]
fn a_corrupt_file_is_moved_aside() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.json");
    fs::write(&path, "{not json sk-raw-secret").unwrap();

    let (store, outcome) = StatsStore::load(&path).unwrap();
    assert!(store.all().is_empty());
    let LoadOutcome::Quarantined { moved_to, reason } = outcome else {
        panic!("expected Quarantined, got {outcome:?}");
    };
    assert!(!reason.contains("sk-raw-secret"), "{reason}");
    assert!(!path.exists());
    assert!(moved_to.exists());
    let names = file_names(dir.path());
    assert_eq!(names.len(), 1, "{names:?}");
    let suffix = names[0].strip_prefix("stats.json.corrupt-").unwrap();
    assert!(suffix.bytes().all(|b| b.is_ascii_digit()), "{names:?}");
}

#[cfg(unix)]
#[test]
fn an_unmovable_corrupt_file_is_an_error() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.json");
    fs::write(&path, "{not json").unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();

    // Root ignores directory permissions; there's nothing to test then.
    let enforced = fs::write(dir.path().join("probe"), "").is_err();
    let result = StatsStore::load(&path);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    if enforced {
        let err = result.err().expect("the rename can't succeed");
        assert!(err.to_string().contains("stats.json"), "{err}");
    }
}

#[test]
fn todays_file_format_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.json");
    let id = key("abc");
    let today = format!(
        r#"{{
  "stats": {{
    "{id}": {{
      "request_count": 3,
      "error_count": 1,
      "total_latency_ms": 300,
      "total_prompt_tokens": 10,
      "total_completion_tokens": 20,
      "models_used": {{"gemma4:e2b": 2}},
      "last_request_timestamp": 1795000000000
    }}
  }}
}}"#
    );
    fs::write(&path, today).unwrap();

    let (store, outcome) = StatsStore::load(&path).unwrap();
    assert_eq!(
        outcome,
        LoadOutcome::Loaded {
            keys: 1,
            rekeyed: 0
        }
    );
    assert_eq!(
        store.get(&id).unwrap(),
        KeyStats {
            request_count: 3,
            error_count: 1,
            total_latency_ms: 300,
            total_prompt_tokens: 10,
            total_completion_tokens: 20,
            models_used: BTreeMap::from([("gemma4:e2b".to_string(), 2)]),
            last_request_timestamp: 1_795_000_000_000,
        }
    );
}

#[test]
fn raw_key_entries_are_rekeyed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.json");
    fs::write(
        &path,
        r#"{"stats":{"sk-raw-secret":{"request_count":2,"error_count":0,"total_latency_ms":20,
            "total_prompt_tokens":0,"total_completion_tokens":0,"models_used":{},
            "last_request_timestamp":1795000000000}}}"#,
    )
    .unwrap();

    let (store, outcome) = StatsStore::load(&path).unwrap();
    assert_eq!(
        outcome,
        LoadOutcome::Loaded {
            keys: 1,
            rekeyed: 1
        }
    );
    assert_eq!(store.get(&key("sk-raw-secret")).unwrap().request_count, 2);

    store.save(&path).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("sk-raw-secret"), "{text}");
    assert!(text.contains(key("sk-raw-secret").as_str()), "{text}");
}

#[test]
fn a_rekeyed_entry_merges_with_its_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.json");
    let id = key("sk-raw-secret");
    let file = format!(
        r#"{{"stats":{{
            "sk-raw-secret":{{"request_count":2,"error_count":1,"total_latency_ms":20,
                "total_prompt_tokens":5,"total_completion_tokens":1,
                "models_used":{{"m1":1}},"last_request_timestamp":2000}},
            "{id}":{{"request_count":3,"error_count":0,"total_latency_ms":30,
                "total_prompt_tokens":7,"total_completion_tokens":2,
                "models_used":{{"m1":2,"m2":1}},"last_request_timestamp":1000}}}}}}"#
    );
    fs::write(&path, file).unwrap();

    let (store, outcome) = StatsStore::load(&path).unwrap();
    assert_eq!(
        outcome,
        LoadOutcome::Loaded {
            keys: 1,
            rekeyed: 1
        }
    );
    assert_eq!(
        store.get(&id).unwrap(),
        KeyStats {
            request_count: 5,
            error_count: 1,
            total_latency_ms: 50,
            total_prompt_tokens: 12,
            total_completion_tokens: 3,
            models_used: BTreeMap::from([("m1".to_string(), 3), ("m2".to_string(), 1)]),
            last_request_timestamp: 2000,
        }
    );
}

#[actix_web::test]
async fn flusher_saves_on_its_interval() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.json");
    let store = Arc::new(StatsStore::default());
    store.record_request(&key("a"), Duration::from_millis(10), false);

    let flusher = spawn_flusher(Arc::clone(&store), path.clone(), Duration::from_millis(50));
    let deadline = Instant::now() + Duration::from_secs(2);
    let saved = loop {
        let text = fs::read_to_string(&path).unwrap_or_default();
        if text.contains(key("a").as_str()) {
            break true;
        }
        if Instant::now() > deadline {
            break false;
        }
        actix_web::rt::time::sleep(Duration::from_millis(10)).await;
    };
    flusher.abort();
    assert!(saved, "the flusher never wrote {}", path.display());
}

#[test]
fn long_model_labels_are_cut() {
    let store = StatsStore::default();
    // Two-byte characters, so the cut has to land on a character boundary.
    let label = format!("primary/{}", "é".repeat(300));
    store.record_completion(&key("a"), &label, None);

    let models = store.get(&key("a")).unwrap().models_used;
    let kept: Vec<&String> = models.keys().collect();
    assert_eq!(kept.len(), 1);
    assert!(kept[0].len() <= 256, "{} bytes", kept[0].len());
    assert!(label.starts_with(kept[0].as_str()));
}

#[test]
fn distinct_labels_per_key_are_capped() {
    let store = StatsStore::default();
    for i in 0..105 {
        store.record_completion(&key("a"), &format!("primary/model-{i}"), None);
    }
    // A label it already has still counts as itself.
    store.record_completion(&key("a"), "primary/model-0", None);

    let models = store.get(&key("a")).unwrap().models_used;
    assert_eq!(models.len(), 101, "{:?}", models.keys().last());
    assert_eq!(models["primary/(other)"], 5);
    assert_eq!(models["primary/model-0"], 2);
}

#[test]
fn a_directory_path_is_an_error_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.json");
    fs::create_dir(&path).unwrap();

    let err = StatsStore::load(&path)
        .err()
        .expect("a directory isn't a stats file");
    assert!(err.to_string().contains("STATS_PATH"), "{err}");
    assert!(path.is_dir());
    assert_eq!(file_names(dir.path()), ["stats.json"]);
}

#[test]
fn open_saves_at_once_so_raw_keys_leave_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.json");
    fs::write(&path, r#"{"stats":{"sk-raw-secret":{"request_count":1}}}"#).unwrap();

    let (store, outcome) = StatsStore::open(&path).unwrap();
    assert_eq!(
        outcome,
        LoadOutcome::Loaded {
            keys: 1,
            rekeyed: 1
        }
    );
    assert_eq!(store.get(&key("sk-raw-secret")).unwrap().request_count, 1);
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("sk-raw-secret"), "{text}");
}

#[cfg(unix)]
#[test]
fn open_fails_when_stats_cannot_be_written() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();

    // Root ignores directory permissions; there's nothing to test then.
    let enforced = fs::write(dir.path().join("probe"), "").is_err();
    let result = StatsStore::open(&dir.path().join("stats.json"));
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    if enforced {
        let err = result
            .err()
            .expect("a read-only directory can't hold stats");
        assert!(err.to_string().contains("STATS_PATH"), "{err}");
    }
}
