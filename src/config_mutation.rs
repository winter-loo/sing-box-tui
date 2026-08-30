use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::atomic_file::{DurableAtomicWriteError, write_atomic_durable};
use crate::node_quality_path::{
    canonical_config_target, config_mutation_lock_path, ensure_active_config_paths_are_distinct,
};
use crate::storage::{
    BenchmarkStore, NodeHistoryReconciliation, NodeHistoryReconciliationTransaction,
    lock_node_quality_reconciliation,
};

// Every config editor performs a read-modify-write cycle. Atomic replacement protects readers
// from partial files, while this process-wide lock prevents concurrent local editors from
// committing changes derived from the same stale snapshot.
static CONFIG_MUTATION_LOCK: Mutex<()> = Mutex::new(());

pub(crate) struct ConfigMutationGuard {
    _process_guard: MutexGuard<'static, ()>,
    _target_guard: ConfigTargetMutationGuard,
}

pub(crate) struct ConfigTargetMutationGuard {
    _file: File,
}

#[derive(Debug)]
pub(crate) struct ActiveNodeConfigCommit {
    pub(crate) backup_path: Option<PathBuf>,
    pub(crate) reconciliation: NodeHistoryReconciliation,
}

struct ActiveNodeCommitHooks<Before, After, Write> {
    before_reconcile: Before,
    after_commit_before_marker_cleanup: After,
    write_config: Write,
}

/// Serializes every read-modify-write of one active config across threads and processes.
///
/// The acquisition order is process mutex, canonical config-target file lock, node-quality file
/// lock, then SQLite `BEGIN IMMEDIATE`. Callers must never acquire these locks in reverse order.
pub(crate) fn lock_config_mutation_for(config_path: &Path) -> Result<ConfigMutationGuard> {
    let process_guard = CONFIG_MUTATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let target_guard = lock_config_target_mutation(config_path)?;
    Ok(ConfigMutationGuard {
        _process_guard: process_guard,
        _target_guard: target_guard,
    })
}

fn lock_config_target_mutation(config_path: &Path) -> Result<ConfigTargetMutationGuard> {
    let lock_path = config_mutation_lock_path(config_path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| {
            format!(
                "failed to open config mutation lock {}",
                lock_path.display()
            )
        })?;
    file.lock().with_context(|| {
        format!(
            "failed to acquire config mutation lock {}",
            lock_path.display()
        )
    })?;
    Ok(ConfigTargetMutationGuard { _file: file })
}

pub(crate) fn paths_refer_to_same_target(left: &Path, right: &Path) -> Result<bool> {
    Ok(canonical_config_target(left)? == canonical_config_target(right)?)
}

pub(crate) fn commit_active_node_config<Build, Prerequisite, Backup>(
    config_path: &Path,
    database_path: &Path,
    build_config: Build,
    prepare_prerequisite: Prerequisite,
    create_backup: Backup,
) -> Result<ActiveNodeConfigCommit>
where
    Build: FnOnce() -> Result<Value>,
    Prerequisite: FnOnce() -> Result<()>,
    Backup: FnOnce() -> Result<Option<PathBuf>>,
{
    ensure_active_config_paths_are_distinct(config_path, database_path, &[])?;
    let _config_guard = lock_config_mutation_for(config_path)?;
    commit_active_node_config_locked(
        config_path,
        database_path,
        build_config,
        prepare_prerequisite,
        create_backup,
        ActiveNodeCommitHooks {
            before_reconcile: || {},
            after_commit_before_marker_cleanup: || {},
            write_config: write_atomic_durable,
        },
    )
}

/// Exercises the production active-config transaction with an injected durable writer.
///
/// This is intentionally test-only: it lets failure-path tests stop before the destination rename
/// without weakening the production durable-write contract.
#[cfg(test)]
pub(crate) fn commit_active_node_config_with_writer_for_test<Build, Prerequisite, Backup, Write>(
    config_path: &Path,
    database_path: &Path,
    build_config: Build,
    prepare_prerequisite: Prerequisite,
    create_backup: Backup,
    write_config: Write,
) -> Result<ActiveNodeConfigCommit>
where
    Build: FnOnce() -> Result<Value>,
    Prerequisite: FnOnce() -> Result<()>,
    Backup: FnOnce() -> Result<Option<PathBuf>>,
    Write: FnOnce(&Path, &[u8]) -> std::result::Result<(), DurableAtomicWriteError>,
{
    ensure_active_config_paths_are_distinct(config_path, database_path, &[])?;
    let _config_guard = lock_config_mutation_for(config_path)?;
    commit_active_node_config_locked(
        config_path,
        database_path,
        build_config,
        prepare_prerequisite,
        create_backup,
        ActiveNodeCommitHooks {
            before_reconcile: || {},
            after_commit_before_marker_cleanup: || {},
            write_config,
        },
    )
}

/// Test-only process-boundary primitive: the canonical file lock is retained while the in-process
/// mutex is deliberately bypassed, which lets deterministic tests model independent processes.
#[cfg(test)]
pub(crate) fn commit_active_node_config_as_independent_process<
    Build,
    Prerequisite,
    Backup,
    Before,
    After,
>(
    config_path: &Path,
    database_path: &Path,
    build_config: Build,
    prepare_prerequisite: Prerequisite,
    create_backup: Backup,
    before_reconcile: Before,
    after_commit_before_marker_cleanup: After,
) -> Result<ActiveNodeConfigCommit>
where
    Build: FnOnce() -> Result<Value>,
    Prerequisite: FnOnce() -> Result<()>,
    Backup: FnOnce() -> Result<Option<PathBuf>>,
    Before: FnOnce(),
    After: FnOnce(),
{
    ensure_active_config_paths_are_distinct(config_path, database_path, &[])?;
    let _config_guard = lock_config_target_mutation(config_path)?;
    commit_active_node_config_locked(
        config_path,
        database_path,
        build_config,
        prepare_prerequisite,
        create_backup,
        ActiveNodeCommitHooks {
            before_reconcile,
            after_commit_before_marker_cleanup,
            write_config: write_atomic_durable,
        },
    )
}

fn commit_active_node_config_locked<Build, Prerequisite, Backup, Before, After, Write>(
    config_path: &Path,
    database_path: &Path,
    build_config: Build,
    prepare_prerequisite: Prerequisite,
    create_backup: Backup,
    hooks: ActiveNodeCommitHooks<Before, After, Write>,
) -> Result<ActiveNodeConfigCommit>
where
    Build: FnOnce() -> Result<Value>,
    Prerequisite: FnOnce() -> Result<()>,
    Backup: FnOnce() -> Result<Option<PathBuf>>,
    Before: FnOnce(),
    After: FnOnce(),
    Write: FnOnce(&Path, &[u8]) -> std::result::Result<(), DurableAtomicWriteError>,
{
    let ActiveNodeCommitHooks {
        before_reconcile,
        after_commit_before_marker_cleanup,
        write_config,
    } = hooks;
    let quality_guard = lock_node_quality_reconciliation(database_path)?;
    let quality_store =
        BenchmarkStore::open_while_reconciliation_locked(database_path, &quality_guard)?;
    let quality_transaction = quality_store.begin_node_history_reconciliation()?;
    let marker_created = quality_store.ensure_quality_writes_blocked()?;
    let previous_config = match read_optional_config(config_path) {
        Ok(previous) => previous,
        Err(error) => {
            return Err(recover_unchanged_config_failure(
                &quality_store,
                quality_transaction,
                marker_created,
                error,
            ));
        }
    };
    let committed_config = match build_config() {
        Ok(config) => config,
        Err(error) => {
            return Err(recover_unchanged_config_failure(
                &quality_store,
                quality_transaction,
                marker_created,
                error,
            ));
        }
    };
    if let Err(error) = prepare_prerequisite() {
        return Err(recover_unchanged_config_failure(
            &quality_store,
            quality_transaction,
            marker_created,
            error,
        ));
    }
    let backup_path = match create_backup() {
        Ok(backup_path) => backup_path,
        Err(error) => {
            return Err(recover_unchanged_config_failure(
                &quality_store,
                quality_transaction,
                marker_created,
                error,
            ));
        }
    };
    let contents = match serde_json::to_string_pretty(&committed_config)
        .context("failed to serialize active sing-box config")
    {
        Ok(contents) => format!("{contents}\n"),
        Err(error) => {
            return Err(recover_unchanged_config_failure(
                &quality_store,
                quality_transaction,
                marker_created,
                error,
            ));
        }
    };
    if let Err(error) = write_config(config_path, contents.as_bytes()) {
        let (error, durability_uncertain) = error.into_parts();
        if durability_uncertain {
            let transaction_rollback = quality_transaction.rollback();
            return Err(combine_failures(
                error.context("active config visibility or durability is uncertain"),
                Ok(()),
                transaction_rollback,
                Ok(()),
                true,
            ));
        }
        return Err(recover_unchanged_config_failure(
            &quality_store,
            quality_transaction,
            marker_created,
            error,
        ));
    }

    before_reconcile();
    let reconciliation = match quality_transaction.apply(&committed_config) {
        Ok(reconciliation) => reconciliation,
        Err(error) => {
            let config_rollback = restore_config(config_path, previous_config.as_deref());
            return Err(recover_changed_config_failure(
                &quality_store,
                quality_transaction,
                marker_created,
                error
                    .context("failed to reconcile node-quality history after active config commit"),
                config_rollback,
            ));
        }
    };
    if reconciliation.identities_changed
        && let Err(error) = quality_store.ensure_runtime_reload_required()
    {
        let config_rollback = restore_config(config_path, previous_config.as_deref());
        return Err(recover_changed_config_failure(
            &quality_store,
            quality_transaction,
            marker_created,
            error.context(
                "failed to fence node-quality facts until the live sing-box config is reloaded",
            ),
            config_rollback,
        ));
    }
    // This durable fence is created before the identity transaction commits and before the
    // general write block is removed. A new TUI process therefore cannot bind the new on-disk
    // fingerprints and accidentally attribute facts still served by an old same-tag runtime.
    if let Err(error) = quality_transaction.commit(reconciliation) {
        let config_rollback = restore_config(config_path, previous_config.as_deref());
        return Err(combine_failures(
            error,
            config_rollback,
            Ok(()),
            Ok(()),
            true,
        ));
    }
    after_commit_before_marker_cleanup();
    quality_store
        .clear_quality_write_block()
        .context("config and node history committed but quality writes remain blocked")?;
    Ok(ActiveNodeConfigCommit {
        backup_path,
        reconciliation,
    })
}

fn read_optional_config(config_path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(config_path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("failed to read active config {}", config_path.display())),
    }
}

fn restore_config(config_path: &Path, previous: Option<&[u8]>) -> Result<()> {
    if let Some(previous) = previous {
        return write_atomic_durable(config_path, previous).map_err(|error| {
            let (error, uncertain) = error.into_parts();
            if uncertain {
                error.context("active config rollback durability is uncertain")
            } else {
                error.context("failed to restore the previous active config")
            }
        });
    }

    remove_new_config_after_failed_commit(config_path)
}

#[cfg(unix)]
fn remove_new_config_after_failed_commit(config_path: &Path) -> Result<()> {
    remove_new_config_with_durability(config_path, sync_parent_directory)
}

#[cfg(not(unix))]
fn remove_new_config_after_failed_commit(config_path: &Path) -> Result<()> {
    remove_new_config_with_durability(config_path, |_| {
        anyhow::bail!(
            "new active config was removed, but durable directory removal is unavailable on this platform"
        )
    })
}

fn remove_new_config_with_durability(
    config_path: &Path,
    persist_removal: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    match fs::remove_file(config_path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove newly created active config {}",
                config_path.display()
            )
        })?,
    }
    persist_removal(config_path).context("failed to durably remove the newly created active config")
}

fn recover_unchanged_config_failure(
    quality_store: &BenchmarkStore,
    quality_transaction: NodeHistoryReconciliationTransaction<'_>,
    marker_created: bool,
    primary: anyhow::Error,
) -> anyhow::Error {
    recover_changed_config_failure(
        quality_store,
        quality_transaction,
        marker_created,
        primary,
        Ok(()),
    )
}

fn recover_changed_config_failure(
    quality_store: &BenchmarkStore,
    quality_transaction: NodeHistoryReconciliationTransaction<'_>,
    marker_created: bool,
    primary: anyhow::Error,
    config_rollback: Result<()>,
) -> anyhow::Error {
    let transaction_rollback = quality_transaction.rollback();
    let marker_cleanup =
        if config_rollback.is_ok() && transaction_rollback.is_ok() && marker_created {
            quality_store.clear_quality_write_block()
        } else {
            Ok(())
        };
    combine_failures(
        primary,
        config_rollback,
        transaction_rollback,
        marker_cleanup,
        false,
    )
}

fn combine_failures(
    primary: anyhow::Error,
    config_rollback: Result<()>,
    transaction_rollback: Result<()>,
    marker_cleanup: Result<()>,
    force_blocked: bool,
) -> anyhow::Error {
    let mut detail = format!("{primary:#}");
    let mut writes_blocked = force_blocked;
    if let Err(error) = config_rollback {
        writes_blocked = true;
        detail.push_str(&format!("; active config rollback failed: {error:#}"));
    }
    if let Err(error) = transaction_rollback {
        writes_blocked = true;
        detail.push_str(&format!(
            "; node-quality transaction rollback failed: {error:#}"
        ));
    }
    if let Err(error) = marker_cleanup {
        writes_blocked = true;
        detail.push_str(&format!(
            "; node-quality write block cleanup failed: {error:#}"
        ));
    }
    if writes_blocked {
        detail.push_str("; quality reads and writes remain blocked");
    }
    anyhow!(detail)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .with_context(|| format!("failed to open directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("failed to flush directory {}", parent.display()))
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveNodeCommitHooks, commit_active_node_config_locked, lock_config_mutation_for,
        paths_refer_to_same_target, remove_new_config_with_durability,
    };
    use crate::atomic_file::{DurableAtomicWriteError, write_atomic};
    use crate::config::build_default_config;
    use crate::controller::{NodeReachabilityAssessment, ProbeOutcome};
    use crate::import::{
        SubscriptionConfigRequest, commit_imported_nodes_to_active_config,
        commit_subscription_payload_to_active_config,
    };
    use crate::provider::commit_provider_payload_to_active_config;
    use crate::storage::BenchmarkStore;
    use serde_json::{Value, json};
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sing-box-tui-{label}-{nonce}"))
    }

    fn node(server: &str) -> Value {
        json!({
            "type": "trojan",
            "tag": "node-a",
            "server": server,
            "server_port": 443,
            "password": "test-secret"
        })
    }

    fn seed_history(database_path: &Path, config: &Value) {
        let store = BenchmarkStore::open(database_path).expect("open node quality store");
        store
            .reconcile_node_history(config)
            .expect("bind node identity");
        assert!(
            store
                .record_reachability_assessment(
                    "select",
                    &NodeReachabilityAssessment::from_attempts(
                        "node-a".to_string(),
                        vec![
                            ProbeOutcome::Reachable { delay_ms: 40 },
                            ProbeOutcome::Reachable { delay_ms: 41 },
                            ProbeOutcome::Reachable { delay_ms: 42 },
                        ],
                    ),
                )
                .expect("record node history")
        );
    }

    fn simulate_managed_runtime_reload(database_path: &Path) {
        // Production clears this fence only after starting sing-box under the config lock and
        // observing its controller. This test helper represents that external lifecycle event so
        // the next writer can seed facts for the newly committed identity generation.
        BenchmarkStore::open(database_path)
            .expect("open store after simulated managed reload")
            .clear_runtime_reload_required()
            .expect("clear runtime fence after simulated managed reload");
    }

    fn assert_history_cleared(database_path: &Path) {
        assert!(
            BenchmarkStore::open(database_path)
                .expect("reopen node quality store")
                .latest_reachability_assessments()
                .expect("read node history")
                .is_empty()
        );
    }

    #[test]
    fn post_rename_directory_sync_failure_keeps_quality_fail_closed() {
        let dir = temp_dir("uncertain-config-durability");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let database_path = dir.join("quality.sqlite3");
        let initial_config = build_default_config(vec![node("initial.example")]);
        fs::write(
            &config_path,
            serde_json::to_vec_pretty(&initial_config).expect("serialize initial config"),
        )
        .expect("write initial config");
        seed_history(&database_path, &initial_config);
        let refreshed_config = build_default_config(vec![node("uncertain.example")]);

        let _config_guard =
            lock_config_mutation_for(&config_path).expect("lock active config target");
        let error = commit_active_node_config_locked(
            &config_path,
            &database_path,
            || Ok(refreshed_config),
            || Ok(()),
            || Ok(None),
            ActiveNodeCommitHooks {
                before_reconcile: || {},
                after_commit_before_marker_cleanup: || {},
                write_config: |path: &Path, contents: &[u8]| {
                    write_atomic(path, contents).expect("simulate successful rename");
                    Err(DurableAtomicWriteError::DurabilityUncertain(
                        anyhow::anyhow!("injected parent directory sync failure"),
                    ))
                },
            },
        )
        .expect_err("uncertain config durability must fail closed");

        assert!(format!("{error:#}").contains("visibility or durability is uncertain"));
        assert!(
            fs::read_to_string(&config_path)
                .expect("read visible config")
                .contains("uncertain.example"),
            "the error models the post-rename state, not an unchanged destination"
        );
        let store = BenchmarkStore::open(&database_path).expect("reopen quality store");
        assert!(
            store
                .latest_reachability_assessments()
                .expect("fail-closed history query")
                .is_empty(),
            "the persistent marker must hide facts while config durability is uncertain"
        );
        assert!(
            !store
                .record_reachability_assessment(
                    "select",
                    &NodeReachabilityAssessment::from_attempts(
                        "node-a".to_string(),
                        vec![
                            ProbeOutcome::Reachable { delay_ms: 50 },
                            ProbeOutcome::Reachable { delay_ms: 51 },
                            ProbeOutcome::Reachable { delay_ms: 52 },
                        ],
                    ),
                )
                .expect("guarded write returns")
        );

        drop(store);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn identity_changing_commit_fences_facts_until_managed_runtime_reload() {
        let dir = temp_dir("runtime-reload-fence");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let database_path = dir.join("quality.sqlite3");
        let initial_config = build_default_config(vec![node("initial.example")]);
        fs::write(
            &config_path,
            serde_json::to_vec_pretty(&initial_config).expect("serialize initial config"),
        )
        .expect("write initial config");
        seed_history(&database_path, &initial_config);

        super::commit_active_node_config(
            &config_path,
            &database_path,
            || Ok(build_default_config(vec![node("changed.example")])),
            || Ok(()),
            || Ok(None),
        )
        .expect("commit identity-changing config");

        let store = BenchmarkStore::open(&database_path).expect("reopen fenced quality store");
        assert!(
            store
                .runtime_reload_required()
                .expect("inspect runtime fence")
        );
        assert!(
            !store
                .record_reachability_assessment(
                    "select",
                    &NodeReachabilityAssessment::from_attempts(
                        "node-a".to_string(),
                        vec![
                            ProbeOutcome::Reachable { delay_ms: 50 },
                            ProbeOutcome::Reachable { delay_ms: 51 },
                            ProbeOutcome::Reachable { delay_ms: 52 },
                        ],
                    ),
                )
                .expect("runtime fence rejects old-controller fact")
        );

        drop(store);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn uncertain_new_config_removal_is_reported_after_the_visible_file_is_deleted() {
        let dir = temp_dir("uncertain-new-config-removal");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        fs::write(&config_path, b"new config\n").expect("write new config");

        let error = remove_new_config_with_durability(&config_path, |_| {
            Err(anyhow::anyhow!("injected unsupported durable deletion"))
        })
        .expect_err("uncertain directory persistence must be surfaced");

        assert!(!config_path.exists());
        assert!(format!("{error:#}").contains("failed to durably remove"));
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn import_subscribe_and_provider_active_writes_clear_same_tag_history_sequentially() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("node-writer-reconciliation");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let database_path = dir.join("quality.sqlite3");
        let initial_config = build_default_config(vec![node("initial.example")]);
        fs::write(
            &config_path,
            serde_json::to_vec_pretty(&initial_config).expect("serialize initial config"),
        )
        .expect("write initial config");
        seed_history(&database_path, &initial_config);
        let stale_writer =
            BenchmarkStore::open(&database_path).expect("open old-generation writer");
        let initial_generation = stale_writer.quality_generation();

        commit_imported_nodes_to_active_config(
            &config_path,
            &config_path,
            &database_path,
            vec![node("import.example")],
            true,
            false,
            false,
        )
        .expect("Clash import active commit succeeds");
        assert_history_cleared(&database_path);
        let import_generation = BenchmarkStore::open(&database_path)
            .expect("read import generation")
            .quality_generation();
        assert!(import_generation > initial_generation);
        let import_config: Value =
            serde_json::from_slice(&fs::read(&config_path).expect("read import config"))
                .expect("parse import config");
        simulate_managed_runtime_reload(&database_path);
        seed_history(&database_path, &import_config);
        let config_alias = dir.join("active-config-alias.json");
        symlink(
            config_path.file_name().expect("config file name"),
            &config_alias,
        )
        .expect("create active config alias");

        commit_subscription_payload_to_active_config(
            &config_path,
            &config_alias,
            &database_path,
            SubscriptionConfigRequest::without_provider(
                &json!({ "outbounds": [node("subscribe.example")] }).to_string(),
                true,
                crate::config::DefaultConfigOptions {
                    include_geosite_rules: false,
                    include_tun_mode: false,
                },
            ),
        )
        .expect("subscribe active commit succeeds");
        assert_history_cleared(&database_path);
        let subscribe_generation = BenchmarkStore::open(&database_path)
            .expect("read subscribe generation")
            .quality_generation();
        assert!(subscribe_generation > import_generation);
        assert!(
            fs::symlink_metadata(&config_alias)
                .expect("inspect config alias")
                .file_type()
                .is_symlink(),
            "the subscribe active writer must preserve the alias"
        );
        let subscribe_config: Value =
            serde_json::from_slice(&fs::read(&config_path).expect("read subscribe config"))
                .expect("parse subscribe config");
        simulate_managed_runtime_reload(&database_path);
        seed_history(&database_path, &subscribe_config);

        commit_provider_payload_to_active_config(
            &config_path,
            &config_path,
            &database_path,
            &json!({ "outbounds": [node("provider.example")] }).to_string(),
            true,
            false,
            false,
        )
        .expect("provider active commit succeeds");
        assert_history_cleared(&database_path);
        let provider_generation = BenchmarkStore::open(&database_path)
            .expect("read provider generation")
            .quality_generation();
        assert!(provider_generation > subscribe_generation);
        let final_config = fs::read_to_string(&config_path).expect("read provider config");
        assert!(final_config.contains("provider.example"));
        assert!(!final_config.contains("subscribe.example"));
        assert!(
            !stale_writer
                .record_reachability_assessment(
                    "select",
                    &NodeReachabilityAssessment::from_attempts(
                        "node-a".to_string(),
                        vec![
                            ProbeOutcome::Reachable { delay_ms: 60 },
                            ProbeOutcome::Reachable { delay_ms: 61 },
                            ProbeOutcome::Reachable { delay_ms: 62 },
                        ],
                    ),
                )
                .expect("old-generation writer returns a guarded outcome"),
            "the writer bound before import must remain rejected after later active writers"
        );

        drop(stale_writer);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn config_target_comparison_resolves_symbolic_link_aliases() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("config-target-alias");
        fs::create_dir_all(&dir).expect("create temp dir");
        let target = dir.join("config.json");
        let alias = dir.join("active.json");
        fs::write(&target, b"{}\n").expect("write target");
        symlink(target.file_name().expect("target file name"), &alias).expect("create alias");

        assert!(paths_refer_to_same_target(&target, &alias).expect("compare config alias"));

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_config_symlink_is_rejected_instead_of_becoming_a_new_lock_namespace() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("dangling-config-target");
        fs::create_dir_all(&dir).expect("create temp dir");
        let missing_target = dir.join("missing.json");
        let alias = dir.join("active.json");
        symlink(
            missing_target
                .file_name()
                .expect("missing target file name"),
            &alias,
        )
        .expect("create dangling alias");

        let error = paths_refer_to_same_target(&alias, &alias)
            .expect_err("a dangling final-component symlink must fail closed");
        assert!(format!("{error:#}").contains("failed to resolve config target"));
        assert!(!missing_target.exists());

        let _ = fs::remove_dir_all(dir);
    }
}
