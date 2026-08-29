use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
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
};

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const BENCHMARK_RETENTION: Duration = Duration::from_secs(48 * 60 * 60);
const BENCHMARK_PRUNE_INTERVAL: Duration = Duration::from_secs(10 * 60);
const BENCHMARK_PRUNE_BATCH_SIZE: usize = 50_000;
const NODE_QUALITY_SCHEMA_VERSION: i64 = 3;

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
}

pub(crate) struct NodeHistoryReconciliationTransaction<'store> {
    store: &'store BenchmarkStore,
    transaction: Option<Transaction<'store>>,
}

pub(crate) struct NodeQualityReconciliationLock {
    _file: Option<File>,
    database_path: PathBuf,
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
        reset_legacy_database(&database_path)?;
        let connection = Connection::open(&database_path).with_context(|| {
            format!("failed to open SQLite database {}", database_path.display())
        })?;
        configure_benchmark_connection(&connection, &database_path)?;
        let store = Self {
            connection,
            database_path,
            last_prune_at_ms: Cell::new(current_timestamp_ms()?),
            quality_generation: Cell::new(0),
        };
        store.initialize()?;
        let generation = store.read_quality_generation_unlocked()?;
        store.quality_generation.set(generation as i64);
        Ok(store)
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
        if self.quality_reads_blocked()? {
            return Ok(Vec::new());
        }
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
    /// a pre-v3 database are discarded by the accepted whole-database reset policy. New fact
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

    #[cfg(test)]
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
        if self.quality_reads_blocked()? {
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

fn reset_legacy_database(path: &Path) -> Result<()> {
    if path == Path::new(":memory:") {
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            ensure_no_orphaned_legacy_sidecars(path)?;
            return Ok(());
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
        return Ok(());
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
                return Ok(());
            }
            anyhow::bail!(
                "refusing to modify unrecognized version {version} database {}",
                path.display()
            );
        }
        legacy_version if legacy_version < NODE_QUALITY_SCHEMA_VERSION => {}
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
    Ok(())
}

fn legacy_reset_sidecar_paths(path: &Path) -> Vec<PathBuf> {
    ["-wal", "-shm", "-journal", QUALITY_WRITE_BLOCK_SUFFIX]
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
        ]
        || !published_core_table_schemas_are_recognized(connection, true)?
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

fn published_core_table_schemas_are_recognized(
    connection: &Connection,
    require_complete: bool,
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
    let indexes_recognized = indexes
        == [
            "idx_benchmark_results_recorded_at",
            "idx_benchmark_results_selector_node_recent",
            "idx_reachability_assessments_selector_node_recent",
        ]
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
        && published_core_sql_is_recognized(connection, require_complete)?;
    Ok(benchmark
        && reachability
        && probes
        && indexes_recognized
        && user_behavior_objects(connection)?.is_empty()
        && probe_foreign_key_is_recognized(connection)?)
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

fn published_core_sql_is_recognized(
    connection: &Connection,
    require_complete: bool,
) -> Result<bool> {
    let reachability_sql = if require_complete {
        vec![REACHABILITY_V2B_TABLE_SQL]
    } else {
        vec![REACHABILITY_V2A_TABLE_SQL, REACHABILITY_V2B_TABLE_SQL]
    };
    Ok(object_sql_matches(
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
    )?)
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
        BenchmarkRecord, BenchmarkStore, NODE_QUALITY_SCHEMA_VERSION,
        QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX, QUALITY_WRITE_BLOCK_SUFFIX,
        current_schema_is_recognized, node_configuration_fingerprint, sync_parent_directory,
    };
    use crate::controller::{NodeReachabilityAssessment, ProbeOutcome, ReachabilityAssessment};
    use crate::node_quality_path::QUALITY_RECONCILIATION_LOCK_SUFFIX;
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
    fn empty_file_and_empty_sqlite_header_recover_to_atomic_v3_schema() {
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
    fn both_published_v2_shapes_are_rebuilt_as_v3_without_old_facts() {
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
                "published v2 facts are discarded before old processes can target the v3 inode"
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
                    .expect("verify v3 identity table")
            );
            drop(store);
            remove_test_db(&path);
        }
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
        let path = test_db_path().with_extension("v3-trigger.sqlite3");
        drop(BenchmarkStore::open(&path).expect("create recognized v3 database"));
        let connection = rusqlite::Connection::open(&path).expect("open v3 fixture");
        connection
            .execute_batch(
                "CREATE TRIGGER unrelated_v3_trigger AFTER INSERT ON benchmark_results \
                 BEGIN SELECT 1; END;",
            )
            .expect("add unknown v3 trigger");
        drop(connection);
        let before = std::fs::read(&path).expect("read v3 candidate");
        let error = BenchmarkStore::open(&path)
            .err()
            .expect("behavior-changing v3 trigger must be rejected");
        assert!(format!("{error:#}").contains("unrecognized version 3"));
        assert_eq!(
            std::fs::read(&path).expect("read preserved v3 candidate"),
            before
        );
        remove_test_db(&path);
    }

    #[test]
    fn every_legacy_sqlite_schema_is_rebuilt_even_with_unrelated_objects() {
        for version in [-1, 0, 1, 2] {
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
                "spoofed-current",
                "CREATE TABLE unrelated (value TEXT); PRAGMA user_version = 3;",
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
