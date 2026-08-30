use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::controller::{NodeReachabilityAssessment, ProbeOutcome, derive_reachability_assessment};
use crate::node_quality_path::{
    QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX, QUALITY_WRITE_BLOCK_SUFFIX, node_quality_reserved_paths,
    usability_probe_lock_path,
};
use crate::sustained_quality::{NodeSustainedQuality, SustainedCompletion, SustainedProbeOutcome};

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const BENCHMARK_RETENTION: Duration = Duration::from_secs(48 * 60 * 60);
const BENCHMARK_PRUNE_INTERVAL: Duration = Duration::from_secs(10 * 60);
const BENCHMARK_PRUNE_BATCH_SIZE: usize = 50_000;
const NODE_QUALITY_SCHEMA_VERSION: i64 = 6;
const MAX_USABILITY_FACTS: usize = 4096;
const MAX_USABILITY_ID_CHARS: usize = 64;
const MAX_USABILITY_SELECTOR_CHARS: usize = 256;
const MAX_USABILITY_NODE_CHARS: usize = 256;
const MAX_USABILITY_DETAIL_CHARS: usize = 512;
const MAX_USABILITY_SUMMARY_CHARS: usize = 512;
const MAX_USABILITY_DIAGNOSTIC_CHARS: usize = 16 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
struct NodeIdentity {
    tag: String,
    fingerprint: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BenchmarkRecord<'a> {
    pub(crate) selector: &'a str,
    pub(crate) node: &'a str,
    pub(crate) filter: &'a str,
    pub(crate) delay_ms: Option<u64>,
    pub(crate) completed: bool,
    pub(crate) job_kind: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NodeLatencySample {
    pub(crate) recorded_at_ms: u64,
    pub(crate) delay_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UsabilityProbeFactRecord {
    pub(crate) node: String,
    pub(crate) usable: bool,
    pub(crate) detail: Option<String>,
}

pub(crate) struct UsabilityProbeRunFinalization<'a> {
    pub(crate) run_id: i64,
    pub(crate) generation: u64,
    pub(crate) process_lease: &'a UsabilityProbeLockLease,
    pub(crate) complete: bool,
    pub(crate) summary: Option<&'a str>,
    pub(crate) diagnostic: Option<&'a str>,
    pub(crate) facts: &'a [UsabilityProbeFactRecord],
    pub(crate) result_ttl: Option<Duration>,
}

#[derive(Clone)]
pub(crate) struct UsabilityProbeLockLease {
    run_id: i64,
    database_path: PathBuf,
    _file: Option<Arc<File>>,
}

impl UsabilityProbeLockLease {
    pub(crate) fn database_path(&self) -> &Path {
        &self.database_path
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredUsabilityProbeRun {
    pub(crate) run_id: i64,
    pub(crate) completed_at_ms: u64,
    pub(crate) expires_at_ms: Option<u64>,
    pub(crate) summary: Option<String>,
    pub(crate) results: Vec<UsabilityProbeFactRecord>,
    pub(crate) latest_attempt: Option<StoredUsabilityProbeAttempt>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredUsabilityProbeAttempt {
    pub(crate) run_id: i64,
    pub(crate) completed_at_ms: u64,
    pub(crate) complete: bool,
    pub(crate) diagnostic: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SustainedSuccessStats {
    pub(crate) successes: usize,
    pub(crate) attempts: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct NodeQuickHistory {
    pub(crate) successful_rounds: usize,
    pub(crate) rounds: usize,
    pub(crate) warm_median_ms: Option<u64>,
    pub(crate) p95_ms: Option<u64>,
    /// Lower median of reachable attempt-zero timings across the retained rounds.
    pub(crate) cold_start_ms: Option<u64>,
}

pub(crate) struct PersistedNodeQualityProjection {
    pub(crate) reachability_assessments: Vec<(String, NodeReachabilityAssessment)>,
    pub(crate) sustained_quality: Vec<(String, NodeSustainedQuality)>,
    pub(crate) quick_history: BTreeMap<(String, String), NodeQuickHistory>,
    pub(crate) sustained_stats: BTreeMap<(String, String), SustainedSuccessStats>,
}

#[derive(Default)]
struct SustainedStorageValues<'a> {
    first_byte_ms: Option<i64>,
    completion_ms: Option<i64>,
    bytes_read: Option<i64>,
    detail: Option<&'a str>,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredBenchmarkRecord {
    pub(crate) selector: String,
    pub(crate) node: String,
    pub(crate) filter: String,
    pub(crate) delay_ms: Option<u64>,
    pub(crate) completed: bool,
    pub(crate) job_kind: String,
}

pub(crate) struct BenchmarkStore {
    connection: Connection,
    database_path: PathBuf,
    last_prune_at_ms: Cell<i64>,
    quality_generation: Cell<i64>,
    observed_data_version: Cell<u64>,
    active_usability_probe_locks: RefCell<BTreeMap<i64, UsabilityProbeLockLease>>,
}

struct UsabilityProbeLockRelease<'a> {
    locks: &'a RefCell<BTreeMap<i64, UsabilityProbeLockLease>>,
    run_id: i64,
}

impl Drop for UsabilityProbeLockRelease<'_> {
    fn drop(&mut self) {
        self.locks.borrow_mut().remove(&self.run_id);
    }
}

pub(crate) struct NodeHistoryReconciliationTransaction<'store> {
    store: &'store BenchmarkStore,
    transaction: Option<Transaction<'store>>,
}

pub(crate) struct NodeQualityReconciliationLock {
    _file: Option<File>,
    database_path: PathBuf,
}

pub(crate) struct NodeQualityReadLease {
    _guard: Option<NodeQualityReconciliationLock>,
    database_path: Option<PathBuf>,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DatabasePreparation {
    Initialize,
    Current,
    MigrateV4,
    MigrateV5,
}

impl NodeQualityReadLease {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub(crate) fn for_test(generation: u64) -> Self {
        Self {
            _guard: None,
            database_path: None,
            generation,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NodeHistoryReconciliation {
    pub(crate) generation: u64,
    pub(crate) identities_changed: bool,
}

pub(crate) fn lock_node_quality_reconciliation(
    database_path: &Path,
) -> Result<NodeQualityReconciliationLock> {
    let database_path = normalize_database_path(database_path)?;
    lock_normalized_node_quality_reconciliation(database_path)
}

fn lock_normalized_node_quality_reconciliation(
    database_path: PathBuf,
) -> Result<NodeQualityReconciliationLock> {
    if database_path == Path::new(":memory:") {
        return Ok(NodeQualityReconciliationLock {
            _file: None,
            database_path,
        });
    }
    let reserved_paths = node_quality_reserved_paths(&database_path)?;
    let lock_path = reserved_paths
        .last()
        .expect("node-quality reserved path list includes its lock")
        .clone();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| {
            format!(
                "failed to open node-quality reconciliation lock {}",
                lock_path.display()
            )
        })?;
    file.lock().with_context(|| {
        format!(
            "failed to acquire node-quality reconciliation lock {}",
            lock_path.display()
        )
    })?;
    Ok(NodeQualityReconciliationLock {
        _file: Some(file),
        database_path,
    })
}

impl BenchmarkStore {
    #[cfg(test)]
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        let database_path = normalize_database_path(path.as_ref())?;
        let guard = lock_normalized_node_quality_reconciliation(database_path.clone())?;
        Self::open_while_reconciliation_locked(&database_path, &guard)
    }

    pub(crate) fn open_while_reconciliation_locked(
        path: &Path,
        guard: &NodeQualityReconciliationLock,
    ) -> Result<Self> {
        let database_path = normalize_database_path(path)?;
        if database_path != guard.database_path {
            anyhow::bail!(
                "node-quality lock does not match SQLite database {}",
                database_path.display()
            );
        }
        let preparation = prepare_node_quality_database(&database_path)?;
        let connection = Connection::open(&database_path).with_context(|| {
            format!("failed to open SQLite database {}", database_path.display())
        })?;
        configure_benchmark_connection(&connection, &database_path)?;
        let store = Self {
            connection,
            database_path,
            last_prune_at_ms: Cell::new(current_timestamp_ms()?),
            quality_generation: Cell::new(0),
            observed_data_version: Cell::new(0),
            active_usability_probe_locks: RefCell::new(BTreeMap::new()),
        };
        match preparation {
            DatabasePreparation::Initialize => store.initialize()?,
            DatabasePreparation::Current => {}
            DatabasePreparation::MigrateV4 => store.migrate_v4_schema()?,
            DatabasePreparation::MigrateV5 => store.migrate_v5_schema()?,
        }
        let generation = store.read_quality_generation_unlocked()?;
        store.quality_generation.set(generation as i64);
        store.observed_data_version.set(store.read_data_version()?);
        Ok(store)
    }

    fn read_data_version(&self) -> Result<u64> {
        let version: i64 = self
            .connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .context("failed to read node-quality SQLite data version")?;
        u64::try_from(version).context("node-quality SQLite data version is negative")
    }

    pub(crate) fn changed_data_version(&self) -> Result<Option<u64>> {
        let current = self.read_data_version()?;
        Ok((current != self.observed_data_version.get()).then_some(current))
    }

    pub(crate) fn mark_data_version_observed(&self, observed: u64) {
        self.observed_data_version.set(observed);
    }

    fn initialize(&self) -> Result<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("failed to begin node-quality schema initialization")?;
        transaction
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS benchmark_results (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    recorded_at_ms INTEGER NOT NULL,
                    selector TEXT NOT NULL,
                    node TEXT NOT NULL,
                    filter TEXT NOT NULL,
                    delay_ms INTEGER,
                    completed INTEGER NOT NULL,
                    job_kind TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_benchmark_results_recorded_at
                    ON benchmark_results(recorded_at_ms);
                DROP INDEX IF EXISTS idx_benchmark_results_selector_node;
                CREATE INDEX IF NOT EXISTS idx_benchmark_results_selector_node_recent
                    ON benchmark_results(selector, node, recorded_at_ms DESC, id DESC);
                CREATE TABLE IF NOT EXISTS reachability_assessments (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    recorded_at_ms INTEGER NOT NULL,
                    selector TEXT NOT NULL,
                    node TEXT NOT NULL,
                    complete INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS probe_attempts (
                    assessment_id INTEGER NOT NULL REFERENCES reachability_assessments(id) ON DELETE CASCADE,
                    attempt_index INTEGER NOT NULL,
                    outcome_kind TEXT NOT NULL,
                    delay_ms INTEGER,
                    detail TEXT,
                    controller_status INTEGER,
                    PRIMARY KEY (assessment_id, attempt_index)
                );
                CREATE INDEX IF NOT EXISTS idx_reachability_assessments_selector_node_recent
                    ON reachability_assessments(selector, node, recorded_at_ms DESC, id DESC);
                CREATE TABLE IF NOT EXISTS node_identities (
                    tag TEXT PRIMARY KEY,
                    fingerprint TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS node_quality_state (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    generation INTEGER NOT NULL,
                    identities_initialized INTEGER NOT NULL
                );
                INSERT OR IGNORE INTO node_quality_state (
                    singleton, generation, identities_initialized
                ) VALUES (1, 0, 0);
                CREATE TABLE IF NOT EXISTS sustained_probe_results (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    recorded_at_ms INTEGER NOT NULL,
                    selector TEXT NOT NULL,
                    node_tag TEXT NOT NULL,
                    target_identity TEXT NOT NULL,
                    outcome_kind TEXT NOT NULL,
                    first_byte_ms INTEGER,
                    completion_ms INTEGER,
                    bytes_read INTEGER,
                    detail TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_sustained_probe_selector_node_target_recent
                    ON sustained_probe_results(
                        selector, node_tag, target_identity, recorded_at_ms DESC, id DESC
                    );
                CREATE TABLE IF NOT EXISTS usability_probe_runs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    started_at_ms INTEGER NOT NULL,
                    completed_at_ms INTEGER,
                    criterion_id TEXT NOT NULL,
                    selector TEXT NOT NULL,
                    generation INTEGER NOT NULL,
                    status TEXT NOT NULL,
                    summary TEXT,
                    diagnostic TEXT,
                    expires_at_ms INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_usability_probe_runs_criterion_selector_recent
                    ON usability_probe_runs(
                        criterion_id, selector, status, generation, completed_at_ms DESC, id DESC
                    );
                CREATE TABLE IF NOT EXISTS usability_probe_facts (
                    run_id INTEGER NOT NULL REFERENCES usability_probe_runs(id) ON DELETE CASCADE,
                    sequence INTEGER NOT NULL,
                    node_tag TEXT NOT NULL REFERENCES node_identities(tag) ON DELETE CASCADE,
                    usable INTEGER NOT NULL,
                    detail TEXT,
                    PRIMARY KEY (run_id, sequence)
                );
                CREATE TABLE IF NOT EXISTS usability_probe_results (
                    run_id INTEGER NOT NULL REFERENCES usability_probe_runs(id) ON DELETE CASCADE,
                    node_tag TEXT NOT NULL REFERENCES node_identities(tag) ON DELETE CASCADE,
                    usable INTEGER NOT NULL,
                    detail TEXT,
                    PRIMARY KEY (run_id, node_tag)
                );
                "#,
            )
            .context("failed to initialize benchmark_results SQLite schema")?;
        transaction
            .pragma_update(None, "user_version", NODE_QUALITY_SCHEMA_VERSION)
            .context("failed to set node-quality schema version")?;
        transaction
            .commit()
            .context("failed to commit node-quality schema initialization")
    }

    fn migrate_v4_schema(&self) -> Result<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Exclusive)
                .context("failed to begin exclusive node-quality v4-to-v6 migration")?;
        // Revalidate after acquiring SQLite's schema-write lock. The outer reconciliation lock
        // serializes cooperating processes, while this second check prevents an untrusted v4 file
        // from being upgraded if it changed between read-only classification and the transaction.
        if !v4_schema_is_recognized(&transaction)? {
            anyhow::bail!("node-quality v4 schema changed before migration");
        }
        transaction
            .execute_batch(USABILITY_PROBE_RUNS_TABLE_SQL)
            .context("failed to create usability-probe run table during v6 migration")?;
        transaction
            .execute_batch(
                "CREATE INDEX idx_usability_probe_runs_criterion_selector_recent \
                 ON usability_probe_runs(criterion_id, selector, status, generation, completed_at_ms DESC, id DESC)",
            )
            .context("failed to create usability-probe run index during v6 migration")?;
        transaction
            .execute_batch(USABILITY_PROBE_FACTS_TABLE_SQL)
            .context("failed to create usability-probe fact table during v6 migration")?;
        transaction
            .execute_batch(USABILITY_PROBE_RESULTS_TABLE_SQL)
            .context("failed to create usability-probe result table during v6 migration")?;
        transaction
            .pragma_update(None, "user_version", NODE_QUALITY_SCHEMA_VERSION)
            .context("failed to set node-quality schema version after v4 migration")?;
        if !current_schema_is_recognized(&transaction)? {
            anyhow::bail!("node-quality v4-to-v6 migration produced an unrecognized schema");
        }
        transaction
            .commit()
            .context("failed to commit node-quality v4-to-v6 migration")
    }

    fn migrate_v5_schema(&self) -> Result<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("failed to begin node-quality v5-to-v6 migration")?;
        if !v5_schema_is_recognized(&transaction)? {
            anyhow::bail!("node-quality v5 schema changed before migration");
        }
        transaction
            .execute_batch("ALTER TABLE usability_probe_runs ADD COLUMN expires_at_ms INTEGER")
            .context("failed to add usability result expiry")?;
        transaction
            .pragma_update(None, "user_version", NODE_QUALITY_SCHEMA_VERSION)
            .context("failed to set node-quality schema version after v5 migration")?;
        if !current_schema_is_recognized(&transaction)? {
            anyhow::bail!("node-quality v5-to-v6 migration produced an unrecognized schema");
        }
        transaction
            .commit()
            .context("failed to commit node-quality v5-to-v6 migration")
    }

    pub(crate) fn begin_node_history_reconciliation(
        &self,
    ) -> Result<NodeHistoryReconciliationTransaction<'_>> {
        // A durable marker is created before the active config changes. FULL synchronization on
        // the reconciliation connection then guarantees that, once the marker can be removed,
        // the new identities and generation have reached stable storage as well.
        self.connection
            .pragma_update(None, "synchronous", "FULL")
            .context("failed to enable durable node-quality reconciliation commits")?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("failed to acquire the node-quality reconciliation lock")?;
        Ok(NodeHistoryReconciliationTransaction {
            store: self,
            transaction: Some(transaction),
        })
    }

    /// Creates the cross-process fail-closed marker while the caller holds the quality DB write
    /// lock. Returns true when this call created the marker and false when an earlier failed
    /// reconciliation already left it in place.
    pub(crate) fn ensure_quality_writes_blocked(&self) -> Result<bool> {
        self.ensure_durable_marker_with(
            QUALITY_WRITE_BLOCK_SUFFIX,
            "node-quality write block",
            sync_parent_directory,
        )
    }

    /// Persists the fact that the active config has moved to a new identity generation while the
    /// live sing-box process may still be serving the previous same-tag outbounds.
    pub(crate) fn ensure_runtime_reload_required(&self) -> Result<bool> {
        self.ensure_durable_marker_with(
            QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX,
            "node-quality runtime reload fence",
            sync_parent_directory,
        )
    }

    #[cfg(test)]
    fn ensure_quality_writes_blocked_with<SyncParent>(
        &self,
        sync_parent: SyncParent,
    ) -> Result<bool>
    where
        SyncParent: FnOnce(&Path) -> Result<()>,
    {
        self.ensure_durable_marker_with(
            QUALITY_WRITE_BLOCK_SUFFIX,
            "node-quality write block",
            sync_parent,
        )
    }

    fn ensure_durable_marker_with<SyncParent>(
        &self,
        suffix: &str,
        label: &str,
        sync_parent: SyncParent,
    ) -> Result<bool>
    where
        SyncParent: FnOnce(&Path) -> Result<()>,
    {
        let Some(path) = self.marker_path(suffix) else {
            return Ok(false);
        };
        let (file, created) = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => (file, true),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                (open_existing_quality_marker(&path, label)?, false)
            }
            Err(error) => Err(error)
                .with_context(|| format!("failed to create {label} {}", path.display()))?,
        };
        file.sync_all()
            .with_context(|| format!("failed to flush {label} {}", path.display()))?;
        sync_parent(&path)
            .with_context(|| format!("failed to persist {label} {}", path.display()))?;
        Ok(created)
    }

    pub(crate) fn clear_quality_write_block(&self) -> Result<()> {
        self.clear_marker(
            QUALITY_WRITE_BLOCK_SUFFIX,
            "node-quality write block",
            false,
        )
    }

    pub(crate) fn clear_runtime_reload_required(&self) -> Result<()> {
        self.clear_marker(
            QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX,
            "node-quality runtime reload fence",
            true,
        )
    }

    fn clear_marker(&self, suffix: &str, label: &str, durable: bool) -> Result<()> {
        let Some(path) = self.marker_path(suffix) else {
            return Ok(());
        };
        match fs::remove_file(&path) {
            Ok(()) => {
                if durable {
                    sync_parent_directory(&path)
                        .with_context(|| format!("failed to persist cleared {label}"))?;
                }
                Ok(())
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to clear {label} {}", path.display()))
            }
        }
    }

    fn marker_path(&self, suffix: &str) -> Option<PathBuf> {
        if self.database_path == Path::new(":memory:") {
            return None;
        }
        let mut path = self.database_path.as_os_str().to_os_string();
        path.push(suffix);
        Some(PathBuf::from(path))
    }

    fn quality_writes_blocked(&self) -> Result<bool> {
        Ok(self.quality_reads_blocked()? || self.runtime_reload_required()?)
    }

    fn quality_reads_blocked(&self) -> Result<bool> {
        self.marker_exists(QUALITY_WRITE_BLOCK_SUFFIX, "node-quality write block")
    }

    pub(crate) fn runtime_reload_required(&self) -> Result<bool> {
        self.marker_exists(
            QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX,
            "node-quality runtime reload fence",
        )
    }

    fn marker_exists(&self, suffix: &str, label: &str) -> Result<bool> {
        let Some(path) = self.marker_path(suffix) else {
            return Ok(false);
        };
        match fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => {
                Err(error).with_context(|| format!("failed to inspect {label} {}", path.display()))
            }
        }
    }

    pub(crate) fn quality_session_current(&self) -> Result<bool> {
        let _cross_process_guard = lock_node_quality_reconciliation(&self.database_path)?;
        self.quality_session_current_while_locked()
    }

    pub(crate) fn acquire_quality_read_lease(&self) -> Result<Option<NodeQualityReadLease>> {
        let guard = lock_node_quality_reconciliation(&self.database_path)?;
        if !self.quality_session_current_while_locked()? {
            return Ok(None);
        }
        Ok(Some(NodeQualityReadLease {
            database_path: Some(guard.database_path.clone()),
            _guard: Some(guard),
            generation: self.quality_generation(),
        }))
    }

    pub(crate) fn validate_quality_read_lease(&self, lease: &NodeQualityReadLease) -> Result<()> {
        // Generation alone is not globally unique: bind both the canonical database identity and
        // generation so a lease cannot authorize facts after reconciliation or from another store.
        if lease.database_path.as_ref() != Some(&self.database_path)
            || lease.generation != self.quality_generation()
        {
            anyhow::bail!("node-quality read lease does not match this store generation");
        }
        Ok(())
    }

    pub(crate) fn node_quality_projection_with_lease(
        &self,
        lease: &NodeQualityReadLease,
        target_identity: &str,
        history_limit: usize,
    ) -> Result<PersistedNodeQualityProjection> {
        self.validate_quality_read_lease(lease)?;
        // The filesystem lease freezes identity/generation, while this SQLite read transaction
        // freezes ordinary fact writes across every query. Without both, foreground could combine
        // reachability from one background commit with sustained/history rows from another.
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)
                .context("failed to begin node-quality projection read transaction")?;
        let reachability_assessments = self.latest_reachability_assessments_while_locked()?;
        let sustained_quality = self.latest_sustained_quality_while_locked(target_identity)?;
        let keys = reachability_assessments
            .iter()
            .map(|(selector, assessment)| (selector.clone(), assessment.name.clone()))
            .chain(
                sustained_quality
                    .iter()
                    .map(|(selector, quality)| (selector.clone(), quality.name.clone())),
            )
            .collect::<BTreeSet<_>>();
        let mut quick_history = BTreeMap::new();
        let mut sustained_stats = BTreeMap::new();
        for (selector, node) in keys {
            quick_history.insert(
                (selector.clone(), node.clone()),
                self.node_quick_history_while_locked(&selector, &node, history_limit)?,
            );
            sustained_stats.insert(
                (selector.clone(), node.clone()),
                self.sustained_success_stats_while_locked(
                    &selector,
                    &node,
                    target_identity,
                    history_limit,
                )?,
            );
        }
        transaction
            .commit()
            .context("failed to finish node-quality projection read transaction")?;
        Ok(PersistedNodeQualityProjection {
            reachability_assessments,
            sustained_quality,
            quick_history,
            sustained_stats,
        })
    }

    /// Keeps only tags bound to this store's generation while the caller holds its read lease.
    ///
    /// The pre-spawn membership check is deliberately performed under the same cross-process
    /// lease used to freeze the job generation. The final INSERT repeats the check because a
    /// worker can outlive this lease and must still fail closed after later reconciliation.
    pub(crate) fn retain_bound_node_tags(
        &self,
        lease: &NodeQualityReadLease,
        tags: &mut Vec<String>,
    ) -> Result<()> {
        self.validate_quality_read_lease(lease)?;
        let mut statement = self
            .connection
            .prepare("SELECT tag FROM node_identities")
            .context("failed to prepare bound node membership query")?;
        let bound = statement
            .query_map([], |row| row.get::<_, String>(0))
            .context("failed to query bound node membership")?
            .collect::<rusqlite::Result<BTreeSet<_>>>()
            .context("failed to read bound node membership")?;
        tags.retain(|tag| bound.contains(tag));
        Ok(())
    }

    fn quality_session_current_while_locked(&self) -> Result<bool> {
        if self.quality_writes_blocked()? {
            return Ok(false);
        }
        self.connection
            .query_row(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM node_quality_state
                    WHERE singleton = 1
                        AND generation = ?1
                        AND identities_initialized = 1
                )
                "#,
                [self.quality_generation.get()],
                |row| row.get::<_, bool>(0),
            )
            .context("failed to validate node-quality session generation")
    }

    pub(crate) fn record_reachability_assessment(
        &self,
        selector: &str,
        assessment: &NodeReachabilityAssessment,
    ) -> Result<bool> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("failed to begin reachability assessment transaction")?;
        if self.quality_writes_blocked()? {
            return Ok(false);
        }
        let inserted = transaction
            .execute(
                r#"
            INSERT INTO reachability_assessments (
                recorded_at_ms, selector, node, complete
            )
            SELECT ?1, ?2, ?3, ?4
            WHERE EXISTS (
                SELECT 1 FROM node_quality_state
                WHERE singleton = 1
                    AND generation = ?5
                    AND identities_initialized = 1
                    -- A fresh store can share the current generation while receiving facts from
                    -- an external controller with a different config. Generation fencing alone
                    -- cannot attribute such a same-process fact to this identity snapshot.
                    AND EXISTS (SELECT 1 FROM node_identities WHERE tag = ?3)
            )
            "#,
                params![
                    current_timestamp_ms()?,
                    selector,
                    assessment.name,
                    i64::from(assessment.assessment.is_some()),
                    self.quality_generation.get()
                ],
            )
            .context("failed to insert reachability assessment")?;
        if inserted == 0 {
            return Ok(false);
        }
        let assessment_id = transaction.last_insert_rowid();
        for (index, outcome) in assessment.attempts.iter().enumerate() {
            let (delay_ms, detail, status): (Option<i64>, Option<&str>, Option<i64>) = match outcome
            {
                ProbeOutcome::Reachable { delay_ms } => (Some(*delay_ms as i64), None, None),
                ProbeOutcome::Timeout => (None, None, None),
                ProbeOutcome::TransportFailure { detail } => (None, Some(detail), None),
                ProbeOutcome::ControllerFailure { status } => (None, None, Some(*status as i64)),
                ProbeOutcome::InvalidMeasurement | ProbeOutcome::Cancelled => (None, None, None),
            };
            let kind = outcome.storage_kind();
            transaction.execute(
                "INSERT INTO probe_attempts (assessment_id, attempt_index, outcome_kind, delay_ms, detail, controller_status) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![assessment_id, index as i64, kind, delay_ms, detail, status],
            ).context("failed to insert probe attempt")?;
        }
        transaction
            .commit()
            .context("failed to commit reachability assessment")?;
        Ok(true)
    }

    pub(crate) fn latest_reachability_assessments(
        &self,
    ) -> Result<Vec<(String, NodeReachabilityAssessment)>> {
        let _cross_process_guard = lock_node_quality_reconciliation(&self.database_path)?;
        if !self.quality_session_current_while_locked()? {
            return Ok(Vec::new());
        }
        self.latest_reachability_assessments_while_locked()
    }

    fn latest_reachability_assessments_while_locked(
        &self,
    ) -> Result<Vec<(String, NodeReachabilityAssessment)>> {
        let mut statement = self
            .connection
            .prepare(
                r#"
            SELECT a.selector, a.node, t.outcome_kind, t.delay_ms, t.detail, t.controller_status
            FROM reachability_assessments a
            JOIN probe_attempts t ON t.assessment_id = a.id
            WHERE a.id IN (
                SELECT id FROM reachability_assessments newer
                WHERE newer.selector = a.selector AND newer.node = a.node AND newer.complete = 1
                ORDER BY recorded_at_ms DESC, id DESC LIMIT 1
            )
            ORDER BY a.selector, a.node, t.attempt_index
            "#,
            )
            .context("failed to prepare latest reachability assessment query")?;
        let rows = statement
            .query_map([], |row| {
                let kind: String = row.get(2)?;
                let delay_ms: Option<i64> = row.get(3)?;
                let detail: Option<String> = row.get(4)?;
                let status: Option<i64> = row.get(5)?;
                let outcome = ProbeOutcome::from_storage(
                    &kind,
                    delay_ms.map(|value| value as u64),
                    detail,
                    status.map(|value| value as u16),
                )
                .map_err(|message| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        std::io::Error::new(std::io::ErrorKind::InvalidData, message).into(),
                    )
                })?;
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, outcome))
            })
            .context("failed to query latest reachability assessments")?;
        let mut grouped: Vec<(String, NodeReachabilityAssessment)> = Vec::new();
        for row in rows {
            let (selector, node, outcome) =
                row.context("failed to read reachability assessment row")?;
            if let Some((last_selector, assessment)) = grouped.last_mut()
                && *last_selector == selector
                && assessment.name == node
            {
                assessment.attempts.push(outcome);
                assessment.assessment = derive_reachability_assessment(&assessment.attempts);
            } else {
                grouped.push((
                    selector,
                    NodeReachabilityAssessment {
                        name: node,
                        attempts: vec![outcome],
                        assessment: None,
                    },
                ));
            }
        }
        Ok(grouped)
    }

    /// Reconciles stored facts against every tagged outbound in a committed config.
    ///
    /// A fact is retained only when a previously persisted identity has the same tag and
    /// fingerprint. Raw stores reject writes until the first identity binding, and any facts from
    /// a pre-v4 database are discarded by the accepted whole-database reset policy. New fact
    /// tables should reference `node_identities(tag) ON DELETE CASCADE`; tables without that
    /// foreign key must also be added to `delete_unretained_node_facts`.
    #[cfg(test)]
    pub(crate) fn reconcile_node_history(
        &self,
        committed_config: &Value,
    ) -> Result<NodeHistoryReconciliation> {
        let cross_process_guard = lock_node_quality_reconciliation(&self.database_path)?;
        let reconciliation = self.bind_node_history_while_reconciliation_locked(
            &cross_process_guard,
            committed_config,
        )?;
        // Unit tests use this helper as their stand-in for a runtime that has loaded the supplied
        // config. Production callers must instead clear the fence through readiness observation.
        self.clear_runtime_reload_required()?;
        Ok(reconciliation)
    }

    pub(crate) fn bind_node_history_while_reconciliation_locked(
        &self,
        guard: &NodeQualityReconciliationLock,
        committed_config: &Value,
    ) -> Result<NodeHistoryReconciliation> {
        if self.database_path != guard.database_path {
            anyhow::bail!(
                "node-quality lock does not match SQLite database {}",
                self.database_path.display()
            );
        }
        let transaction = self.begin_node_history_reconciliation()?;
        let marker_created = self.ensure_quality_writes_blocked()?;
        let reconciliation = match transaction.apply(committed_config) {
            Ok(reconciliation) => reconciliation,
            Err(error) => {
                let rollback = transaction.rollback();
                if rollback.is_ok() && marker_created {
                    self.clear_quality_write_block()?;
                }
                return match rollback {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(anyhow!(
                        "{error:#}; node-quality transaction rollback failed: {rollback_error:#}; quality writes remain blocked"
                    )),
                };
            }
        };
        if reconciliation.identities_changed
            && let Err(error) = self.ensure_runtime_reload_required()
        {
            let rollback = transaction.rollback();
            if rollback.is_ok() && marker_created {
                self.clear_quality_write_block()?;
            }
            return match rollback {
                Ok(()) => Err(error).context(
                    "failed to fence node-quality persistence before publishing changed identities",
                ),
                Err(rollback_error) => Err(anyhow!(
                    "failed to fence changed node identities: {error:#}; node-quality transaction rollback failed: {rollback_error:#}; quality writes remain blocked"
                )),
            };
        }
        // Startup may discover identity drift without going through a config mutation. The
        // durable runtime fence must therefore exist before the new generation becomes visible,
        // just as it does for subscription refreshes.
        transaction.commit(reconciliation)?;
        self.clear_quality_write_block()
            .context("node history committed but quality writes remain blocked")?;
        Ok(reconciliation)
    }

    pub(crate) fn record_sustained_quality(
        &self,
        selector: &str,
        target_identity: &str,
        result: &NodeSustainedQuality,
    ) -> Result<bool> {
        let values = match &result.outcome {
            SustainedProbeOutcome::Completed(completion) => SustainedStorageValues {
                first_byte_ms: Some(completion.first_byte_ms as i64),
                completion_ms: Some(completion.completion_ms as i64),
                bytes_read: Some(completion.bytes_read as i64),
                detail: None,
            },
            SustainedProbeOutcome::TransferFailed { .. } => SustainedStorageValues {
                detail: Some("sustained transfer failed"),
                ..SustainedStorageValues::default()
            },
            SustainedProbeOutcome::RuntimeFailed { .. } => SustainedStorageValues {
                detail: Some("isolated runtime unavailable"),
                ..SustainedStorageValues::default()
            },
            SustainedProbeOutcome::Cancelled => SustainedStorageValues::default(),
        };
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("failed to begin sustained-quality transaction")?;
        if self.quality_writes_blocked()? {
            return Ok(false);
        }
        let inserted = transaction
            .execute(
                r#"
                INSERT INTO sustained_probe_results (
                    recorded_at_ms, selector, node_tag, target_identity, outcome_kind,
                    first_byte_ms, completion_ms, bytes_read, detail
                )
                SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9
                WHERE EXISTS (
                    SELECT 1 FROM node_quality_state
                    WHERE singleton = 1
                        AND generation = ?10
                        AND identities_initialized = 1
                )
                  AND EXISTS (SELECT 1 FROM node_identities WHERE tag = ?3)
                "#,
                params![
                    current_timestamp_ms()?,
                    selector,
                    result.name,
                    target_identity,
                    result.outcome.storage_kind(),
                    values.first_byte_ms,
                    values.completion_ms,
                    values.bytes_read,
                    values.detail,
                    self.quality_generation.get(),
                ],
            )
            .context("failed to insert sustained probe result")?;
        if inserted == 0 {
            return Ok(false);
        }
        transaction
            .commit()
            .context("failed to commit sustained probe result")?;
        Ok(true)
    }

    pub(crate) fn latest_sustained_quality(
        &self,
        target_identity: &str,
    ) -> Result<Vec<(String, NodeSustainedQuality)>> {
        let _cross_process_guard = lock_node_quality_reconciliation(&self.database_path)?;
        if !self.quality_session_current_while_locked()? {
            return Ok(Vec::new());
        }
        self.latest_sustained_quality_while_locked(target_identity)
    }

    fn latest_sustained_quality_while_locked(
        &self,
        target_identity: &str,
    ) -> Result<Vec<(String, NodeSustainedQuality)>> {
        let mut statement = self
            .connection
            .prepare(
                r#"
                SELECT s.selector, s.node_tag, s.outcome_kind, s.first_byte_ms,
                       s.completion_ms, s.bytes_read, s.detail
                FROM sustained_probe_results s
                WHERE s.target_identity = ?1
                  AND s.id = (
                    SELECT newer.id
                    FROM sustained_probe_results newer
                    WHERE newer.selector = s.selector
                      AND newer.node_tag = s.node_tag
                      AND newer.target_identity = s.target_identity
                      AND newer.outcome_kind IN ('completed', 'transfer_failed')
                    ORDER BY newer.recorded_at_ms DESC, newer.id DESC
                    LIMIT 1
                )
                ORDER BY s.selector, s.node_tag
                "#,
            )
            .context("failed to prepare latest sustained-quality query")?;
        let rows = statement
            .query_map(params![target_identity], |row| {
                let kind: String = row.get(2)?;
                let first_byte_ms: Option<i64> = row.get(3)?;
                let completion_ms: Option<i64> = row.get(4)?;
                let bytes_read: Option<i64> = row.get(5)?;
                let detail: Option<String> = row.get(6)?;
                let outcome = sustained_outcome_from_storage(
                    &kind,
                    first_byte_ms,
                    completion_ms,
                    bytes_read,
                    detail,
                )?;
                Ok((
                    row.get::<_, String>(0)?,
                    NodeSustainedQuality {
                        name: row.get(1)?,
                        outcome,
                    },
                ))
            })
            .context("failed to query latest sustained quality")?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row.context("failed to read sustained-quality row")?);
        }
        Ok(results)
    }

    pub(crate) fn sustained_success_stats(
        &self,
        selector: &str,
        node: &str,
        target_identity: &str,
        limit: usize,
    ) -> Result<SustainedSuccessStats> {
        let _cross_process_guard = lock_node_quality_reconciliation(&self.database_path)?;
        if !self.quality_session_current_while_locked()? {
            return Ok(SustainedSuccessStats::default());
        }
        self.sustained_success_stats_while_locked(selector, node, target_identity, limit)
    }

    pub(crate) fn sustained_success_stats_with_lease(
        &self,
        lease: &NodeQualityReadLease,
        selector: &str,
        node: &str,
        target_identity: &str,
        limit: usize,
    ) -> Result<SustainedSuccessStats> {
        if lease.database_path.as_ref() != Some(&self.database_path)
            || lease.generation != self.quality_generation()
        {
            anyhow::bail!("node-quality read lease does not match this store generation");
        }
        self.sustained_success_stats_while_locked(selector, node, target_identity, limit)
    }

    fn sustained_success_stats_while_locked(
        &self,
        selector: &str,
        node: &str,
        target_identity: &str,
        limit: usize,
    ) -> Result<SustainedSuccessStats> {
        let mut statement = self
            .connection
            .prepare(
                r#"
                SELECT outcome_kind
                FROM sustained_probe_results
                WHERE selector = ?1
                  AND node_tag = ?2
                  AND target_identity = ?3
                  AND outcome_kind IN ('completed', 'transfer_failed')
                ORDER BY recorded_at_ms DESC, id DESC
                LIMIT ?4
                "#,
            )
            .context("failed to prepare sustained-success query")?;
        let rows = statement
            .query_map(
                params![selector, node, target_identity, limit as i64],
                |row| row.get::<_, String>(0),
            )
            .context("failed to query sustained success")?;
        let mut stats = SustainedSuccessStats::default();
        for row in rows {
            let kind = row.context("failed to read sustained-success row")?;
            stats.attempts += 1;
            stats.successes += usize::from(kind == "completed");
        }
        Ok(stats)
    }

    pub(crate) fn node_quick_history(
        &self,
        selector: &str,
        node: &str,
        limit: usize,
    ) -> Result<NodeQuickHistory> {
        let _cross_process_guard = lock_node_quality_reconciliation(&self.database_path)?;
        if !self.quality_session_current_while_locked()? {
            return Ok(NodeQuickHistory::default());
        }
        self.node_quick_history_while_locked(selector, node, limit)
    }

    pub(crate) fn node_quick_history_with_lease(
        &self,
        lease: &NodeQualityReadLease,
        selector: &str,
        node: &str,
        limit: usize,
    ) -> Result<NodeQuickHistory> {
        if lease.database_path.as_ref() != Some(&self.database_path)
            || lease.generation != self.quality_generation()
        {
            anyhow::bail!("node-quality read lease does not match this store generation");
        }
        self.node_quick_history_while_locked(selector, node, limit)
    }

    fn node_quick_history_while_locked(
        &self,
        selector: &str,
        node: &str,
        limit: usize,
    ) -> Result<NodeQuickHistory> {
        let mut statement = self
            .connection
            .prepare(
                r#"
                SELECT a.id, t.attempt_index, t.outcome_kind, t.delay_ms
                FROM reachability_assessments a
                JOIN probe_attempts t ON t.assessment_id = a.id
                WHERE a.selector = ?1
                  AND a.node = ?2
                  AND a.complete = 1
                  AND a.id IN (
                      SELECT newer.id
                      FROM reachability_assessments newer
                      WHERE newer.selector = ?1
                        AND newer.node = ?2
                        AND newer.complete = 1
                      ORDER BY newer.recorded_at_ms DESC, newer.id DESC
                      LIMIT ?3
                  )
                ORDER BY a.recorded_at_ms DESC, a.id DESC, t.attempt_index
                "#,
            )
            .context("failed to prepare quick-history query")?;
        let rows = statement
            .query_map(params![selector, node, limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })
            .context("failed to query quick history")?;
        let mut rounds = Vec::<(i64, usize)>::new();
        let mut all_delays = Vec::new();
        let mut warm_delays = Vec::new();
        let mut cold_delays = Vec::new();
        for row in rows {
            let (assessment_id, attempt_index, kind, delay_ms) =
                row.context("failed to read quick-history row")?;
            if rounds.last().is_none_or(|(id, _)| *id != assessment_id) {
                rounds.push((assessment_id, 0));
            }
            if kind == "reachable" {
                rounds.last_mut().expect("round was inserted").1 += 1;
                if let Some(delay_ms) = delay_ms.map(|value| value as u64) {
                    all_delays.push(delay_ms);
                    if attempt_index == 0 {
                        cold_delays.push(delay_ms);
                    } else {
                        warm_delays.push(delay_ms);
                    }
                }
            }
        }
        let successful_rounds = rounds
            .iter()
            .filter(|(_, reachable)| *reachable >= 2)
            .count();
        if warm_delays.is_empty() {
            warm_delays.clone_from(&all_delays);
        }
        Ok(NodeQuickHistory {
            successful_rounds,
            rounds: rounds.len(),
            warm_median_ms: median(&mut warm_delays),
            p95_ms: percentile_95(&mut all_delays),
            cold_start_ms: median(&mut cold_delays),
        })
    }

    pub(crate) fn record_benchmark(&self, record: &BenchmarkRecord<'_>) -> Result<bool> {
        let recorded_at_ms = current_timestamp_ms()?;
        let delay_ms = record.delay_ms.map(|value| value as i64);
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("failed to begin benchmark result transaction")?;
        if self.quality_writes_blocked()? {
            return Ok(false);
        }
        let inserted = transaction
            .execute(
                r#"
                INSERT INTO benchmark_results (
                    recorded_at_ms,
                    selector,
                    node,
                    filter,
                    delay_ms,
                    completed,
                    job_kind
                )
                SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
                WHERE EXISTS (
                    SELECT 1 FROM node_quality_state
                    WHERE singleton = 1
                        AND generation = ?8
                        AND identities_initialized = 1
                        -- Keep the membership check in the same SQLite statement as the insert so
                        -- reconciliation cannot race an out-of-snapshot tag into persisted facts.
                        AND EXISTS (SELECT 1 FROM node_identities WHERE tag = ?3)
                )
                "#,
                params![
                    recorded_at_ms,
                    record.selector,
                    record.node,
                    record.filter,
                    delay_ms,
                    if record.completed { 1_i64 } else { 0_i64 },
                    record.job_kind,
                    self.quality_generation.get(),
                ],
            )
            .context("failed to insert benchmark result into SQLite")?;
        if inserted == 0 {
            return Ok(false);
        }
        transaction
            .commit()
            .context("failed to commit benchmark result")?;
        if let Err(error) = self.maybe_prune_benchmark_history(recorded_at_ms) {
            eprintln!("warning: failed to prune old benchmark history: {error:#}");
        }
        Ok(true)
    }

    pub(crate) fn begin_usability_probe_run(
        &self,
        criterion_id: &str,
        selector: &str,
        expected_generation: u64,
    ) -> Result<Option<(i64, u64)>> {
        if criterion_id.is_empty() || criterion_id.chars().count() > MAX_USABILITY_ID_CHARS {
            anyhow::bail!("usability criterion id must contain 1 to 64 characters");
        }
        if selector.is_empty() || selector.chars().count() > MAX_USABILITY_SELECTOR_CHARS {
            anyhow::bail!("usability selector must contain 1 to 256 characters");
        }
        if !self.active_usability_probe_locks.borrow().is_empty() {
            anyhow::bail!("a custom usability probe is already running in this process");
        }
        let process_lock = if self.database_path == Path::new(":memory:") {
            None
        } else {
            let lock_path = usability_probe_lock_path(&self.database_path)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .with_context(|| {
                    format!(
                        "failed to open usability-probe lock {}",
                        lock_path.display()
                    )
                })?;
            file.try_lock().with_context(|| {
                format!(
                    "another foreground or background usability probe is already running for {}",
                    self.database_path.display()
                )
            })?;
            Some(file)
        };
        let _cross_process_guard = lock_node_quality_reconciliation(&self.database_path)?;
        if !self.quality_session_current_while_locked()? {
            return Ok(None);
        }
        // The runtime receipt's generation and per-criterion no-overlap rule are checked inside
        // the same immediate transaction that creates the running row. The database is the only
        // authority shared by foreground and headless processes, so an in-memory job flag cannot
        // safely protect paid/application probes from being launched twice.
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("failed to begin usability-probe run transaction")?;
        // WHY: successfully acquiring the OS lock proves no cooperating process still owns a
        // custom probe. Recover a crash/SIGKILL remnant before inserting the new audit row; unlike
        // a time lease, this never mistakes a legitimately slow paid probe for a dead owner.
        transaction
            .execute(
                r#"
                UPDATE usability_probe_runs
                SET completed_at_ms = ?1, status = 'incomplete',
                    diagnostic = 'probe owner exited before finalization'
                WHERE criterion_id = ?2 AND selector = ?3 AND status = 'running'
                "#,
                params![current_timestamp_ms()?, criterion_id, selector],
            )
            .context("failed to recover an abandoned usability-probe run")?;
        let inserted = transaction
            .execute(
                r#"
                INSERT INTO usability_probe_runs (
                    started_at_ms, criterion_id, selector, generation, status
                )
                SELECT ?1, ?2, ?3, ?4, 'running'
                WHERE EXISTS (
                    SELECT 1 FROM node_quality_state
                    WHERE singleton = 1
                      AND generation = ?4
                      AND identities_initialized = 1
                )
                "#,
                params![
                    current_timestamp_ms()?,
                    criterion_id,
                    selector,
                    expected_generation as i64,
                ],
            )
            .context("failed to create usability-probe run")?;
        if inserted == 0 {
            return Ok(None);
        }
        let run_id = transaction.last_insert_rowid();
        transaction
            .commit()
            .context("failed to commit usability-probe run start")?;
        self.active_usability_probe_locks.borrow_mut().insert(
            run_id,
            UsabilityProbeLockLease {
                run_id,
                database_path: self.database_path.clone(),
                _file: process_lock.map(Arc::new),
            },
        );
        Ok(Some((run_id, expected_generation)))
    }

    pub(crate) fn usability_probe_lock_lease(
        &self,
        run_id: i64,
    ) -> Result<UsabilityProbeLockLease> {
        self.active_usability_probe_locks
            .borrow()
            .get(&run_id)
            .cloned()
            .with_context(|| format!("usability-probe run {run_id} has no active process lease"))
    }

    #[cfg(test)]
    pub(crate) fn finish_usability_probe_run(
        &self,
        run_id: i64,
        generation: u64,
        requested_complete: bool,
        summary: Option<&str>,
        diagnostic: Option<&str>,
        facts: &[UsabilityProbeFactRecord],
    ) -> Result<bool> {
        let process_lease = self.usability_probe_lock_lease(run_id)?;
        self.finish_usability_probe_run_with_ttl(UsabilityProbeRunFinalization {
            run_id,
            generation,
            process_lease: &process_lease,
            complete: requested_complete,
            summary,
            diagnostic,
            facts,
            result_ttl: None,
        })
    }

    pub(crate) fn finish_usability_probe_run_with_ttl(
        &self,
        finalization: UsabilityProbeRunFinalization<'_>,
    ) -> Result<bool> {
        let UsabilityProbeRunFinalization {
            run_id,
            generation,
            process_lease,
            complete: requested_complete,
            summary,
            diagnostic,
            facts,
            result_ttl,
        } = finalization;
        if process_lease.run_id != run_id || process_lease.database_path != self.database_path {
            anyhow::bail!("usability-probe run {run_id} has an invalid process lease");
        }
        // Always release the OS lock when finalization returns, including SQLite errors. A later
        // process can then recover the still-running audit row instead of being blocked forever.
        let _lock_release = UsabilityProbeLockRelease {
            locks: &self.active_usability_probe_locks,
            run_id,
        };
        let _cross_process_guard = lock_node_quality_reconciliation(&self.database_path)?;
        let generation_current = generation == self.quality_generation()
            && self.quality_session_current_while_locked()?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .context("failed to begin usability-probe completion transaction")?;
        let run_exists = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM usability_probe_runs WHERE id = ?1 AND generation = ?2 AND status = 'running')",
                params![run_id, generation as i64],
                |row| row.get::<_, bool>(0),
            )
            .context("failed to validate usability-probe run")?;
        if !run_exists {
            anyhow::bail!("usability-probe run {run_id} is missing, stale, or already finished");
        }

        let summary = summary.map(|value| truncate_chars(value, MAX_USABILITY_SUMMARY_CHARS));
        let diagnostic =
            diagnostic.map(|value| truncate_chars(value, MAX_USABILITY_DIAGNOSTIC_CHARS));
        let mut unique_nodes = BTreeSet::new();
        let mut all_nodes_bound = generation_current;
        let mut facts_valid = facts.len() <= MAX_USABILITY_FACTS;
        let mut validation_diagnostic = (facts.len() > MAX_USABILITY_FACTS)
            .then(|| format!("usability run contains more than {MAX_USABILITY_FACTS} node facts"));
        if generation_current && facts_valid {
            for fact in facts {
                if !unique_nodes.insert(fact.node.as_str()) {
                    facts_valid = false;
                    validation_diagnostic = Some(format!(
                        "usability run contains duplicate fact for {}",
                        truncate_chars(&fact.node, MAX_USABILITY_NODE_CHARS)
                    ));
                    break;
                }
                if fact.node.trim().is_empty()
                    || fact.node.chars().count() > MAX_USABILITY_NODE_CHARS
                    || fact.node.chars().any(char::is_control)
                    || fact
                        .detail
                        .as_ref()
                        .is_some_and(|detail| detail.chars().count() > MAX_USABILITY_DETAIL_CHARS)
                {
                    facts_valid = false;
                    validation_diagnostic =
                        Some("usability run contains an invalid node fact".to_string());
                    break;
                }
                let bound = transaction
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM node_identities WHERE tag = ?1)",
                        [fact.node.as_str()],
                        |row| row.get::<_, bool>(0),
                    )
                    .with_context(|| {
                        format!("failed to validate usability fact for {}", fact.node)
                    })?;
                all_nodes_bound &= bound;
            }
        }
        let complete = requested_complete && generation_current && all_nodes_bound && facts_valid;
        if generation_current && all_nodes_bound && facts_valid {
            for (sequence, fact) in facts.iter().enumerate() {
                transaction
                    .execute(
                        "INSERT INTO usability_probe_facts (run_id, sequence, node_tag, usable, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            run_id,
                            sequence as i64,
                            fact.node,
                            i64::from(fact.usable),
                            fact.detail,
                        ],
                    )
                    .with_context(|| {
                        format!("failed to persist usability fact for {}", fact.node)
                    })?;
                if complete {
                    transaction
                        .execute(
                            "INSERT INTO usability_probe_results (run_id, node_tag, usable, detail) VALUES (?1, ?2, ?3, ?4)",
                            params![run_id, fact.node, i64::from(fact.usable), fact.detail],
                        )
                        .with_context(|| {
                            format!("failed to publish usability result for {}", fact.node)
                        })?;
                }
            }
        }
        let final_diagnostic = if !generation_current {
            Some(
                "node configuration generation changed before usability results could publish"
                    .to_string(),
            )
        } else if !all_nodes_bound {
            Some("a usability result no longer belongs to the bound node configuration".to_string())
        } else if !facts_valid {
            validation_diagnostic
        } else {
            diagnostic
        };
        let completed_at_ms = current_timestamp_ms()?;
        let expires_at_ms = result_ttl.map(|ttl| {
            completed_at_ms.saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX))
        });
        let updated = transaction
            .execute(
                r#"
                UPDATE usability_probe_runs
                SET completed_at_ms = ?1, status = ?2, summary = ?3, diagnostic = ?4,
                    expires_at_ms = ?5
                WHERE id = ?6 AND generation = ?7 AND status = 'running'
                "#,
                params![
                    completed_at_ms,
                    if complete { "complete" } else { "incomplete" },
                    summary.as_deref(),
                    final_diagnostic,
                    complete.then_some(expires_at_ms).flatten(),
                    run_id,
                    generation as i64,
                ],
            )
            .context("failed to finalize usability-probe run")?;
        if updated != 1 {
            anyhow::bail!("usability-probe run {run_id} changed while it was being finalized");
        }
        // Facts, published results, and the terminal complete marker share this one commit. Panel
        // readers can therefore observe either the prior complete run or this entire run, never a
        // prefix whose missing node rows would be mistaken for application rejection.
        transaction
            .commit()
            .context("failed to commit usability-probe completion")?;
        Ok(complete)
    }

    pub(crate) fn latest_usability_probe_run(
        &self,
        criterion_id: &str,
        selector: &str,
        selector_members: &[String],
    ) -> Result<Option<StoredUsabilityProbeRun>> {
        let _cross_process_guard = lock_node_quality_reconciliation(&self.database_path)?;
        if !self.quality_session_current_while_locked()? {
            return Ok(None);
        }
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)
                .context("failed to begin usability projection read transaction")?;
        let run = Self::latest_usability_probe_run_from_connection(
            &transaction,
            criterion_id,
            selector,
            selector_members,
        )?;
        transaction
            .commit()
            .context("failed to finish usability projection read transaction")?;
        Ok(run)
    }

    pub(crate) fn latest_usability_probe_run_with_lease(
        &self,
        lease: &NodeQualityReadLease,
        criterion_id: &str,
        selector: &str,
        selector_members: &[String],
    ) -> Result<Option<StoredUsabilityProbeRun>> {
        self.validate_quality_read_lease(lease)?;
        // The filesystem lease freezes identity/generation through the eventual selector PUT;
        // this SQLite transaction independently freezes the run row and its result rows so a
        // concurrent complete publication cannot splice membership from two manifest runs.
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)
                .context("failed to begin leased usability projection read transaction")?;
        let run = Self::latest_usability_probe_run_from_connection(
            &transaction,
            criterion_id,
            selector,
            selector_members,
        )?;
        transaction
            .commit()
            .context("failed to finish leased usability projection read transaction")?;
        Ok(run)
    }

    fn latest_usability_probe_run_from_connection(
        connection: &Connection,
        criterion_id: &str,
        selector: &str,
        selector_members: &[String],
    ) -> Result<Option<StoredUsabilityProbeRun>> {
        let run = connection
            .query_row(
                r#"
                SELECT id, completed_at_ms, expires_at_ms, summary
                FROM usability_probe_runs
                WHERE criterion_id = ?1
                  AND selector = ?2
                  AND status = 'complete'
                ORDER BY completed_at_ms DESC, id DESC
                LIMIT 1
                "#,
                params![criterion_id, selector],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .context("failed to query latest complete usability-probe run")?;
        let latest_attempt = connection
            .query_row(
                r#"
                SELECT id, completed_at_ms, status, diagnostic
                FROM usability_probe_runs
                WHERE criterion_id = ?1 AND selector = ?2 AND completed_at_ms IS NOT NULL
                ORDER BY completed_at_ms DESC, id DESC LIMIT 1
                "#,
                params![criterion_id, selector],
                |row| {
                    Ok(StoredUsabilityProbeAttempt {
                        run_id: row.get(0)?,
                        completed_at_ms: row.get::<_, i64>(1)? as u64,
                        complete: row.get::<_, String>(2)? == "complete",
                        diagnostic: row.get(3)?,
                    })
                },
            )
            .optional()
            .context("failed to query latest usability-probe attempt")?;
        let Some((run_id, completed_at_ms, expires_at_ms, summary)) = run else {
            return Ok(None);
        };
        let allowed = selector_members.iter().collect::<BTreeSet<_>>();
        let mut statement = connection
            .prepare(
                "SELECT node_tag, usable, detail FROM usability_probe_results WHERE run_id = ?1 ORDER BY rowid",
            )
            .context("failed to prepare usability result projection")?;
        let rows = statement
            .query_map([run_id], |row| {
                Ok(UsabilityProbeFactRecord {
                    node: row.get(0)?,
                    usable: row.get::<_, i64>(1)? != 0,
                    detail: row.get(2)?,
                })
            })
            .context("failed to query usability result projection")?;
        let mut results = Vec::new();
        for row in rows {
            let result = row.context("failed to read usability result")?;
            // Generation fences only an in-flight publication. Once complete, FK cascade removes
            // changed identities one node at a time; this selector intersection then prevents the
            // surviving unchanged facts from leaking out of the current selector membership.
            if allowed.contains(&result.node) {
                results.push(result);
            }
        }
        Ok(Some(StoredUsabilityProbeRun {
            run_id,
            completed_at_ms: completed_at_ms as u64,
            expires_at_ms: expires_at_ms.map(|value| value as u64),
            summary,
            results,
            latest_attempt,
        }))
    }

    pub(crate) fn quality_generation(&self) -> u64 {
        self.quality_generation.get() as u64
    }

    fn read_quality_generation_unlocked(&self) -> Result<u64> {
        self.connection
            .query_row(
                "SELECT generation FROM node_quality_state WHERE singleton = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|generation| generation as u64)
            .context("failed to read current node quality generation")
    }

    fn maybe_prune_benchmark_history(&self, now_ms: i64) -> Result<()> {
        let prune_interval_ms = BENCHMARK_PRUNE_INTERVAL.as_millis() as i64;
        if now_ms.saturating_sub(self.last_prune_at_ms.get()) < prune_interval_ms {
            return Ok(());
        }
        // Advance before deleting so a busy database does not make every subsequent insert retry
        // the maintenance work. The bounded batch catches up over time without one huge WAL spike.
        self.last_prune_at_ms.set(now_ms);
        let cutoff_ms = now_ms.saturating_sub(BENCHMARK_RETENTION.as_millis() as i64);
        self.prune_benchmarks_before(cutoff_ms, BENCHMARK_PRUNE_BATCH_SIZE)?;
        Ok(())
    }

    fn prune_benchmarks_before(&self, cutoff_ms: i64, limit: usize) -> Result<usize> {
        self.connection
            .execute(
                r#"
                DELETE FROM benchmark_results
                WHERE id IN (
                    SELECT id
                    FROM benchmark_results
                    WHERE recorded_at_ms < ?1
                    ORDER BY recorded_at_ms ASC, id ASC
                    LIMIT ?2
                )
                "#,
                params![cutoff_ms, limit as i64],
            )
            .context("failed to delete expired benchmark history")
    }

    pub(crate) fn node_latency_history(
        &self,
        selector: &str,
        node: &str,
        limit: usize,
    ) -> Result<Vec<NodeLatencySample>> {
        let _cross_process_guard = lock_node_quality_reconciliation(&self.database_path)?;
        if !self.quality_session_current_while_locked()? {
            return Ok(Vec::new());
        }
        let mut statement = self
            .connection
            .prepare(
                r#"
                SELECT recorded_at_ms, delay_ms
                FROM (
                    SELECT id, recorded_at_ms, delay_ms
                    FROM benchmark_results
                    WHERE selector = ?1
                        AND node = ?2
                        AND completed = 1
                    ORDER BY recorded_at_ms DESC, id DESC
                    LIMIT ?3
                )
                ORDER BY recorded_at_ms ASC, id ASC
                "#,
            )
            .context("failed to prepare node latency history query")?;
        let rows = statement
            .query_map(params![selector, node, limit as i64], |row| {
                let recorded_at_ms: i64 = row.get(0)?;
                let delay_ms: Option<i64> = row.get(1)?;
                Ok(NodeLatencySample {
                    recorded_at_ms: recorded_at_ms as u64,
                    delay_ms: delay_ms.map(|value| value as u64),
                })
            })
            .context("failed to query node latency history")?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("failed to read node latency history row")?);
        }
        Ok(out)
    }

    #[cfg(test)]
    pub(crate) fn recent_benchmarks(&self, limit: usize) -> Result<Vec<StoredBenchmarkRecord>> {
        let mut statement = self
            .connection
            .prepare(
                r#"
                SELECT selector, node, filter, delay_ms, completed, job_kind
                FROM benchmark_results
                ORDER BY id DESC
                LIMIT ?1
                "#,
            )
            .context("failed to prepare benchmark history query")?;
        let rows = statement
            .query_map(params![limit as i64], |row| {
                let delay_ms: Option<i64> = row.get(3)?;
                let completed: i64 = row.get(4)?;
                Ok(StoredBenchmarkRecord {
                    selector: row.get(0)?,
                    node: row.get(1)?,
                    filter: row.get(2)?,
                    delay_ms: delay_ms.map(|value| value as u64),
                    completed: completed != 0,
                    job_kind: row.get(5)?,
                })
            })
            .context("failed to query benchmark history")?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("failed to read benchmark history row")?);
        }
        Ok(out)
    }

    #[cfg(test)]
    pub(crate) fn stored_node_identities(&self) -> Result<Vec<(String, String)>> {
        let mut statement = self
            .connection
            .prepare("SELECT tag, fingerprint FROM node_identities ORDER BY tag")
            .context("failed to prepare stored node identity query")?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .context("failed to query stored node identities")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read stored node identities")
    }
}

impl NodeHistoryReconciliationTransaction<'_> {
    pub(crate) fn apply(&self, committed_config: &Value) -> Result<NodeHistoryReconciliation> {
        let transaction = self
            .transaction
            .as_ref()
            .context("node history reconciliation transaction is no longer active")?;
        let refreshed = node_identities_from_config(committed_config)?;
        let persisted = {
            let mut statement = transaction
                .prepare("SELECT tag, fingerprint FROM node_identities ORDER BY tag")
                .context("failed to prepare node identity query")?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .context("failed to query node identities")?;
            let mut identities = BTreeMap::new();
            for row in rows {
                let (tag, fingerprint) = row.context("failed to read node identity")?;
                identities.insert(tag, fingerprint);
            }
            identities
        };
        let refreshed_fingerprints = refreshed
            .iter()
            .map(|(tag, identity)| (tag.clone(), identity.fingerprint.clone()))
            .collect::<BTreeMap<_, _>>();
        let (previous_generation, identities_initialized): (i64, bool) = transaction
            .query_row(
                "SELECT generation, identities_initialized FROM node_quality_state WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)),
            )
            .context("failed to read node quality generation")?;
        let identities_changed = !identities_initialized || persisted != refreshed_fingerprints;
        let generation = if identities_changed {
            previous_generation
                .checked_add(1)
                .context("node quality generation overflow")?
        } else {
            previous_generation
        };
        let retained = refreshed
            .iter()
            .filter(|(tag, identity)| {
                persisted
                    .get(*tag)
                    .is_some_and(|fingerprint| fingerprint == &identity.fingerprint)
            })
            .map(|(tag, _)| tag.clone())
            .collect::<BTreeSet<_>>();

        delete_unretained_node_facts(transaction, &retained)?;
        transaction
            .execute(
                "DELETE FROM node_identities WHERE tag NOT IN (SELECT tag FROM retained_node_tags)",
                [],
            )
            .context("failed to delete removed node identities")?;
        for identity in refreshed.values() {
            transaction
                .execute(
                    r#"
                    INSERT INTO node_identities (tag, fingerprint) VALUES (?1, ?2)
                    ON CONFLICT(tag) DO UPDATE SET fingerprint = excluded.fingerprint
                    "#,
                    params![identity.tag, identity.fingerprint],
                )
                .with_context(|| format!("failed to persist identity for node {}", identity.tag))?;
        }
        transaction
            .execute(
                "UPDATE node_quality_state SET generation = ?1, identities_initialized = 1 WHERE singleton = 1",
                [generation],
            )
            .context("failed to advance node quality generation")?;
        Ok(NodeHistoryReconciliation {
            generation: generation as u64,
            identities_changed,
        })
    }

    pub(crate) fn commit(mut self, reconciliation: NodeHistoryReconciliation) -> Result<()> {
        let transaction = self
            .transaction
            .take()
            .context("node history reconciliation transaction is no longer active")?;
        if let Err(commit_error) = transaction.execute_batch("COMMIT") {
            let rollback = transaction.execute_batch("ROLLBACK");
            drop(transaction);
            return match rollback {
                Ok(()) => Err(commit_error)
                    .context("failed to commit node history reconciliation; rollback succeeded"),
                Err(rollback_error) => Err(anyhow!(
                    "failed to commit node history reconciliation: {commit_error}; transaction rollback also failed: {rollback_error}; quality writes remain blocked"
                )),
            };
        }
        drop(transaction);
        self.store
            .quality_generation
            .set(reconciliation.generation as i64);
        Ok(())
    }

    pub(crate) fn rollback(mut self) -> Result<()> {
        let transaction = self
            .transaction
            .take()
            .context("node history reconciliation transaction is no longer active")?;
        let rollback = transaction
            .execute_batch("ROLLBACK")
            .context("failed to roll back node history reconciliation");
        drop(transaction);
        rollback
    }
}

fn delete_unretained_node_facts(
    transaction: &rusqlite::Transaction<'_>,
    retained: &BTreeSet<String>,
) -> Result<()> {
    transaction
        .execute_batch(
            r#"
            CREATE TEMP TABLE IF NOT EXISTS retained_node_tags (
                tag TEXT PRIMARY KEY
            );
            DELETE FROM retained_node_tags;
            "#,
        )
        .context("failed to prepare retained node identities")?;
    for tag in retained {
        transaction
            .execute("INSERT INTO retained_node_tags (tag) VALUES (?1)", [tag])
            .with_context(|| format!("failed to retain history for node {tag}"))?;
    }
    transaction
        .execute(
            r#"
            DELETE FROM probe_attempts
            WHERE assessment_id IN (
                SELECT id FROM reachability_assessments
                WHERE node NOT IN (SELECT tag FROM retained_node_tags)
            )
            "#,
            [],
        )
        .context("failed to delete unretained probe attempts")?;
    transaction
        .execute(
            "DELETE FROM reachability_assessments WHERE node NOT IN (SELECT tag FROM retained_node_tags)",
            [],
        )
        .context("failed to delete unretained reachability assessments")?;
    transaction
        .execute(
            "DELETE FROM benchmark_results WHERE node NOT IN (SELECT tag FROM retained_node_tags)",
            [],
        )
        .context("failed to delete unretained benchmark facts")?;
    transaction
        .execute(
            "DELETE FROM sustained_probe_results WHERE node_tag NOT IN (SELECT tag FROM retained_node_tags)",
            [],
        )
        .context("failed to delete unretained sustained-quality facts")?;
    Ok(())
}

fn node_identities_from_config(config: &Value) -> Result<BTreeMap<String, NodeIdentity>> {
    let outbounds = config
        .get("outbounds")
        .and_then(Value::as_array)
        .context("committed sing-box config is missing an outbounds array")?;
    let mut identities = BTreeMap::new();
    for outbound in outbounds {
        let Some(tag) = outbound.get("tag").and_then(Value::as_str) else {
            continue;
        };
        let tag = tag.to_string();
        let identity = NodeIdentity {
            tag: tag.clone(),
            fingerprint: node_configuration_fingerprint(outbound)?,
        };
        if identities.insert(tag.clone(), identity).is_some() {
            anyhow::bail!("committed sing-box config contains duplicate node tag {tag}");
        }
    }
    Ok(identities)
}

fn node_configuration_fingerprint(outbound: &Value) -> Result<String> {
    let canonical = canonical_json_value(outbound);
    let encoded = serde_json::to_vec(&canonical)
        .context("failed to encode canonical node configuration for fingerprinting")?;
    let digest = Sha256::digest(encoded);
    Ok(format!("{digest:x}"))
}

fn canonical_json_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut canonical = Map::new();
            for key in keys {
                canonical.insert(key.clone(), canonical_json_value(&object[key]));
            }
            Value::Object(canonical)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_json_value).collect()),
        _ => value.clone(),
    }
}

fn sustained_outcome_from_storage(
    kind: &str,
    first_byte_ms: Option<i64>,
    completion_ms: Option<i64>,
    bytes_read: Option<i64>,
    detail: Option<String>,
) -> rusqlite::Result<SustainedProbeOutcome> {
    let invalid = |message: &'static str| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            std::io::Error::new(std::io::ErrorKind::InvalidData, message).into(),
        )
    };
    match kind {
        "completed" => {
            let first_byte_ms = first_byte_ms
                .filter(|value| *value >= 0)
                .map(|value| value as u64)
                .ok_or_else(|| invalid("completed sustained result is missing first-byte time"))?;
            let completion_ms = completion_ms
                .filter(|value| *value >= 0)
                .map(|value| value as u64)
                .ok_or_else(|| invalid("completed sustained result is missing completion time"))?;
            let bytes_read = bytes_read
                .filter(|value| *value >= 0)
                .map(|value| value as u64)
                .ok_or_else(|| invalid("completed sustained result is missing bytes"))?;
            SustainedCompletion::from_facts(first_byte_ms, completion_ms, bytes_read)
                .map(SustainedProbeOutcome::Completed)
                .map_err(|_| invalid("completed sustained result contains invalid facts"))
        }
        "transfer_failed" => Ok(SustainedProbeOutcome::TransferFailed {
            detail: detail.unwrap_or_default(),
        }),
        _ => Err(invalid("latest sustained result has an unexpected outcome")),
    }
}

fn median(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[(values.len() - 1) / 2])
}

fn percentile_95(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let rank = (values.len() * 95).div_ceil(100).saturating_sub(1);
    values.get(rank).copied()
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn current_timestamp_ms() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time is before UNIX epoch")?
        .as_millis() as i64)
}

fn normalize_database_path(path: &Path) -> Result<PathBuf> {
    if path == Path::new(":memory:") {
        Ok(path.to_path_buf())
    } else {
        node_quality_reserved_paths(path).map(|paths| paths[0].clone())
    }
}

fn open_existing_quality_marker(path: &Path, label: &str) -> Result<File> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect existing {label} {}", path.display()))?;
    if !metadata.file_type().is_file() {
        anyhow::bail!("existing {label} is not a regular file: {}", path.display());
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to reopen existing {label} {}", path.display()))?;
    if !file
        .metadata()
        .with_context(|| format!("failed to validate existing {label} {}", path.display()))?
        .is_file()
    {
        anyhow::bail!("existing {label} is not a regular file: {}", path.display());
    }
    Ok(file)
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

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn prepare_node_quality_database(path: &Path) -> Result<DatabasePreparation> {
    if path == Path::new(":memory:") {
        return Ok(DatabasePreparation::Initialize);
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            ensure_no_orphaned_legacy_sidecars(path)?;
            return Ok(DatabasePreparation::Initialize);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect node-quality database {}", path.display())
            });
        }
    };
    if !metadata.file_type().is_file() {
        anyhow::bail!(
            "refusing to open non-regular node-quality database {}",
            path.display()
        );
    }
    if metadata.len() == 0 {
        ensure_no_orphaned_legacy_sidecars(path)?;
        return Ok(DatabasePreparation::Initialize);
    }
    // Validate every known sidecar before SQLite opens the main file. A directory, symlink, or
    // other unexpected sidecar must not let a legacy reset delete or mutate the main database.
    let reset_sidecars = inspect_legacy_reset_sidecars(path)?;

    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| {
            format!(
                "refusing to replace unrecognized node-quality database {}",
                path.display()
            )
        })?;
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .with_context(|| {
            format!(
                "failed to inspect node-quality schema version in {}",
                path.display()
            )
        })?;
    match version {
        NODE_QUALITY_SCHEMA_VERSION => {
            if current_schema_is_recognized(&connection)? {
                return Ok(DatabasePreparation::Current);
            }
            anyhow::bail!(
                "refusing to modify unrecognized version {version} database {}",
                path.display()
            );
        }
        4 => {
            // v4 is the first production append-only node-quality model, so its measurements are
            // user data rather than disposable legacy cache. Only its exact published shape earns
            // the additive migration; a spoofed v4 remains untouched and fails closed.
            if v4_schema_is_recognized(&connection)? {
                return Ok(DatabasePreparation::MigrateV4);
            }
            anyhow::bail!(
                "refusing to modify unrecognized version {version} database {}",
                path.display()
            );
        }
        5 => {
            if v5_schema_is_recognized(&connection)? {
                return Ok(DatabasePreparation::MigrateV5);
            }
            anyhow::bail!(
                "refusing to modify unrecognized version {version} database {}",
                path.display()
            );
        }
        legacy_version if legacy_version < 4 => {}
        _ => {
            anyhow::bail!(
                "refusing to replace unrecognized node-quality schema version {version} in {}",
                path.display()
            );
        }
    }
    drop(connection);

    for sidecar in reset_sidecars {
        std::fs::remove_file(&sidecar).with_context(|| {
            format!(
                "failed to delete legacy SQLite sidecar {}",
                sidecar.display()
            )
        })?;
    }
    std::fs::remove_file(path)
        .with_context(|| format!("failed to delete legacy SQLite database {}", path.display()))?;
    sync_parent_directory(path).with_context(|| {
        format!(
            "failed to make legacy SQLite database removal durable for {}",
            path.display()
        )
    })?;
    Ok(DatabasePreparation::Initialize)
}

fn legacy_reset_sidecar_paths(path: &Path) -> Vec<PathBuf> {
    [
        "-wal",
        "-shm",
        "-journal",
        QUALITY_WRITE_BLOCK_SUFFIX,
        QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX,
    ]
    .into_iter()
    .map(|suffix| {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        PathBuf::from(sidecar)
    })
    .collect()
}

fn inspect_legacy_reset_sidecars(path: &Path) -> Result<Vec<PathBuf>> {
    let mut existing = Vec::new();
    for sidecar in legacy_reset_sidecar_paths(path) {
        let metadata = match fs::symlink_metadata(&sidecar) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect legacy SQLite sidecar {}",
                        sidecar.display()
                    )
                });
            }
        };
        if !metadata.file_type().is_file() {
            anyhow::bail!(
                "refusing to delete non-regular legacy SQLite sidecar {}",
                sidecar.display()
            );
        }
        existing.push(sidecar);
    }
    Ok(existing)
}

fn ensure_no_orphaned_legacy_sidecars(path: &Path) -> Result<()> {
    let sidecars = inspect_legacy_reset_sidecars(path)?;
    if let Some(sidecar) = sidecars.first() {
        anyhow::bail!(
            "refusing to initialize node-quality database {} with orphaned SQLite sidecar {}",
            path.display(),
            sidecar.display()
        );
    }
    Ok(())
}

fn current_schema_is_recognized(connection: &Connection) -> Result<bool> {
    if user_table_names(connection)?
        != [
            "benchmark_results",
            "node_identities",
            "node_quality_state",
            "probe_attempts",
            "reachability_assessments",
            "sustained_probe_results",
            "usability_probe_facts",
            "usability_probe_results",
            "usability_probe_runs",
        ]
        || !published_core_table_schemas_are_recognized(connection, true, true)?
        || !table_has_exact_columns(
            connection,
            "node_identities",
            &[("tag", "TEXT", false, 1), ("fingerprint", "TEXT", true, 0)],
        )?
        || !table_has_exact_columns(
            connection,
            "node_quality_state",
            &[
                ("singleton", "INTEGER", false, 1),
                ("generation", "INTEGER", true, 0),
                ("identities_initialized", "INTEGER", true, 0),
            ],
        )?
        || !table_has_exact_columns(
            connection,
            "sustained_probe_results",
            &[
                ("id", "INTEGER", false, 1),
                ("recorded_at_ms", "INTEGER", true, 0),
                ("selector", "TEXT", true, 0),
                ("node_tag", "TEXT", true, 0),
                ("target_identity", "TEXT", true, 0),
                ("outcome_kind", "TEXT", true, 0),
                ("first_byte_ms", "INTEGER", false, 0),
                ("completion_ms", "INTEGER", false, 0),
                ("bytes_read", "INTEGER", false, 0),
                ("detail", "TEXT", false, 0),
            ],
        )?
        || !table_has_exact_columns(
            connection,
            "usability_probe_runs",
            &[
                ("id", "INTEGER", false, 1),
                ("started_at_ms", "INTEGER", true, 0),
                ("completed_at_ms", "INTEGER", false, 0),
                ("criterion_id", "TEXT", true, 0),
                ("selector", "TEXT", true, 0),
                ("generation", "INTEGER", true, 0),
                ("status", "TEXT", true, 0),
                ("summary", "TEXT", false, 0),
                ("diagnostic", "TEXT", false, 0),
                ("expires_at_ms", "INTEGER", false, 0),
            ],
        )?
        || !table_has_exact_columns(
            connection,
            "usability_probe_facts",
            &[
                ("run_id", "INTEGER", true, 1),
                ("sequence", "INTEGER", true, 2),
                ("node_tag", "TEXT", true, 0),
                ("usable", "INTEGER", true, 0),
                ("detail", "TEXT", false, 0),
            ],
        )?
        || !table_has_exact_columns(
            connection,
            "usability_probe_results",
            &[
                ("run_id", "INTEGER", true, 1),
                ("node_tag", "TEXT", true, 2),
                ("usable", "INTEGER", true, 0),
                ("detail", "TEXT", false, 0),
            ],
        )?
        || !user_behavior_objects(connection)?.is_empty()
        || !object_sql_matches(
            connection,
            "table",
            "node_identities",
            &["CREATE TABLE node_identities (tag TEXT PRIMARY KEY, fingerprint TEXT NOT NULL)"],
        )?
        || !object_sql_matches(
            connection,
            "table",
            "node_quality_state",
            &[
                "CREATE TABLE node_quality_state (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), generation INTEGER NOT NULL, identities_initialized INTEGER NOT NULL)",
            ],
        )?
        || !object_sql_matches(
            connection,
            "table",
            "sustained_probe_results",
            &[SUSTAINED_PROBE_RESULTS_TABLE_SQL],
        )?
        || !object_sql_matches(
            connection,
            "table",
            "usability_probe_runs",
            &[USABILITY_PROBE_RUNS_TABLE_SQL],
        )?
        || !object_sql_matches(
            connection,
            "table",
            "usability_probe_facts",
            &[USABILITY_PROBE_FACTS_TABLE_SQL],
        )?
        || !object_sql_matches(
            connection,
            "table",
            "usability_probe_results",
            &[USABILITY_PROBE_RESULTS_TABLE_SQL],
        )?
    {
        return Ok(false);
    }
    let singleton_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM node_quality_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .context("failed to validate node-quality state singleton")?;
    Ok(singleton_rows == 1)
}

fn v4_schema_is_recognized(connection: &Connection) -> Result<bool> {
    if user_table_names(connection)?
        != [
            "benchmark_results",
            "node_identities",
            "node_quality_state",
            "probe_attempts",
            "reachability_assessments",
            "sustained_probe_results",
        ]
        || !published_core_table_schemas_are_recognized(connection, true, false)?
        || !table_has_exact_columns(
            connection,
            "node_identities",
            &[("tag", "TEXT", false, 1), ("fingerprint", "TEXT", true, 0)],
        )?
        || !table_has_exact_columns(
            connection,
            "node_quality_state",
            &[
                ("singleton", "INTEGER", false, 1),
                ("generation", "INTEGER", true, 0),
                ("identities_initialized", "INTEGER", true, 0),
            ],
        )?
        || !table_has_exact_columns(
            connection,
            "sustained_probe_results",
            &[
                ("id", "INTEGER", false, 1),
                ("recorded_at_ms", "INTEGER", true, 0),
                ("selector", "TEXT", true, 0),
                ("node_tag", "TEXT", true, 0),
                ("target_identity", "TEXT", true, 0),
                ("outcome_kind", "TEXT", true, 0),
                ("first_byte_ms", "INTEGER", false, 0),
                ("completion_ms", "INTEGER", false, 0),
                ("bytes_read", "INTEGER", false, 0),
                ("detail", "TEXT", false, 0),
            ],
        )?
        || !object_sql_matches(
            connection,
            "table",
            "node_identities",
            &["CREATE TABLE node_identities (tag TEXT PRIMARY KEY, fingerprint TEXT NOT NULL)"],
        )?
        || !object_sql_matches(
            connection,
            "table",
            "node_quality_state",
            &[
                "CREATE TABLE node_quality_state (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), generation INTEGER NOT NULL, identities_initialized INTEGER NOT NULL)",
            ],
        )?
        || !object_sql_matches(
            connection,
            "table",
            "sustained_probe_results",
            &[SUSTAINED_PROBE_RESULTS_TABLE_SQL],
        )?
    {
        return Ok(false);
    }
    let singleton_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM node_quality_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .context("failed to validate v4 node-quality state singleton")?;
    Ok(singleton_rows == 1)
}

fn v5_schema_is_recognized(connection: &Connection) -> Result<bool> {
    if user_table_names(connection)?
        != [
            "benchmark_results",
            "node_identities",
            "node_quality_state",
            "probe_attempts",
            "reachability_assessments",
            "sustained_probe_results",
            "usability_probe_facts",
            "usability_probe_results",
            "usability_probe_runs",
        ]
        || !published_core_table_schemas_are_recognized(connection, true, true)?
        || !table_has_exact_columns(
            connection,
            "usability_probe_runs",
            &[
                ("id", "INTEGER", false, 1),
                ("started_at_ms", "INTEGER", true, 0),
                ("completed_at_ms", "INTEGER", false, 0),
                ("criterion_id", "TEXT", true, 0),
                ("selector", "TEXT", true, 0),
                ("generation", "INTEGER", true, 0),
                ("status", "TEXT", true, 0),
                ("summary", "TEXT", false, 0),
                ("diagnostic", "TEXT", false, 0),
            ],
        )?
        || !object_sql_matches(
            connection,
            "table",
            "usability_probe_runs",
            &[USABILITY_PROBE_RUNS_V5_TABLE_SQL],
        )?
        || !user_behavior_objects(connection)?.is_empty()
    {
        return Ok(false);
    }
    let singleton_rows: i64 = connection.query_row(
        "SELECT COUNT(*) FROM node_quality_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    Ok(singleton_rows == 1 && usability_probe_foreign_keys_are_recognized(connection)?)
}

fn published_core_table_schemas_are_recognized(
    connection: &Connection,
    require_complete: bool,
    include_usability: bool,
) -> Result<bool> {
    let benchmark = table_has_exact_columns(
        connection,
        "benchmark_results",
        &[
            ("id", "INTEGER", false, 1),
            ("recorded_at_ms", "INTEGER", true, 0),
            ("selector", "TEXT", true, 0),
            ("node", "TEXT", true, 0),
            ("filter", "TEXT", true, 0),
            ("delay_ms", "INTEGER", false, 0),
            ("completed", "INTEGER", true, 0),
            ("job_kind", "TEXT", true, 0),
        ],
    )?;
    let reachability_columns = table_columns(connection, "reachability_assessments")?;
    let reachability_v2a = expected_columns(&[
        ("id", "INTEGER", false, 1),
        ("recorded_at_ms", "INTEGER", true, 0),
        ("selector", "TEXT", true, 0),
        ("node", "TEXT", true, 0),
    ]);
    let reachability_v2b = expected_columns(&[
        ("id", "INTEGER", false, 1),
        ("recorded_at_ms", "INTEGER", true, 0),
        ("selector", "TEXT", true, 0),
        ("node", "TEXT", true, 0),
        ("complete", "INTEGER", true, 0),
    ]);
    let reachability = if require_complete {
        reachability_columns == reachability_v2b
    } else {
        reachability_columns == reachability_v2a || reachability_columns == reachability_v2b
    };
    let probes = table_has_exact_columns(
        connection,
        "probe_attempts",
        &[
            ("assessment_id", "INTEGER", true, 1),
            ("attempt_index", "INTEGER", true, 2),
            ("outcome_kind", "TEXT", true, 0),
            ("delay_ms", "INTEGER", false, 0),
            ("detail", "TEXT", false, 0),
            ("controller_status", "INTEGER", false, 0),
        ],
    )?;
    let indexes = user_index_names(connection)?;
    let expected_indexes = if include_usability {
        vec![
            "idx_benchmark_results_recorded_at",
            "idx_benchmark_results_selector_node_recent",
            "idx_reachability_assessments_selector_node_recent",
            "idx_sustained_probe_selector_node_target_recent",
            "idx_usability_probe_runs_criterion_selector_recent",
        ]
    } else {
        vec![
            "idx_benchmark_results_recorded_at",
            "idx_benchmark_results_selector_node_recent",
            "idx_reachability_assessments_selector_node_recent",
            "idx_sustained_probe_selector_node_target_recent",
        ]
    };
    let indexes_recognized = indexes == expected_indexes
        && index_has_exact_key(
            connection,
            "idx_benchmark_results_recorded_at",
            &[("recorded_at_ms", false)],
        )?
        && index_has_exact_key(
            connection,
            "idx_benchmark_results_selector_node_recent",
            &[
                ("selector", false),
                ("node", false),
                ("recorded_at_ms", true),
                ("id", true),
            ],
        )?
        && index_has_exact_key(
            connection,
            "idx_reachability_assessments_selector_node_recent",
            &[
                ("selector", false),
                ("node", false),
                ("recorded_at_ms", true),
                ("id", true),
            ],
        )?
        && index_has_exact_key(
            connection,
            "idx_sustained_probe_selector_node_target_recent",
            &[
                ("selector", false),
                ("node_tag", false),
                ("target_identity", false),
                ("recorded_at_ms", true),
                ("id", true),
            ],
        )?
        && (!include_usability
            || index_has_exact_key(
                connection,
                "idx_usability_probe_runs_criterion_selector_recent",
                &[
                    ("criterion_id", false),
                    ("selector", false),
                    ("status", false),
                    ("generation", false),
                    ("completed_at_ms", true),
                    ("id", true),
                ],
            )?)
        && published_core_sql_is_recognized(connection, require_complete, include_usability)?;
    Ok(benchmark
        && reachability
        && probes
        && indexes_recognized
        && user_behavior_objects(connection)?.is_empty()
        && probe_foreign_key_is_recognized(connection)?
        && (!include_usability || usability_probe_foreign_keys_are_recognized(connection)?))
}

const BENCHMARK_RESULTS_TABLE_SQL: &str = r#"
    CREATE TABLE benchmark_results (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        recorded_at_ms INTEGER NOT NULL,
        selector TEXT NOT NULL,
        node TEXT NOT NULL,
        filter TEXT NOT NULL,
        delay_ms INTEGER,
        completed INTEGER NOT NULL,
        job_kind TEXT NOT NULL
    )
"#;

const REACHABILITY_V2A_TABLE_SQL: &str = r#"
    CREATE TABLE reachability_assessments (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        recorded_at_ms INTEGER NOT NULL,
        selector TEXT NOT NULL,
        node TEXT NOT NULL
    )
"#;

const REACHABILITY_V2B_TABLE_SQL: &str = r#"
    CREATE TABLE reachability_assessments (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        recorded_at_ms INTEGER NOT NULL,
        selector TEXT NOT NULL,
        node TEXT NOT NULL,
        complete INTEGER NOT NULL
    )
"#;

const PROBE_ATTEMPTS_TABLE_SQL: &str = r#"
    CREATE TABLE probe_attempts (
        assessment_id INTEGER NOT NULL REFERENCES reachability_assessments(id) ON DELETE CASCADE,
        attempt_index INTEGER NOT NULL,
        outcome_kind TEXT NOT NULL,
        delay_ms INTEGER,
        detail TEXT,
        controller_status INTEGER,
        PRIMARY KEY (assessment_id, attempt_index)
    )
"#;

const SUSTAINED_PROBE_RESULTS_TABLE_SQL: &str = r#"
    CREATE TABLE sustained_probe_results (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        recorded_at_ms INTEGER NOT NULL,
        selector TEXT NOT NULL,
        node_tag TEXT NOT NULL,
        target_identity TEXT NOT NULL,
        outcome_kind TEXT NOT NULL,
        first_byte_ms INTEGER,
        completion_ms INTEGER,
        bytes_read INTEGER,
        detail TEXT
    )
"#;

const USABILITY_PROBE_RUNS_TABLE_SQL: &str = r#"
    CREATE TABLE usability_probe_runs (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        started_at_ms INTEGER NOT NULL,
        completed_at_ms INTEGER,
        criterion_id TEXT NOT NULL,
        selector TEXT NOT NULL,
        generation INTEGER NOT NULL,
        status TEXT NOT NULL,
        summary TEXT,
        diagnostic TEXT,
        expires_at_ms INTEGER
    )
"#;

const USABILITY_PROBE_RUNS_V5_TABLE_SQL: &str = r#"
    CREATE TABLE usability_probe_runs (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        started_at_ms INTEGER NOT NULL,
        completed_at_ms INTEGER,
        criterion_id TEXT NOT NULL,
        selector TEXT NOT NULL,
        generation INTEGER NOT NULL,
        status TEXT NOT NULL,
        summary TEXT,
        diagnostic TEXT
    )
"#;

const USABILITY_PROBE_FACTS_TABLE_SQL: &str = r#"
    CREATE TABLE usability_probe_facts (
        run_id INTEGER NOT NULL REFERENCES usability_probe_runs(id) ON DELETE CASCADE,
        sequence INTEGER NOT NULL,
        node_tag TEXT NOT NULL REFERENCES node_identities(tag) ON DELETE CASCADE,
        usable INTEGER NOT NULL,
        detail TEXT,
        PRIMARY KEY (run_id, sequence)
    )
"#;

const USABILITY_PROBE_RESULTS_TABLE_SQL: &str = r#"
    CREATE TABLE usability_probe_results (
        run_id INTEGER NOT NULL REFERENCES usability_probe_runs(id) ON DELETE CASCADE,
        node_tag TEXT NOT NULL REFERENCES node_identities(tag) ON DELETE CASCADE,
        usable INTEGER NOT NULL,
        detail TEXT,
        PRIMARY KEY (run_id, node_tag)
    )
"#;

fn published_core_sql_is_recognized(
    connection: &Connection,
    require_complete: bool,
    include_usability: bool,
) -> Result<bool> {
    let reachability_sql = if require_complete {
        vec![REACHABILITY_V2B_TABLE_SQL]
    } else {
        vec![REACHABILITY_V2A_TABLE_SQL, REACHABILITY_V2B_TABLE_SQL]
    };
    let core_recognized = object_sql_matches(
        connection,
        "table",
        "benchmark_results",
        &[BENCHMARK_RESULTS_TABLE_SQL],
    )? && object_sql_matches(
        connection,
        "table",
        "reachability_assessments",
        &reachability_sql,
    )? && object_sql_matches(
        connection,
        "table",
        "probe_attempts",
        &[PROBE_ATTEMPTS_TABLE_SQL],
    )? && object_sql_matches(
        connection,
        "index",
        "idx_benchmark_results_recorded_at",
        &["CREATE INDEX idx_benchmark_results_recorded_at ON benchmark_results(recorded_at_ms)"],
    )? && object_sql_matches(
        connection,
        "index",
        "idx_benchmark_results_selector_node_recent",
        &[
            "CREATE INDEX idx_benchmark_results_selector_node_recent ON benchmark_results(selector, node, recorded_at_ms DESC, id DESC)",
        ],
    )? && object_sql_matches(
        connection,
        "index",
        "idx_reachability_assessments_selector_node_recent",
        &[
            "CREATE INDEX idx_reachability_assessments_selector_node_recent ON reachability_assessments(selector, node, recorded_at_ms DESC, id DESC)",
        ],
    )? && object_sql_matches(
        connection,
        "index",
        "idx_sustained_probe_selector_node_target_recent",
        &[
            "CREATE INDEX idx_sustained_probe_selector_node_target_recent ON sustained_probe_results(selector, node_tag, target_identity, recorded_at_ms DESC, id DESC)",
        ],
    )?;
    Ok(core_recognized
        && (!include_usability
            || object_sql_matches(
                connection,
                "index",
                "idx_usability_probe_runs_criterion_selector_recent",
                &[
                    "CREATE INDEX idx_usability_probe_runs_criterion_selector_recent ON usability_probe_runs(criterion_id, selector, status, generation, completed_at_ms DESC, id DESC)",
                ],
            )?))
}

fn user_table_names(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .context("failed to inspect node-quality tables")?;
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .context("failed to query node-quality tables")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read node-quality tables")
}

fn table_has_exact_columns(
    connection: &Connection,
    table: &str,
    expected: &[(&str, &str, bool, i64)],
) -> Result<bool> {
    Ok(table_columns(connection, table)? == expected_columns(expected))
}

fn expected_columns(expected: &[(&str, &str, bool, i64)]) -> Vec<(String, String, bool, i64)> {
    expected
        .iter()
        .map(|(name, data_type, not_null, primary_key)| {
            (
                (*name).to_string(),
                (*data_type).to_string(),
                *not_null,
                *primary_key,
            )
        })
        .collect()
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<(String, String, bool, i64)>> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .with_context(|| format!("failed to inspect {table} columns"))?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?.to_ascii_uppercase(),
                row.get::<_, i64>(3)? != 0,
                row.get::<_, i64>(5)?,
            ))
        })
        .with_context(|| format!("failed to query {table} columns"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {table} columns"))
}

fn user_index_names(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'index' AND sql IS NOT NULL ORDER BY name",
        )
        .context("failed to inspect node-quality indexes")?;
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .context("failed to query node-quality indexes")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read node-quality indexes")
}

fn user_behavior_objects(connection: &Connection) -> Result<Vec<(String, String)>> {
    let mut statement = connection
        .prepare(
            "SELECT type, name FROM sqlite_master \
             WHERE type IN ('trigger', 'view') ORDER BY type, name",
        )
        .context("failed to inspect node-quality views and triggers")?;
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .context("failed to query node-quality views and triggers")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read node-quality views and triggers")
}

fn object_sql_matches(
    connection: &Connection,
    object_type: &str,
    name: &str,
    expected: &[&str],
) -> Result<bool> {
    let sql = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
            params![object_type, name],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .with_context(|| format!("failed to inspect {object_type} SQL for {name}"))?
        .flatten();
    let Some(sql) = sql else {
        return Ok(false);
    };
    let sql = normalize_schema_sql(&sql);
    Ok(expected
        .iter()
        .any(|expected| normalize_schema_sql(expected) == sql))
}

fn normalize_schema_sql(sql: &str) -> String {
    sql.chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn index_has_exact_key(
    connection: &Connection,
    index: &str,
    expected: &[(&str, bool)],
) -> Result<bool> {
    let mut statement = connection
        .prepare(&format!("PRAGMA index_xinfo({index})"))
        .with_context(|| format!("failed to inspect {index}"))?;
    let keys = statement
        .query_map([], |row| {
            let key: i64 = row.get(5)?;
            let name: Option<String> = row.get(2)?;
            let descending = row.get::<_, i64>(3)? != 0;
            Ok((key != 0).then_some((name, descending)))
        })
        .with_context(|| format!("failed to query {index}"))?
        .filter_map(|row| match row {
            Ok(Some((Some(name), descending))) => Some(Ok((name, descending))),
            Ok(Some((None, _))) => Some(Err(rusqlite::Error::InvalidQuery)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {index}"))?;
    Ok(keys
        == expected
            .iter()
            .map(|(name, descending)| ((*name).to_string(), *descending))
            .collect::<Vec<_>>())
}

fn probe_foreign_key_is_recognized(connection: &Connection) -> Result<bool> {
    let mut statement = connection
        .prepare("PRAGMA foreign_key_list(probe_attempts)")
        .context("failed to inspect probe_attempts foreign keys")?;
    let foreign_keys = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(6)?,
            ))
        })
        .context("failed to query probe_attempts foreign keys")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read probe_attempts foreign keys")?;
    Ok(foreign_keys
        == [(
            "reachability_assessments".to_string(),
            "assessment_id".to_string(),
            "id".to_string(),
            "CASCADE".to_string(),
        )])
}

fn usability_probe_foreign_keys_are_recognized(connection: &Connection) -> Result<bool> {
    fn foreign_keys(
        connection: &Connection,
        table: &str,
    ) -> Result<Vec<(String, String, String, String)>> {
        let mut statement = connection
            .prepare(&format!("PRAGMA foreign_key_list({table})"))
            .with_context(|| format!("failed to inspect {table} foreign keys"))?;
        let mut keys = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .with_context(|| format!("failed to query {table} foreign keys"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .with_context(|| format!("failed to read {table} foreign keys"))?;
        keys.sort();
        Ok(keys)
    }

    let expected = [
        (
            "node_identities".to_string(),
            "node_tag".to_string(),
            "tag".to_string(),
            "CASCADE".to_string(),
        ),
        (
            "usability_probe_runs".to_string(),
            "run_id".to_string(),
            "id".to_string(),
            "CASCADE".to_string(),
        ),
    ];
    Ok(
        foreign_keys(connection, "usability_probe_facts")? == expected
            && foreign_keys(connection, "usability_probe_results")? == expected,
    )
}

fn configure_benchmark_connection(connection: &Connection, path: &Path) -> Result<()> {
    connection
        .busy_timeout(SQLITE_BUSY_TIMEOUT)
        .with_context(|| format!("failed to set SQLite busy timeout for {}", path.display()))?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .with_context(|| {
            format!(
                "failed to enable SQLite foreign keys for {}",
                path.display()
            )
        })?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .with_context(|| format!("failed to enable SQLite WAL mode for {}", path.display()))?;
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .with_context(|| {
            format!(
                "failed to set SQLite synchronous=NORMAL for {}",
                path.display()
            )
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BenchmarkRecord, BenchmarkStore, NODE_QUALITY_SCHEMA_VERSION, NodeQualityReadLease,
        QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX, QUALITY_WRITE_BLOCK_SUFFIX, UsabilityProbeFactRecord,
        UsabilityProbeRunFinalization, current_schema_is_recognized,
        node_configuration_fingerprint, sync_parent_directory,
    };
    use crate::controller::{NodeReachabilityAssessment, ProbeOutcome, ReachabilityAssessment};
    use crate::node_quality_path::QUALITY_RECONCILIATION_LOCK_SUFFIX;
    use crate::sustained_quality::{
        NodeSustainedQuality, SustainedCompletion, SustainedProbeOutcome,
    };
    use std::cell::Cell;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Barrier, mpsc};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn test_db_path() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("sing-box-tui-test-{nanos}.sqlite3"))
    }

    fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
        let mut text = path.as_os_str().to_os_string();
        text.push(suffix);
        PathBuf::from(text)
    }

    fn remove_test_db(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(sqlite_sidecar_path(path, "-wal"));
        let _ = std::fs::remove_file(sqlite_sidecar_path(path, "-shm"));
        let _ = std::fs::remove_file(sqlite_sidecar_path(path, "-journal"));
        let _ = std::fs::remove_file(sqlite_sidecar_path(path, QUALITY_WRITE_BLOCK_SUFFIX));
        let _ = std::fs::remove_file(sqlite_sidecar_path(
            path,
            QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX,
        ));
        let _ = std::fs::remove_file(sqlite_sidecar_path(
            path,
            QUALITY_RECONCILIATION_LOCK_SUFFIX,
        ));
    }

    fn complete_assessment(node: &str, delay_ms: u64) -> NodeReachabilityAssessment {
        NodeReachabilityAssessment::from_attempts(
            node.to_string(),
            vec![
                ProbeOutcome::Reachable { delay_ms },
                ProbeOutcome::Reachable {
                    delay_ms: delay_ms + 1,
                },
                ProbeOutcome::Reachable {
                    delay_ms: delay_ms + 2,
                },
            ],
        )
    }

    fn bind_test_identities(store: &BenchmarkStore, nodes: &[&str]) {
        let mut outbounds = vec![serde_json::json!({
            "type": "selector",
            "tag": "select",
            "outbounds": nodes,
        })];
        outbounds.extend(
            nodes
                .iter()
                .map(|node| serde_json::json!({"type":"direct", "tag":node})),
        );
        store
            .reconcile_node_history(&node_config(outbounds))
            .expect("bind test node identities");
    }

    fn node_config(outbounds: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({ "outbounds": outbounds })
    }

    fn seed_recognized_legacy_database(path: &Path) {
        let legacy = rusqlite::Connection::open(path).expect("create legacy database");
        legacy
            .execute_batch(
                r#"
                CREATE TABLE benchmark_results (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    recorded_at_ms INTEGER NOT NULL,
                    selector TEXT NOT NULL,
                    node TEXT NOT NULL,
                    filter TEXT NOT NULL,
                    delay_ms INTEGER,
                    completed INTEGER NOT NULL,
                    job_kind TEXT NOT NULL
                );
                CREATE INDEX idx_benchmark_results_recorded_at
                    ON benchmark_results(recorded_at_ms);
                CREATE INDEX idx_benchmark_results_selector_node
                    ON benchmark_results(selector, node);
                PRAGMA user_version = 0;
                "#,
            )
            .expect("seed recognized legacy schema");
    }

    fn seed_published_v2_database(path: &Path, complete_column: bool) {
        let connection = rusqlite::Connection::open(path).expect("create v2 database");
        let complete_column = if complete_column {
            ", complete INTEGER NOT NULL"
        } else {
            ""
        };
        connection
            .execute_batch(&format!(
                r#"
                CREATE TABLE benchmark_results (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    recorded_at_ms INTEGER NOT NULL,
                    selector TEXT NOT NULL,
                    node TEXT NOT NULL,
                    filter TEXT NOT NULL,
                    delay_ms INTEGER,
                    completed INTEGER NOT NULL,
                    job_kind TEXT NOT NULL
                );
                CREATE INDEX idx_benchmark_results_recorded_at
                    ON benchmark_results(recorded_at_ms);
                CREATE INDEX idx_benchmark_results_selector_node_recent
                    ON benchmark_results(selector, node, recorded_at_ms DESC, id DESC);
                CREATE TABLE reachability_assessments (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    recorded_at_ms INTEGER NOT NULL,
                    selector TEXT NOT NULL,
                    node TEXT NOT NULL{complete_column}
                );
                CREATE TABLE probe_attempts (
                    assessment_id INTEGER NOT NULL REFERENCES reachability_assessments(id) ON DELETE CASCADE,
                    attempt_index INTEGER NOT NULL,
                    outcome_kind TEXT NOT NULL,
                    delay_ms INTEGER,
                    detail TEXT,
                    controller_status INTEGER,
                    PRIMARY KEY (assessment_id, attempt_index)
                );
                CREATE INDEX idx_reachability_assessments_selector_node_recent
                    ON reachability_assessments(selector, node, recorded_at_ms DESC, id DESC);
                INSERT INTO benchmark_results (
                    recorded_at_ms, selector, node, filter, delay_ms, completed, job_kind
                ) VALUES (1, 'select', 'legacy-node', 'all', 42, 1, 'manual');
                PRAGMA user_version = 2;
                "#,
            ))
            .expect("seed published v2 schema");
    }

    fn seed_published_v4_database_with_facts(path: &Path) {
        let connection = rusqlite::Connection::open(path).expect("create v4 database");
        connection
            .execute_batch(
                r#"
                PRAGMA foreign_keys = ON;
                CREATE TABLE benchmark_results (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    recorded_at_ms INTEGER NOT NULL,
                    selector TEXT NOT NULL,
                    node TEXT NOT NULL,
                    filter TEXT NOT NULL,
                    delay_ms INTEGER,
                    completed INTEGER NOT NULL,
                    job_kind TEXT NOT NULL
                );
                CREATE INDEX idx_benchmark_results_recorded_at
                    ON benchmark_results(recorded_at_ms);
                CREATE INDEX idx_benchmark_results_selector_node_recent
                    ON benchmark_results(selector, node, recorded_at_ms DESC, id DESC);
                CREATE TABLE reachability_assessments (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    recorded_at_ms INTEGER NOT NULL,
                    selector TEXT NOT NULL,
                    node TEXT NOT NULL,
                    complete INTEGER NOT NULL
                );
                CREATE TABLE probe_attempts (
                    assessment_id INTEGER NOT NULL REFERENCES reachability_assessments(id) ON DELETE CASCADE,
                    attempt_index INTEGER NOT NULL,
                    outcome_kind TEXT NOT NULL,
                    delay_ms INTEGER,
                    detail TEXT,
                    controller_status INTEGER,
                    PRIMARY KEY (assessment_id, attempt_index)
                );
                CREATE INDEX idx_reachability_assessments_selector_node_recent
                    ON reachability_assessments(selector, node, recorded_at_ms DESC, id DESC);
                CREATE TABLE node_identities (
                    tag TEXT PRIMARY KEY,
                    fingerprint TEXT NOT NULL
                );
                CREATE TABLE node_quality_state (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    generation INTEGER NOT NULL,
                    identities_initialized INTEGER NOT NULL
                );
                CREATE TABLE sustained_probe_results (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    recorded_at_ms INTEGER NOT NULL,
                    selector TEXT NOT NULL,
                    node_tag TEXT NOT NULL,
                    target_identity TEXT NOT NULL,
                    outcome_kind TEXT NOT NULL,
                    first_byte_ms INTEGER,
                    completion_ms INTEGER,
                    bytes_read INTEGER,
                    detail TEXT
                );
                CREATE INDEX idx_sustained_probe_selector_node_target_recent
                    ON sustained_probe_results(
                        selector, node_tag, target_identity, recorded_at_ms DESC, id DESC
                    );

                INSERT INTO node_quality_state VALUES (1, 17, 1);
                INSERT INTO node_identities VALUES ('node-a', 'fingerprint-a');
                INSERT INTO node_identities VALUES ('select', 'fingerprint-selector');
                INSERT INTO benchmark_results VALUES (
                    3, 1000, 'select', 'node-a', 'all', 42, 1, 'manual'
                );
                INSERT INTO reachability_assessments VALUES (5, 1100, 'select', 'node-a', 1);
                INSERT INTO probe_attempts VALUES (5, 0, 'reachable', 42, NULL, NULL);
                INSERT INTO probe_attempts VALUES (5, 1, 'timeout', NULL, NULL, NULL);
                INSERT INTO probe_attempts VALUES (5, 2, 'reachable', 44, NULL, NULL);
                INSERT INTO sustained_probe_results VALUES (
                    7, 1200, 'select', 'node-a', 'target-v4', 'completed',
                    80, 500, 524288, NULL
                );
                PRAGMA user_version = 4;
                "#,
            )
            .expect("seed published v4 schema and facts");
    }

    #[test]
    fn node_fingerprint_canonicalizes_object_keys_but_keeps_every_material_value() {
        let original: serde_json::Value = serde_json::from_str(
            r#"{
                "type":"vless",
                "tag":"node-a",
                "server":"proxy.example",
                "server_port":443,
                "uuid":"credential-secret",
                "tls":{"enabled":true,"server_name":"edge.example","alpn":["h2","http/1.1"]},
                "transport":{"type":"ws","path":"/proxy"}
            }"#,
        )
        .expect("parse original outbound");
        let reordered: serde_json::Value = serde_json::from_str(
            r#"{
                "transport":{"path":"/proxy","type":"ws"},
                "tls":{"alpn":["h2","http/1.1"],"server_name":"edge.example","enabled":true},
                "uuid":"credential-secret",
                "server_port":443,
                "server":"proxy.example",
                "tag":"node-a",
                "type":"vless"
            }"#,
        )
        .expect("parse reordered outbound");

        let fingerprint = node_configuration_fingerprint(&original).expect("fingerprint node");
        assert_eq!(
            fingerprint,
            node_configuration_fingerprint(&reordered).expect("fingerprint reordered node")
        );
        assert_eq!(fingerprint.len(), 64, "SHA-256 is encoded as lowercase hex");
        assert!(fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()));

        for changed in [
            serde_json::json!({
                "type":"trojan", "tag":"node-a", "server":"proxy.example",
                "server_port":443, "uuid":"credential-secret",
                "tls":{"enabled":true,"server_name":"edge.example","alpn":["h2","http/1.1"]},
                "transport":{"type":"ws","path":"/proxy"}
            }),
            serde_json::json!({
                "type":"vless", "tag":"node-a", "server":"other.example",
                "server_port":443, "uuid":"credential-secret",
                "tls":{"enabled":true,"server_name":"edge.example","alpn":["h2","http/1.1"]},
                "transport":{"type":"ws","path":"/proxy"}
            }),
            serde_json::json!({
                "type":"vless", "tag":"node-a", "server":"proxy.example",
                "server_port":8443, "uuid":"credential-secret",
                "tls":{"enabled":true,"server_name":"edge.example","alpn":["h2","http/1.1"]},
                "transport":{"type":"ws","path":"/proxy"}
            }),
            serde_json::json!({
                "type":"vless", "tag":"node-a", "server":"proxy.example",
                "server_port":443, "uuid":"different-secret",
                "tls":{"enabled":true,"server_name":"edge.example","alpn":["h2","http/1.1"]},
                "transport":{"type":"ws","path":"/proxy"}
            }),
            serde_json::json!({
                "type":"vless", "tag":"node-a", "server":"proxy.example",
                "server_port":443, "uuid":"credential-secret",
                "tls":{"enabled":true,"server_name":"edge.example","alpn":["http/1.1","h2"]},
                "transport":{"type":"ws","path":"/proxy"}
            }),
            serde_json::json!({
                "type":"vless", "tag":"node-a", "server":"proxy.example",
                "server_port":443, "uuid":"credential-secret",
                "tls":{"enabled":true,"server_name":"edge.example","alpn":["h2","http/1.1"]},
                "transport":{"type":"grpc","path":"/proxy"}
            }),
        ] {
            assert_ne!(
                fingerprint,
                node_configuration_fingerprint(&changed).expect("fingerprint changed node")
            );
        }
    }

    #[test]
    fn in_memory_store_uses_no_filesystem_quality_lock() {
        let guard = super::lock_node_quality_reconciliation(Path::new(":memory:"))
            .expect("create in-memory quality guard");
        assert!(guard._file.is_none());
        let store = BenchmarkStore::open(":memory:").expect("open in-memory benchmark store");
        assert_eq!(store.quality_generation(), 0);
    }

    #[test]
    fn uninitialized_node_identities_reject_all_quality_facts() {
        let store = BenchmarkStore::open(":memory:").expect("open in-memory benchmark store");

        assert!(
            !store
                .record_benchmark(&BenchmarkRecord {
                    selector: "select",
                    node: "node-a",
                    filter: "",
                    delay_ms: Some(42),
                    completed: true,
                    job_kind: "single",
                })
                .expect("attempt unbound benchmark write")
        );
        assert!(
            !store
                .record_reachability_assessment("select", &complete_assessment("node-a", 40),)
                .expect("attempt unbound reachability write")
        );
        assert!(
            store
                .recent_benchmarks(10)
                .expect("query benchmark facts")
                .is_empty()
        );
        assert!(
            store
                .latest_reachability_assessments()
                .expect("query reachability facts")
                .is_empty()
        );
    }

    #[test]
    fn current_generation_rejects_facts_for_tags_outside_bound_identities() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open node-quality store");
        bind_test_identities(&store, &["node-a"]);

        assert!(
            !store
                .record_benchmark(&BenchmarkRecord {
                    selector: "select",
                    node: "external-only-node",
                    filter: "",
                    delay_ms: Some(42),
                    completed: true,
                    job_kind: "single",
                })
                .expect("reject out-of-snapshot benchmark fact")
        );
        assert!(
            !store
                .record_reachability_assessment(
                    "select",
                    &complete_assessment("external-only-node", 40),
                )
                .expect("reject out-of-snapshot reachability fact")
        );
        assert!(store.recent_benchmarks(10).unwrap().is_empty());
        assert!(store.latest_reachability_assessments().unwrap().is_empty());

        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn first_identity_reconciliation_discards_unattributed_existing_facts() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open node-quality store");
        store
            .connection
            .execute(
                "INSERT INTO reachability_assessments (recorded_at_ms, selector, node, complete) VALUES (1, 'select', 'node-a', 1)",
                [],
            )
            .expect("seed unattributed assessment from a pre-binding writer");
        let assessment_id = store.connection.last_insert_rowid();
        for attempt_index in 0..3 {
            store
                .connection
                .execute(
                    "INSERT INTO probe_attempts (assessment_id, attempt_index, outcome_kind, delay_ms) VALUES (?1, ?2, 'reachable', ?3)",
                    rusqlite::params![assessment_id, attempt_index, 40 + attempt_index],
                )
                .expect("seed unattributed probe attempt");
        }

        store
            .reconcile_node_history(&node_config(vec![serde_json::json!({
                "type":"trojan", "tag":"node-a", "server":"same.example",
                "server_port":443, "password":"must-not-be-stored"
            })]))
            .expect("establish first identity snapshot");

        assert!(
            store
                .latest_reachability_assessments()
                .expect("query history")
                .is_empty(),
            "facts without a persisted fingerprint cannot be proven unchanged"
        );
        let identities = store.stored_node_identities().expect("query identities");
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].0, "node-a");
        assert_eq!(identities[0].1.len(), 64);
        assert!(!identities[0].1.contains("must-not-be-stored"));
        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn concurrent_first_opens_serialize_legacy_reset_and_schema_initialization() {
        let path = test_db_path();
        seed_recognized_legacy_database(&path);

        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let worker_path = path.clone();
            let worker_barrier = barrier.clone();
            workers.push(thread::spawn(move || {
                worker_barrier.wait();
                let store = BenchmarkStore::open(&worker_path)
                    .expect("concurrent open serializes schema upgrade");
                assert_eq!(store.quality_generation(), 0);
                store
            }));
        }
        barrier.wait();
        let stores = workers
            .into_iter()
            .map(|worker| worker.join().expect("schema worker exits"))
            .collect::<Vec<_>>();

        assert_eq!(
            stores[0]
                .connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("read first schema version"),
            NODE_QUALITY_SCHEMA_VERSION
        );
        assert_eq!(
            stores[1]
                .connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("read second schema version"),
            NODE_QUALITY_SCHEMA_VERSION
        );
        assert!(
            stores[0]
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='node_quality_state')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .expect("verify initialized schema")
        );

        drop(stores);
        remove_test_db(&path);
    }

    #[test]
    fn empty_file_and_empty_sqlite_header_recover_to_atomic_current_schema() {
        for (label, create) in [("zero-bytes", false), ("sqlite-header", true)] {
            let path = test_db_path().with_extension(format!("{label}.sqlite3"));
            if create {
                drop(rusqlite::Connection::open(&path).expect("create empty SQLite header"));
            } else {
                std::fs::write(&path, []).expect("create zero-byte candidate");
            }

            let store = BenchmarkStore::open(&path).expect("initialize empty database candidate");
            assert_eq!(
                store
                    .connection
                    .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                    .expect("read initialized schema version"),
                NODE_QUALITY_SCHEMA_VERSION
            );
            assert!(
                current_schema_is_recognized(&store.connection)
                    .expect("validate complete atomic schema")
            );
            drop(store);
            remove_test_db(&path);
        }
    }

    #[test]
    fn both_published_v2_shapes_are_rebuilt_as_current_without_old_facts() {
        for complete_column in [false, true] {
            let path = test_db_path().with_extension(if complete_column {
                "v2b.sqlite3"
            } else {
                "v2a.sqlite3"
            });
            seed_published_v2_database(&path, complete_column);

            let store = BenchmarkStore::open(&path).expect("rebuild recognized v2 database");
            assert_eq!(
                store
                    .connection
                    .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                    .expect("read rebuilt schema version"),
                NODE_QUALITY_SCHEMA_VERSION
            );
            assert!(
                store
                    .recent_benchmarks(10)
                    .expect("read rebuilt benchmark history")
                    .is_empty(),
                "published v2 facts are discarded before old processes can target the current inode"
            );
            assert!(
                store
                    .connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                         WHERE type='table' AND name='node_identities')",
                        [],
                        |row| row.get::<_, bool>(0),
                    )
                    .expect("verify current identity table")
            );
            drop(store);
            remove_test_db(&path);
        }
    }

    #[test]
    fn exact_v4_schema_migrates_additively_without_losing_quality_facts() {
        let path = test_db_path().with_extension("v4-facts.sqlite3");
        seed_published_v4_database_with_facts(&path);

        let store = BenchmarkStore::open(&path).expect("migrate trusted v4 database");

        assert_eq!(store.quality_generation(), 17);
        assert_eq!(
            store
                .connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("read migrated schema version"),
            NODE_QUALITY_SCHEMA_VERSION
        );
        assert!(
            current_schema_is_recognized(&store.connection)
                .expect("recognize migrated current schema")
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT fingerprint FROM node_identities WHERE tag = 'node-a'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read preserved identity"),
            "fingerprint-a"
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT delay_ms FROM benchmark_results WHERE id = 3",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read preserved quick result"),
            42
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM probe_attempts WHERE assessment_id = 5",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read preserved reachability attempts"),
            3
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT bytes_read FROM sustained_probe_results WHERE id = 7",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read preserved sustained result"),
            524_288
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name LIKE 'usability_probe_%'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count additive usability tables"),
            3
        );

        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn exact_v5_schema_adds_expiry_without_losing_complete_usability_results() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("initialize v6 store");
        bind_test_identities(&store, &["node-a"]);
        let (run_id, generation) = store
            .begin_usability_probe_run("criterion", "select", store.quality_generation())
            .expect("begin v5 fixture run")
            .expect("quality generation current");
        store
            .finish_usability_probe_run(
                run_id,
                generation,
                true,
                Some("v5 complete"),
                None,
                &[UsabilityProbeFactRecord {
                    node: "node-a".to_string(),
                    usable: true,
                    detail: None,
                }],
            )
            .expect("publish v5 fixture run");
        store
            .connection
            .execute_batch(
                "ALTER TABLE usability_probe_runs DROP COLUMN expires_at_ms; PRAGMA user_version = 5;",
            )
            .expect("downgrade fixture to exact published v5 shape");
        drop(store);

        let migrated = BenchmarkStore::open(&path).expect("migrate exact v5 schema");
        let run = migrated
            .latest_usability_probe_run("criterion", "select", &["node-a".to_string()])
            .expect("read migrated result")
            .expect("complete result retained");
        assert_eq!(run.run_id, run_id);
        assert_eq!(run.summary.as_deref(), Some("v5 complete"));
        assert_eq!(run.expires_at_ms, None);
        assert_eq!(
            migrated
                .connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("read migrated version"),
            NODE_QUALITY_SCHEMA_VERSION
        );

        drop(migrated);
        remove_test_db(&path);
    }

    #[test]
    fn legacy_reset_preflights_all_sidecars_before_deleting_the_main_database() {
        let path = test_db_path().with_extension("blocked-reset.sqlite3");
        seed_published_v2_database(&path, true);
        let before = std::fs::read(&path).expect("read published v2 database");
        let wal = sqlite_sidecar_path(&path, "-wal");
        std::fs::create_dir(&wal).expect("create undeletable sidecar shape");

        let error = BenchmarkStore::open(&path)
            .err()
            .expect("non-regular sidecar must stop legacy reset");
        assert!(format!("{error:#}").contains("non-regular legacy SQLite sidecar"));
        assert_eq!(
            std::fs::read(&path).expect("read preserved published database"),
            before,
            "the main database must remain intact when sidecar preflight fails"
        );

        std::fs::remove_dir(&wal).expect("remove sidecar directory");
        remove_test_db(&path);
    }

    #[test]
    fn missing_or_zero_byte_database_with_orphaned_sidecar_fails_closed() {
        for create_main in [false, true] {
            let path = test_db_path().with_extension(if create_main {
                "zero-with-orphan.sqlite3"
            } else {
                "missing-with-orphan.sqlite3"
            });
            if create_main {
                std::fs::write(&path, []).expect("create zero-byte database candidate");
            }
            let journal = sqlite_sidecar_path(&path, "-journal");
            std::fs::write(&journal, b"orphan-canary\n").expect("create orphaned journal");

            let error = BenchmarkStore::open(&path)
                .err()
                .expect("orphaned sidecar must stop database initialization");
            assert!(format!("{error:#}").contains("orphaned SQLite sidecar"));
            assert_eq!(
                std::fs::read(&journal).expect("read preserved orphan"),
                b"orphan-canary\n"
            );
            assert_eq!(path.exists(), create_main);
            if create_main {
                assert_eq!(
                    std::fs::metadata(&path)
                        .expect("inspect zero-byte database")
                        .len(),
                    0
                );
            }

            remove_test_db(&path);
        }
    }

    #[test]
    fn malformed_current_schema_is_preserved() {
        let path = test_db_path().with_extension("v6-trigger.sqlite3");
        drop(BenchmarkStore::open(&path).expect("create recognized v6 database"));
        let connection = rusqlite::Connection::open(&path).expect("open v6 fixture");
        connection
            .execute_batch(
                "CREATE TRIGGER unrelated_v6_trigger AFTER INSERT ON benchmark_results \
                 BEGIN SELECT 1; END;",
            )
            .expect("add unknown v6 trigger");
        drop(connection);
        let before = std::fs::read(&path).expect("read v6 candidate");
        let error = BenchmarkStore::open(&path)
            .err()
            .expect("behavior-changing v6 trigger must be rejected");
        assert!(format!("{error:#}").contains("unrecognized version 6"));
        assert_eq!(
            std::fs::read(&path).expect("read preserved v6 candidate"),
            before
        );
        remove_test_db(&path);
    }

    #[test]
    fn every_legacy_sqlite_schema_is_rebuilt_even_with_unrelated_objects() {
        for version in [-1, 0, 1, 2, 3] {
            let path = test_db_path().with_extension(format!("legacy-v{version}.sqlite3"));
            let connection = rusqlite::Connection::open(&path).expect("create legacy database");
            connection
                .execute_batch(&format!(
                    "CREATE TABLE unrelated (value TEXT); \
                     CREATE INDEX unrelated_index ON unrelated(value); \
                     CREATE VIEW unrelated_view AS SELECT value FROM unrelated; \
                     INSERT INTO unrelated VALUES ('legacy fact'); \
                     PRAGMA user_version = {version};"
                ))
                .expect("seed unrelated legacy objects");
            drop(connection);
            let old_runtime_fence = sqlite_sidecar_path(&path, QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX);
            std::fs::write(&old_runtime_fence, b"legacy runtime fence\n")
                .expect("seed legacy runtime fence");

            let store = BenchmarkStore::open(&path).expect("rebuild every legacy SQLite schema");
            assert_eq!(
                store
                    .connection
                    .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                    .expect("read rebuilt version"),
                NODE_QUALITY_SCHEMA_VERSION
            );
            assert!(
                current_schema_is_recognized(&store.connection)
                    .expect("validate rebuilt current schema")
            );
            assert!(
                !store
                    .connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='unrelated')",
                        [],
                        |row| row.get::<_, bool>(0),
                    )
                    .expect("check unrelated legacy object removal")
            );
            assert!(
                !old_runtime_fence.exists(),
                "a whole-database reset must remove every legacy protocol sidecar"
            );
            drop(store);
            remove_test_db(&path);
        }
    }

    #[test]
    fn unrecognized_files_and_database_schemas_are_never_replaced() {
        let json_path = test_db_path();
        let json = br#"{"ordinary":"configuration"}\n"#;
        std::fs::write(&json_path, json).expect("write ordinary JSON file");
        let error = BenchmarkStore::open(&json_path)
            .err()
            .expect("ordinary file must be rejected");
        assert!(
            format!("{error:#}").contains("schema version"),
            "unexpected diagnostic: {error:#}"
        );
        assert_eq!(
            std::fs::read(&json_path).expect("read preserved JSON"),
            json
        );
        remove_test_db(&json_path);

        for (label, schema) in [
            (
                "spoofed-v4",
                "CREATE TABLE unrelated (value TEXT); PRAGMA user_version = 4;",
            ),
            (
                "spoofed-current",
                "CREATE TABLE unrelated (value TEXT); PRAGMA user_version = 5;",
            ),
            (
                "future",
                "CREATE TABLE benchmark_results (id INTEGER); PRAGMA user_version = 99;",
            ),
        ] {
            let path = test_db_path().with_extension(format!("{label}.sqlite3"));
            let connection = rusqlite::Connection::open(&path).expect("create candidate database");
            connection
                .execute_batch(schema)
                .expect("seed candidate schema");
            drop(connection);
            let before = std::fs::read(&path).expect("read candidate bytes");

            let error = BenchmarkStore::open(&path)
                .err()
                .expect("unknown schema must be rejected");
            assert!(format!("{error:#}").contains("refusing to"));
            assert_eq!(
                std::fs::read(&path).expect("read preserved database"),
                before,
                "{label} database must remain byte-for-byte unchanged"
            );
            remove_test_db(&path);
        }
    }

    #[test]
    fn unchanged_identity_snapshot_keeps_the_existing_quality_generation() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open node-quality store");
        let first = store
            .reconcile_node_history(&node_config(vec![serde_json::json!({
                "type":"trojan", "tag":"node-a", "server":"same.example",
                "server_port":443, "password":"secret"
            })]))
            .expect("establish identity generation");
        let second = store
            .reconcile_node_history(&node_config(vec![serde_json::json!({
                "password":"secret", "server_port":443, "server":"same.example",
                "tag":"node-a", "type":"trojan"
            })]))
            .expect("reconcile unchanged identity");

        assert!(first.identities_changed);
        assert!(!second.identities_changed);
        assert_eq!(first.generation, second.generation);

        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn quality_read_lease_blocks_reconciliation_and_stale_generation_cannot_reacquire() {
        let path = test_db_path();
        let reader = BenchmarkStore::open(&path).expect("open quality reader");
        let initial = reader
            .reconcile_node_history(&node_config(vec![
                serde_json::json!({"type":"direct", "tag":"node-a"}),
            ]))
            .expect("bind initial identity");
        let reconciler = BenchmarkStore::open(&path).expect("open reconciler");
        let lease = reader
            .acquire_quality_read_lease()
            .expect("acquire quality lease")
            .expect("current generation has a lease");
        assert_eq!(lease.generation(), initial.generation);

        let (attempted_tx, attempted_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            let result = reconciler.reconcile_node_history(&node_config(vec![
                serde_json::json!({"type":"direct", "tag":"node-b"}),
            ]));
            finished_tx.send(result).unwrap();
        });
        attempted_rx.recv().unwrap();
        assert!(
            finished_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "reconciliation must wait while the final quality decision holds its lease"
        );
        drop(lease);
        let refreshed = finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("reconciliation resumes after lease release")
            .expect("reconcile replacement identity");
        worker.join().unwrap();
        assert!(refreshed.generation > initial.generation);
        assert!(
            reader
                .acquire_quality_read_lease()
                .expect("inspect stale reader")
                .is_none(),
            "a stale process cannot reacquire a decision lease from only its cached generation"
        );
        drop(reader);
        remove_test_db(&path);
    }

    #[test]
    fn unchanged_named_selector_urltest_and_direct_outbounds_keep_their_facts() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open node-quality store");
        let config = node_config(vec![
            serde_json::json!({
                "type":"selector", "tag":"select",
                "outbounds":["auto", "direct", "proxy"]
            }),
            serde_json::json!({
                "type":"urltest", "tag":"auto", "outbounds":["proxy"]
            }),
            serde_json::json!({"type":"direct", "tag":"direct"}),
            serde_json::json!({
                "type":"trojan", "tag":"proxy", "server":"same.example",
                "server_port":443, "password":"secret"
            }),
        ]);
        store
            .reconcile_node_history(&config)
            .expect("bind every named outbound identity");
        for (index, node) in ["select", "auto", "direct", "proxy"]
            .into_iter()
            .enumerate()
        {
            assert!(
                store
                    .record_reachability_assessment(
                        "select",
                        &complete_assessment(node, 40 + index as u64),
                    )
                    .expect("record named outbound fact")
            );
        }

        let reconciliation = store
            .reconcile_node_history(&config)
            .expect("reconcile unchanged named outbounds");

        assert!(!reconciliation.identities_changed);
        let retained = store
            .latest_reachability_assessments()
            .expect("read retained named outbound facts")
            .into_iter()
            .map(|(_, assessment)| assessment.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            retained,
            BTreeSet::from([
                "auto".to_string(),
                "direct".to_string(),
                "proxy".to_string(),
                "select".to_string(),
            ])
        );

        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn reconciliation_preserves_only_unchanged_tag_and_fingerprint_history() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open node-quality store");
        let old_config = node_config(vec![
            serde_json::json!({
                "type":"trojan", "tag":"unchanged", "server":"same.example",
                "server_port":443, "password":"secret-a", "tls":{"enabled":true}
            }),
            serde_json::json!({
                "type":"trojan", "tag":"removed", "server":"removed.example",
                "server_port":443, "password":"secret-b"
            }),
            serde_json::json!({
                "type":"vless", "tag":"changed", "server":"old.example",
                "server_port":443, "uuid":"secret-c"
            }),
            serde_json::json!({
                "type":"hysteria2", "tag":"old-name", "server":"rename.example",
                "server_port":443, "password":"secret-d"
            }),
        ]);
        store
            .reconcile_node_history(&old_config)
            .expect("seed identity snapshot");
        for (index, node) in ["unchanged", "removed", "changed", "old-name"]
            .into_iter()
            .enumerate()
        {
            store
                .record_reachability_assessment(
                    "select",
                    &complete_assessment(node, 40 + index as u64 * 10),
                )
                .expect("seed assessment");
            store
                .record_benchmark(&BenchmarkRecord {
                    selector: "select",
                    node,
                    filter: "all",
                    delay_ms: Some(40 + index as u64 * 10),
                    completed: true,
                    job_kind: "manual",
                })
                .expect("seed benchmark fact");
        }

        let new_config: serde_json::Value = serde_json::from_str(
            r#"{
                "outbounds":[
                    {"tls":{"enabled":true},"password":"secret-a","server_port":443,"server":"same.example","tag":"unchanged","type":"trojan"},
                    {"type":"vless","tag":"changed","server":"new.example","server_port":443,"uuid":"secret-c"},
                    {"type":"hysteria2","tag":"new-name","server":"rename.example","server_port":443,"password":"secret-d"},
                    {"type":"trojan","tag":"added","server":"added.example","server_port":443,"password":"secret-e"}
                ]
            }"#,
        )
        .expect("parse refreshed config");
        store
            .reconcile_node_history(&new_config)
            .expect("reconcile history");

        let assessments = store
            .latest_reachability_assessments()
            .expect("query reconciled assessments");
        assert_eq!(assessments.len(), 1);
        assert_eq!(assessments[0].1.name, "unchanged");
        let benchmarks = store.recent_benchmarks(20).expect("query benchmarks");
        assert_eq!(benchmarks.len(), 1);
        assert_eq!(benchmarks[0].node, "unchanged");
        assert_eq!(
            store
                .stored_node_identities()
                .expect("query refreshed identities")
                .into_iter()
                .map(|(tag, _)| tag)
                .collect::<Vec<_>>(),
            vec!["added", "changed", "new-name", "unchanged"]
        );
        let attempts: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM probe_attempts", [], |row| row.get(0))
            .expect("count retained attempt facts");
        assert_eq!(attempts, 3);

        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn stale_cross_process_store_cannot_resurrect_removed_or_changed_node_facts() {
        let path = test_db_path();
        let stale_worker = BenchmarkStore::open(&path).expect("open background worker store");
        let old_config = node_config(vec![
            serde_json::json!({
                "type":"trojan", "tag":"removed", "server":"removed.example",
                "server_port":443, "password":"old-secret"
            }),
            serde_json::json!({
                "type":"trojan", "tag":"changed", "server":"old.example",
                "server_port":443, "password":"old-secret"
            }),
        ]);
        let initial = stale_worker
            .reconcile_node_history(&old_config)
            .expect("seed old identity generation");
        assert!(initial.identities_changed);

        let reconciler = BenchmarkStore::open(&path).expect("open subscription worker store");
        let refreshed = reconciler
            .reconcile_node_history(&node_config(vec![serde_json::json!({
                "type":"trojan", "tag":"changed", "server":"new.example",
                "server_port":443, "password":"new-secret"
            })]))
            .expect("commit refreshed identity generation");
        assert!(refreshed.identities_changed);
        assert!(refreshed.generation > initial.generation);

        assert!(
            !stale_worker
                .record_benchmark(&BenchmarkRecord {
                    selector: "select",
                    node: "removed",
                    filter: "all",
                    delay_ms: Some(40),
                    completed: true,
                    job_kind: "auto",
                })
                .expect("reject stale removed-node result")
        );
        assert!(
            !stale_worker
                .record_reachability_assessment("select", &complete_assessment("changed", 50),)
                .expect("reject stale same-tag changed-node assessment")
        );
        assert!(
            reconciler
                .recent_benchmarks(10)
                .expect("query benchmark facts")
                .is_empty()
        );
        assert!(
            reconciler
                .latest_reachability_assessments()
                .expect("query reachability facts")
                .is_empty()
        );

        drop(stale_worker);
        let restarted_worker =
            BenchmarkStore::open(&path).expect("bind restarted worker to current generation");
        assert!(
            restarted_worker
                .record_benchmark(&BenchmarkRecord {
                    selector: "select",
                    node: "changed",
                    filter: "all",
                    delay_ms: Some(60),
                    completed: true,
                    job_kind: "auto",
                })
                .expect("accept result after managed config restart")
        );

        drop(reconciler);
        drop(restarted_worker);
        remove_test_db(&path);
    }

    #[test]
    fn waiting_old_generation_writer_is_rejected_across_reconciliation_commit() {
        let path = test_db_path();
        let initial_store = BenchmarkStore::open(&path).expect("open initial quality store");
        initial_store
            .reconcile_node_history(&node_config(vec![serde_json::json!({
                "type":"trojan", "tag":"node-a", "server":"old.example",
                "server_port":443, "password":"old-secret"
            })]))
            .expect("seed old identity generation");
        drop(initial_store);

        let stale_path = path.clone();
        let (writer_ready_tx, writer_ready_rx) = mpsc::channel();
        let (writer_start_tx, writer_start_rx) = mpsc::channel();
        let (writer_done_tx, writer_done_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            let stale_store =
                BenchmarkStore::open(&stale_path).expect("open stale worker connection");
            writer_ready_tx.send(()).expect("report stale writer ready");
            writer_start_rx.recv().expect("wait for reconciliation");
            let accepted = stale_store
                .record_benchmark(&BenchmarkRecord {
                    selector: "select",
                    node: "node-a",
                    filter: "all",
                    delay_ms: Some(42),
                    completed: true,
                    job_kind: "auto",
                })
                .expect("stale writer returns a guarded outcome");
            writer_done_tx
                .send(accepted)
                .expect("report stale writer outcome");
        });
        writer_ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("stale writer opens before reconciliation");

        let reconciler = BenchmarkStore::open(&path).expect("open reconciliation connection");
        let transaction = reconciler
            .begin_node_history_reconciliation()
            .expect("acquire immediate reconciliation transaction");
        assert!(
            reconciler
                .ensure_quality_writes_blocked()
                .expect("create quality write block")
        );
        let reconciliation = transaction
            .apply(&node_config(vec![serde_json::json!({
                "type":"trojan", "tag":"node-a", "server":"new.example",
                "server_port":443, "password":"new-secret"
            })]))
            .expect("apply new identities");
        writer_start_tx
            .send(())
            .expect("start writer behind immediate transaction");
        assert!(
            writer_done_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "the old writer must wait for the reconciliation transaction"
        );

        transaction
            .commit(reconciliation)
            .expect("commit new identity generation");
        assert!(
            !writer_done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("old writer finishes after commit"),
            "the marker must reject a writer that acquires the DB lock at the commit boundary"
        );
        reconciler
            .clear_quality_write_block()
            .expect("clear marker after observing the guarded writer");
        writer.join().expect("stale writer exits");
        assert!(
            reconciler
                .recent_benchmarks(10)
                .expect("query guarded benchmark rows")
                .is_empty()
        );

        drop(reconciler);
        remove_test_db(&path);
    }

    #[test]
    fn retrying_existing_marker_redurabilizes_before_config_mutation() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open node-quality store");

        let first_error = store
            .ensure_quality_writes_blocked_with(|_| {
                Err(anyhow::anyhow!(
                    "injected first marker parent directory sync failure"
                ))
            })
            .expect_err("first marker creation must surface parent sync failure");
        assert!(format!("{first_error:#}").contains("failed to persist node-quality write block"));
        assert!(
            store
                .quality_writes_blocked()
                .expect("inspect marker left by failed create"),
            "the failed first attempt leaves a visible marker whose durability is uncertain"
        );

        let config_mutated = Cell::new(false);
        let retry_error = (|| -> anyhow::Result<()> {
            store.ensure_quality_writes_blocked_with(|_| {
                Err(anyhow::anyhow!(
                    "injected retry marker parent directory sync failure"
                ))
            })?;
            config_mutated.set(true);
            Ok(())
        })()
        .expect_err("an existing marker must be re-persisted before config mutation");
        assert!(format!("{retry_error:#}").contains("failed to persist node-quality write block"));
        assert!(
            !config_mutated.get(),
            "a retry may not cross the config-mutation barrier until parent fsync succeeds"
        );

        let retry_parent_synced = Cell::new(false);
        assert!(
            !store
                .ensure_quality_writes_blocked_with(|marker_path| {
                    sync_parent_directory(marker_path)?;
                    retry_parent_synced.set(true);
                    Ok(())
                })
                .expect("retry re-persists existing marker"),
            "the retry must retain ownership semantics for a pre-existing marker"
        );
        assert!(
            retry_parent_synced.get(),
            "the successful retry must re-establish marker directory durability"
        );

        store
            .clear_quality_write_block()
            .expect("clear re-persisted marker");
        drop(store);
        remove_test_db(&path);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_database_aliases_share_the_same_fail_closed_marker() {
        use std::os::unix::fs::symlink;

        let path = test_db_path();
        let alias = path.with_extension("alias.sqlite3");
        let real_store = BenchmarkStore::open(&path).expect("open real database path");
        symlink(
            path.file_name().expect("database path has a file name"),
            &alias,
        )
        .expect("create database alias");
        let alias_store = BenchmarkStore::open(&alias).expect("open database through alias");

        assert_eq!(
            real_store.database_path, alias_store.database_path,
            "all guards and sidecars must derive from one canonical database target"
        );
        assert!(
            real_store
                .ensure_quality_writes_blocked()
                .expect("create marker through real path")
        );
        assert!(
            alias_store
                .quality_writes_blocked()
                .expect("inspect marker through alias"),
            "an alias must not create a separate fail-closed namespace"
        );
        alias_store
            .clear_quality_write_block()
            .expect("clear canonical marker through alias");
        assert!(
            !real_store
                .quality_writes_blocked()
                .expect("inspect cleared canonical marker")
        );

        drop(alias_store);
        drop(real_store);
        let _ = std::fs::remove_file(alias);
        remove_test_db(&path);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_database_symlink_is_rejected_before_any_alias_sidecar_is_created() {
        use std::os::unix::fs::symlink;

        let path = test_db_path();
        let alias = path.with_extension("dangling.sqlite3");
        symlink(
            path.file_name().expect("database path has a file name"),
            &alias,
        )
        .expect("create dangling database alias");

        let open_error = BenchmarkStore::open(&alias)
            .err()
            .expect("dangling database aliases must not create a new database namespace");
        assert!(format!("{open_error:#}").contains("failed to resolve"));
        let lock_error = super::lock_node_quality_reconciliation(&alias)
            .err()
            .expect("dangling database aliases must not create a lock namespace");
        assert!(format!("{lock_error:#}").contains("failed to resolve"));
        assert!(!path.exists());
        assert!(!sqlite_sidecar_path(&alias, QUALITY_RECONCILIATION_LOCK_SUFFIX).exists());

        let _ = std::fs::remove_file(alias);
    }

    #[test]
    fn reachability_assessment_round_trips_factual_attempts() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open node-quality store");
        bind_test_identities(&store, &["node-a"]);
        let assessment = NodeReachabilityAssessment {
            name: "node-a".into(),
            attempts: vec![
                ProbeOutcome::Reachable { delay_ms: 42 },
                ProbeOutcome::Timeout,
                ProbeOutcome::TransportFailure {
                    detail: "connection reset".into(),
                },
            ],
            assessment: Some(ReachabilityAssessment::Degraded),
        };
        store
            .record_reachability_assessment("select", &assessment)
            .expect("record facts");

        assert_eq!(
            store.latest_reachability_assessments().expect("load facts"),
            vec![("select".into(), assessment)]
        );
        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn incomplete_run_does_not_replace_latest_complete_assessment() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).unwrap();
        bind_test_identities(&store, &["node-a"]);
        let complete = NodeReachabilityAssessment::from_attempts(
            "node-a".into(),
            vec![
                ProbeOutcome::Reachable { delay_ms: 40 },
                ProbeOutcome::Reachable { delay_ms: 45 },
                ProbeOutcome::Timeout,
            ],
        );
        let incomplete = NodeReachabilityAssessment::from_attempts(
            "node-a".into(),
            vec![
                ProbeOutcome::Reachable { delay_ms: 50 },
                ProbeOutcome::ControllerFailure { status: 503 },
                ProbeOutcome::Timeout,
            ],
        );
        store
            .record_reachability_assessment("select", &complete)
            .unwrap();
        store
            .record_reachability_assessment("select", &incomplete)
            .unwrap();

        assert_eq!(
            store.latest_reachability_assessments().unwrap(),
            vec![("select".into(), complete)]
        );
        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn sustained_facts_round_trip_and_infrastructure_failure_preserves_evidence() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).unwrap();
        bind_test_identities(&store, &["node-a", "node-b"]);
        let completed_with_untrusted_metric = NodeSustainedQuality {
            name: "node-a".into(),
            outcome: SustainedProbeOutcome::Completed(SustainedCompletion {
                first_byte_ms: 120,
                completion_ms: 620,
                bytes_read: 512 * 1024,
                throughput_bytes_per_second: 7,
            }),
        };
        store
            .record_sustained_quality("select", "target-a", &completed_with_untrusted_metric)
            .unwrap();
        let mut columns = store
            .connection
            .prepare("PRAGMA table_info(sustained_probe_results)")
            .unwrap();
        let column_names = columns
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(!column_names.iter().any(|name| name.contains("throughput")));
        drop(columns);
        store
            .record_sustained_quality(
                "select",
                "target-a",
                &NodeSustainedQuality {
                    name: "node-a".into(),
                    outcome: SustainedProbeOutcome::RuntimeFailed {
                        detail: "isolated controller unavailable".into(),
                    },
                },
            )
            .unwrap();
        let canary = "account-token-canary";
        store
            .record_sustained_quality(
                "select",
                "target-a",
                &NodeSustainedQuality {
                    name: "node-b".into(),
                    outcome: SustainedProbeOutcome::TransferFailed {
                        detail: canary.into(),
                    },
                },
            )
            .unwrap();
        store
            .record_sustained_quality(
                "select",
                "target-b",
                &NodeSustainedQuality {
                    name: "node-a".into(),
                    outcome: SustainedProbeOutcome::TransferFailed {
                        detail: "different target".into(),
                    },
                },
            )
            .unwrap();

        let loaded = store.latest_sustained_quality("target-a").unwrap();
        assert!(!format!("{loaded:?}").contains(canary));
        assert_eq!(
            loaded,
            vec![
                (
                    "select".into(),
                    NodeSustainedQuality {
                        name: "node-a".into(),
                        outcome: SustainedProbeOutcome::Completed(SustainedCompletion {
                            first_byte_ms: 120,
                            completion_ms: 620,
                            bytes_read: 512 * 1024,
                            throughput_bytes_per_second: 1024 * 1024,
                        }),
                    },
                ),
                (
                    "select".into(),
                    NodeSustainedQuality {
                        name: "node-b".into(),
                        outcome: SustainedProbeOutcome::TransferFailed {
                            detail: "sustained transfer failed".into(),
                        },
                    },
                )
            ]
        );
        assert_eq!(
            store
                .sustained_success_stats("select", "node-a", "target-a", 10)
                .unwrap(),
            super::SustainedSuccessStats {
                successes: 1,
                attempts: 1,
            }
        );
        assert!(matches!(
            store
                .latest_sustained_quality("target-b")
                .unwrap()
                .as_slice(),
            [(
                selector,
                NodeSustainedQuality {
                    name,
                    outcome: SustainedProbeOutcome::TransferFailed { .. },
                },
            )] if selector == "select" && name == "node-a"
        ));
        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn quick_history_derives_success_warm_median_and_p95() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).unwrap();
        bind_test_identities(&store, &["node-a"]);
        for attempts in [
            vec![
                ProbeOutcome::Reachable { delay_ms: 90 },
                ProbeOutcome::Reachable { delay_ms: 50 },
                ProbeOutcome::Reachable { delay_ms: 60 },
            ],
            vec![
                ProbeOutcome::Reachable { delay_ms: 100 },
                ProbeOutcome::Timeout,
                ProbeOutcome::Reachable { delay_ms: 70 },
            ],
        ] {
            store
                .record_reachability_assessment(
                    "select",
                    &NodeReachabilityAssessment::from_attempts("node-a".into(), attempts),
                )
                .unwrap();
        }

        let history = store.node_quick_history("select", "node-a", 10).unwrap();
        assert_eq!(history.successful_rounds, 2);
        assert_eq!(history.rounds, 2);
        assert_eq!(history.warm_median_ms, Some(60));
        assert_eq!(history.p95_ms, Some(100));
        assert_eq!(history.cold_start_ms, Some(90));
        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn assessment_and_attempts_commit_atomically() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).unwrap();
        bind_test_identities(&store, &["node-a"]);
        store.connection.execute_batch(
            "CREATE TRIGGER fail_second_attempt BEFORE INSERT ON probe_attempts WHEN NEW.attempt_index = 1 BEGIN SELECT RAISE(ABORT, 'forced failure'); END;",
        ).unwrap();
        let assessment = NodeReachabilityAssessment::from_attempts(
            "node-a".into(),
            vec![
                ProbeOutcome::Reachable { delay_ms: 40 },
                ProbeOutcome::Reachable { delay_ms: 45 },
                ProbeOutcome::Reachable { delay_ms: 50 },
            ],
        );

        assert!(
            store
                .record_reachability_assessment("select", &assessment)
                .is_err()
        );
        let parents: i64 = store
            .connection
            .query_row("SELECT count(*) FROM reachability_assessments", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(parents, 0);
        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn records_benchmark_latency_rows() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open sqlite store");
        bind_test_identities(&store, &["美国-a"]);

        store
            .record_benchmark(&BenchmarkRecord {
                selector: "select",
                node: "美国-a",
                filter: "美国,香港",
                delay_ms: Some(82),
                completed: true,
                job_kind: "auto",
            })
            .expect("record benchmark");

        let rows = store.recent_benchmarks(10).expect("read rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].selector, "select");
        assert_eq!(rows[0].node, "美国-a");
        assert_eq!(rows[0].filter, "美国,香港");
        assert_eq!(rows[0].delay_ms, Some(82));
        assert!(rows[0].completed);
        assert_eq!(rows[0].job_kind, "auto");

        remove_test_db(&path);
    }

    #[test]
    fn complete_custom_run_publishes_atomically_and_later_incomplete_preserves_it() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open sqlite store");
        bind_test_identities(&store, &["node-a", "node-b"]);
        let (first_run, generation) = store
            .begin_usability_probe_run("agy", "select", store.quality_generation())
            .expect("begin complete run")
            .expect("quality session is current");
        assert!(
            store
                .finish_usability_probe_run(
                    first_run,
                    generation,
                    true,
                    Some("first complete"),
                    None,
                    &[
                        UsabilityProbeFactRecord {
                            node: "node-a".to_string(),
                            usable: true,
                            detail: Some("accepted".to_string()),
                        },
                        UsabilityProbeFactRecord {
                            node: "node-b".to_string(),
                            usable: false,
                            detail: Some("rejected".to_string()),
                        },
                    ],
                )
                .expect("finish complete run")
        );
        let first_projection = store
            .latest_usability_probe_run(
                "agy",
                "select",
                &["node-a".to_string(), "node-b".to_string()],
            )
            .expect("read first projection")
            .expect("complete run is published");
        assert_eq!(first_projection.run_id, first_run);
        assert_eq!(first_projection.results.len(), 2);

        let (failed_run, failed_generation) = store
            .begin_usability_probe_run("agy", "select", store.quality_generation())
            .expect("begin incomplete run")
            .expect("quality session is current");
        assert!(
            !store
                .finish_usability_probe_run(
                    failed_run,
                    failed_generation,
                    false,
                    None,
                    Some("fixture authentication failed"),
                    &[UsabilityProbeFactRecord {
                        node: "node-a".to_string(),
                        usable: false,
                        detail: None,
                    }],
                )
                .expect("finish incomplete run")
        );
        let preserved = store
            .latest_usability_probe_run(
                "agy",
                "select",
                &["node-a".to_string(), "node-b".to_string()],
            )
            .expect("read preserved projection")
            .expect("prior complete run remains visible");
        assert_eq!(preserved.run_id, first_run);
        assert_eq!(preserved.summary.as_deref(), Some("first complete"));
        assert!(preserved.completed_at_ms > 0);

        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn custom_ttl_is_frozen_at_publication_and_latest_failure_is_returned_with_prior_result() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open sqlite store");
        bind_test_identities(&store, &["node-a"]);
        let (complete_run, generation) = store
            .begin_usability_probe_run("agy", "select", store.quality_generation())
            .expect("begin complete run")
            .expect("quality generation current");
        let complete_lease = store
            .usability_probe_lock_lease(complete_run)
            .expect("clone complete-run lease");
        assert!(
            store
                .finish_usability_probe_run_with_ttl(UsabilityProbeRunFinalization {
                    run_id: complete_run,
                    generation,
                    process_lease: &complete_lease,
                    complete: true,
                    summary: Some("accepted"),
                    diagnostic: None,
                    facts: &[UsabilityProbeFactRecord {
                        node: "node-a".to_string(),
                        usable: true,
                        detail: None,
                    }],
                    result_ttl: Some(Duration::from_secs(60)),
                },)
                .expect("publish expiring run")
        );
        drop(complete_lease);
        let (failed_run, generation) = store
            .begin_usability_probe_run("agy", "select", store.quality_generation())
            .expect("begin failed run")
            .expect("quality generation current");
        assert!(
            !store
                .finish_usability_probe_run(
                    failed_run,
                    generation,
                    false,
                    None,
                    Some("authentication failed"),
                    &[],
                )
                .expect("finalize failed run")
        );
        let state = store
            .latest_usability_probe_run("agy", "select", &["node-a".to_string()])
            .expect("read usability state")
            .expect("prior complete run retained");
        assert_eq!(state.run_id, complete_run);
        assert!(
            state
                .expires_at_ms
                .is_some_and(|expires| expires > state.completed_at_ms)
        );
        let failure = state
            .latest_attempt
            .expect("latest failed attempt is visible");
        assert_eq!(failure.run_id, failed_run);
        assert!(!failure.complete);
        assert_eq!(failure.diagnostic.as_deref(), Some("authentication failed"));

        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn stale_custom_run_receipt_creates_no_dangling_running_row() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open sqlite store");
        bind_test_identities(&store, &["node-a"]);

        let stale_generation = store.quality_generation().saturating_add(1);
        assert!(
            store
                .begin_usability_probe_run("criterion", "select", stale_generation)
                .expect("reject stale generation without a storage error")
                .is_none()
        );
        let run_count: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM usability_probe_runs", [], |row| {
                row.get(0)
            })
            .expect("count usability runs");
        assert_eq!(run_count, 0, "a stale lease must not create a running run");

        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn foreground_and_background_stores_cannot_overlap_the_same_custom_probe() {
        let path = test_db_path();
        let foreground = BenchmarkStore::open(&path).expect("open foreground store");
        bind_test_identities(&foreground, &["node-a"]);
        let background = BenchmarkStore::open(&path).expect("open background store");

        let (run_id, generation) = foreground
            .begin_usability_probe_run("paid-criterion", "select", foreground.quality_generation())
            .expect("begin foreground run")
            .expect("quality generation is current");
        let overlap = background
            .begin_usability_probe_run("paid-criterion", "select", background.quality_generation())
            .expect_err("a second process must not launch the same paid probe");
        assert!(format!("{overlap:#}").contains("already running"));

        foreground
            .finish_usability_probe_run(run_id, generation, false, None, Some("cancelled"), &[])
            .expect("finish foreground run");
        assert!(
            background
                .begin_usability_probe_run(
                    "paid-criterion",
                    "select",
                    background.quality_generation(),
                )
                .expect("begin after terminal state")
                .is_some(),
            "a terminal attempt must release the per-criterion launch slot"
        );

        drop(background);
        drop(foreground);
        remove_test_db(&path);
    }

    #[test]
    fn abandoned_custom_probe_lock_is_recovered_without_permanent_blocking() {
        let path = test_db_path();
        let abandoned_run = {
            let foreground = BenchmarkStore::open(&path).expect("open foreground store");
            bind_test_identities(&foreground, &["node-a"]);
            foreground
                .begin_usability_probe_run(
                    "paid-criterion",
                    "select",
                    foreground.quality_generation(),
                )
                .expect("begin abandoned run")
                .expect("quality generation is current")
                .0
        };

        let recovered = BenchmarkStore::open(&path).expect("open recovery store");
        let (replacement_run, generation) = recovered
            .begin_usability_probe_run("paid-criterion", "select", recovered.quality_generation())
            .expect("recover abandoned owner")
            .expect("quality generation remains current");
        let abandoned = recovered
            .connection
            .query_row(
                "SELECT status, diagnostic FROM usability_probe_runs WHERE id = ?1",
                [abandoned_run],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .expect("read recovered audit row");
        assert_eq!(abandoned.0, "incomplete");
        assert_eq!(abandoned.1, "probe owner exited before finalization");

        recovered
            .finish_usability_probe_run(replacement_run, generation, false, None, None, &[])
            .expect("finalize replacement run");
        drop(recovered);
        remove_test_db(&path);
    }

    #[test]
    fn active_probe_lease_outlives_a_replaced_store() {
        let path = test_db_path();
        let (active_run, generation, lease) = {
            let foreground = BenchmarkStore::open(&path).expect("open foreground store");
            bind_test_identities(&foreground, &["node-a"]);
            let (run_id, generation) = foreground
                .begin_usability_probe_run(
                    "paid-criterion",
                    "select",
                    foreground.quality_generation(),
                )
                .expect("begin foreground run")
                .expect("quality generation is current");
            let lease = foreground
                .usability_probe_lock_lease(run_id)
                .expect("clone active probe lease");
            (run_id, generation, lease)
        };

        let replacement = BenchmarkStore::open(&path).expect("open replacement store");
        let overlap = replacement
            .begin_usability_probe_run("paid-criterion", "select", replacement.quality_generation())
            .expect_err("the active job lease must survive store replacement");
        assert!(format!("{overlap:#}").contains("already running"));

        replacement
            .finish_usability_probe_run_with_ttl(UsabilityProbeRunFinalization {
                run_id: active_run,
                generation,
                process_lease: &lease,
                complete: false,
                summary: None,
                diagnostic: Some("store replaced while the probe was active"),
                facts: &[],
                result_ttl: None,
            })
            .expect("the replacement store can finalize through the active lease");
        let status: String = replacement
            .connection
            .query_row(
                "SELECT status FROM usability_probe_runs WHERE id = ?1",
                [active_run],
                |row| row.get(0),
            )
            .expect("read finalized run status");
        assert_eq!(status, "incomplete");

        drop(lease);
        let (replacement_run, generation) = replacement
            .begin_usability_probe_run("paid-criterion", "select", replacement.quality_generation())
            .expect("recover after active job releases its lease")
            .expect("quality generation remains current");
        replacement
            .finish_usability_probe_run(replacement_run, generation, false, None, None, &[])
            .expect("finalize replacement run");
        drop(replacement);
        remove_test_db(&path);
    }

    #[test]
    fn custom_projection_is_intersected_with_current_selector_members() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open sqlite store");
        bind_test_identities(&store, &["node-a", "node-b"]);
        let (run_id, generation) = store
            .begin_usability_probe_run("criterion", "select", store.quality_generation())
            .expect("begin run")
            .expect("quality session is current");
        store
            .finish_usability_probe_run(
                run_id,
                generation,
                true,
                None,
                None,
                &[
                    UsabilityProbeFactRecord {
                        node: "node-a".to_string(),
                        usable: true,
                        detail: None,
                    },
                    UsabilityProbeFactRecord {
                        node: "node-b".to_string(),
                        usable: true,
                        detail: None,
                    },
                ],
            )
            .expect("finish run");
        let projection = store
            .latest_usability_probe_run("criterion", "select", &["node-b".to_string()])
            .expect("read projection")
            .expect("run exists");
        assert_eq!(
            projection
                .results
                .iter()
                .map(|result| result.node.as_str())
                .collect::<Vec<_>>(),
            ["node-b"]
        );

        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn leased_custom_projection_validates_generation_and_preserves_member_intersection() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open sqlite store");
        bind_test_identities(&store, &["node-a", "node-b"]);
        let (run_id, generation) = store
            .begin_usability_probe_run("criterion", "select", store.quality_generation())
            .expect("begin run")
            .expect("quality session is current");
        store
            .finish_usability_probe_run(
                run_id,
                generation,
                true,
                None,
                None,
                &[
                    UsabilityProbeFactRecord {
                        node: "node-a".to_string(),
                        usable: true,
                        detail: None,
                    },
                    UsabilityProbeFactRecord {
                        node: "node-b".to_string(),
                        usable: false,
                        detail: Some("rejected".to_string()),
                    },
                ],
            )
            .expect("finish run");

        let lease = store
            .acquire_quality_read_lease()
            .expect("acquire read lease")
            .expect("quality session is current");
        let projection = store
            .latest_usability_probe_run_with_lease(
                &lease,
                "criterion",
                "select",
                &["node-b".to_string()],
            )
            .expect("read leased projection")
            .expect("run exists");
        assert_eq!(projection.run_id, run_id);
        assert_eq!(projection.results.len(), 1);
        assert_eq!(projection.results[0].node, "node-b");
        assert!(!projection.results[0].usable);

        let stale_lease = NodeQualityReadLease::for_test(lease.generation().saturating_add(1));
        assert!(
            store
                .latest_usability_probe_run_with_lease(
                    &stale_lease,
                    "criterion",
                    "select",
                    &["node-b".to_string()],
                )
                .is_err(),
            "a generation-only fixture must not authorize a production store read"
        );

        drop(lease);
        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn custom_projection_survives_reconciliation_for_each_unchanged_node() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open sqlite store");
        bind_test_identities(&store, &["node-a", "node-b"]);
        let original_generation = store.quality_generation();
        let (run_id, generation) = store
            .begin_usability_probe_run("criterion", "select", original_generation)
            .expect("begin run")
            .expect("quality session is current");
        store
            .finish_usability_probe_run(
                run_id,
                generation,
                true,
                None,
                None,
                &[
                    UsabilityProbeFactRecord {
                        node: "node-a".to_string(),
                        usable: true,
                        detail: Some("unchanged".to_string()),
                    },
                    UsabilityProbeFactRecord {
                        node: "node-b".to_string(),
                        usable: true,
                        detail: Some("will change".to_string()),
                    },
                ],
            )
            .expect("finish complete run");

        let changed = node_config(vec![
            serde_json::json!({
                "type":"selector", "tag":"select", "outbounds":["node-a", "node-b", "node-c"]
            }),
            serde_json::json!({"type":"direct", "tag":"node-a"}),
            serde_json::json!({
                "type":"socks", "tag":"node-b", "server":"changed.example", "server_port":1080
            }),
            serde_json::json!({"type":"direct", "tag":"node-c"}),
        ]);
        store
            .reconcile_node_history(&changed)
            .expect("advance generation for changed and added nodes");
        assert!(store.quality_generation() > original_generation);

        let projection = store
            .latest_usability_probe_run(
                "criterion",
                "select",
                &[
                    "node-a".to_string(),
                    "node-b".to_string(),
                    "node-c".to_string(),
                ],
            )
            .expect("read reconciled projection")
            .expect("the complete run survives per-node reconciliation");
        assert_eq!(projection.run_id, run_id);
        assert_eq!(
            projection
                .results
                .iter()
                .map(|result| result.node.as_str())
                .collect::<Vec<_>>(),
            ["node-a"]
        );
        assert_eq!(projection.results[0].detail.as_deref(), Some("unchanged"));

        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn custom_generation_drift_finishes_incomplete_without_publishing() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open sqlite store");
        bind_test_identities(&store, &["node-a"]);
        let (run_id, generation) = store
            .begin_usability_probe_run("criterion", "select", store.quality_generation())
            .expect("begin run")
            .expect("quality session is current");
        let changed = serde_json::json!({
            "outbounds": [
                {"type":"selector", "tag":"select", "outbounds":["node-a"]},
                {"type":"socks", "tag":"node-a", "server":"changed.example", "server_port":1080}
            ]
        });
        store
            .reconcile_node_history(&changed)
            .expect("advance node generation");
        assert!(
            !store
                .finish_usability_probe_run(
                    run_id,
                    generation,
                    true,
                    None,
                    None,
                    &[UsabilityProbeFactRecord {
                        node: "node-a".to_string(),
                        usable: true,
                        detail: None,
                    }],
                )
                .expect("stale run becomes incomplete")
        );
        assert!(
            store
                .latest_usability_probe_run("criterion", "select", &["node-a".to_string()])
                .expect("query current generation")
                .is_none()
        );
        let status: String = store
            .connection
            .query_row(
                "SELECT status FROM usability_probe_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .expect("read stale run status");
        assert_eq!(status, "incomplete");

        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn malformed_custom_facts_are_terminal_incomplete_not_permanently_running() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open sqlite store");
        bind_test_identities(&store, &["node-a"]);
        let (run_id, generation) = store
            .begin_usability_probe_run("criterion", "select", store.quality_generation())
            .expect("begin run")
            .expect("quality session is current");
        let duplicate = UsabilityProbeFactRecord {
            node: "node-a".to_string(),
            usable: true,
            detail: None,
        };
        assert!(
            !store
                .finish_usability_probe_run(
                    run_id,
                    generation,
                    true,
                    None,
                    None,
                    &[duplicate.clone(), duplicate],
                )
                .expect("malformed run is finalized")
        );
        let status: String = store
            .connection
            .query_row(
                "SELECT status FROM usability_probe_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .expect("read run status");
        assert_eq!(status, "incomplete");

        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn record_benchmark_waits_for_short_lived_write_lock() {
        let path = test_db_path();
        let holder = BenchmarkStore::open(&path).expect("open sqlite holder");
        bind_test_identities(&holder, &["node-a"]);
        let writer = BenchmarkStore::open(&path).expect("open sqlite writer");
        holder
            .connection
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold write lock");

        let (started_tx, started_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx.send(()).expect("signal writer started");
            writer.record_benchmark(&BenchmarkRecord {
                selector: "select",
                node: "node-a",
                filter: "node",
                delay_ms: Some(42),
                completed: true,
                job_kind: "auto",
            })
        });

        started_rx.recv().expect("writer started");
        thread::sleep(Duration::from_millis(100));
        holder
            .connection
            .execute_batch("COMMIT")
            .expect("release write lock");
        worker
            .join()
            .expect("writer thread")
            .expect("record benchmark");

        let rows = holder.recent_benchmarks(10).expect("read rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].node, "node-a");
        assert_eq!(rows[0].delay_ms, Some(42));

        remove_test_db(&path);
    }

    #[test]
    fn benchmark_history_uses_recent_index_and_prunes_in_bounded_batches() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open sqlite store");
        for recorded_at_ms in [100_i64, 200, 600] {
            store
                .connection
                .execute(
                    r#"
                    INSERT INTO benchmark_results (
                        recorded_at_ms, selector, node, filter, delay_ms, completed, job_kind
                    ) VALUES (?1, 'select', 'node-a', 'node', 42, 1, 'auto')
                    "#,
                    [recorded_at_ms],
                )
                .expect("insert benchmark fixture");
        }

        let deleted = store
            .prune_benchmarks_before(500, 1)
            .expect("prune one expired row");
        assert_eq!(deleted, 1);
        let expired: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM benchmark_results WHERE recorded_at_ms < 500",
                [],
                |row| row.get(0),
            )
            .expect("count expired rows");
        assert_eq!(expired, 1, "the prune batch must stay bounded");

        let mut statement = store
            .connection
            .prepare("PRAGMA index_list('benchmark_results')")
            .expect("prepare index list");
        let indexes = statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query index list")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("read index names");
        assert!(
            indexes
                .iter()
                .any(|name| name == "idx_benchmark_results_selector_node_recent")
        );

        drop(statement);
        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn reads_node_latency_history_in_time_order() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open sqlite store");
        bind_test_identities(&store, &["美国-a", "美国-b"]);

        for (node, delay_ms) in [
            ("美国-a", Some(100)),
            ("美国-b", Some(50)),
            ("美国-a", None),
            ("美国-a", Some(80)),
        ] {
            store
                .record_benchmark(&BenchmarkRecord {
                    selector: "select",
                    node,
                    filter: "美国",
                    delay_ms,
                    completed: true,
                    job_kind: "auto",
                })
                .expect("record benchmark");
        }

        let points = store
            .node_latency_history("select", "美国-a", 10)
            .expect("read latency history");
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].delay_ms, Some(100));
        assert_eq!(points[1].delay_ms, None);
        assert_eq!(points[2].delay_ms, Some(80));
        assert!(points[0].recorded_at_ms <= points[1].recorded_at_ms);

        remove_test_db(&path);
    }
}
