use std::cell::Cell;
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::controller::{NodeReachabilityAssessment, ProbeOutcome, derive_reachability_assessment};

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const BENCHMARK_RETENTION: Duration = Duration::from_secs(48 * 60 * 60);
const BENCHMARK_PRUNE_INTERVAL: Duration = Duration::from_secs(10 * 60);
const BENCHMARK_PRUNE_BATCH_SIZE: usize = 50_000;
const NODE_QUALITY_SCHEMA_VERSION: i64 = 2;

pub(crate) fn default_benchmark_db_path() -> PathBuf {
    env::var("SING_BOX_TUI_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("singbox.sqlite3"))
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
    last_prune_at_ms: Cell<i64>,
}

impl BenchmarkStore {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        reset_legacy_database(path.as_ref())?;
        let connection = Connection::open(path.as_ref()).with_context(|| {
            format!("failed to open SQLite database {}", path.as_ref().display())
        })?;
        configure_benchmark_connection(&connection, path.as_ref())?;
        let store = Self {
            connection,
            last_prune_at_ms: Cell::new(current_timestamp_ms()?),
        };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<()> {
        self.connection
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
                PRAGMA user_version = 2;
                "#,
            )
            .context("failed to initialize benchmark_results SQLite schema")?;
        Ok(())
    }

    pub(crate) fn record_reachability_assessment(
        &self,
        selector: &str,
        assessment: &NodeReachabilityAssessment,
    ) -> Result<()> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .context("failed to begin reachability assessment transaction")?;
        transaction.execute(
            "INSERT INTO reachability_assessments (recorded_at_ms, selector, node, complete) VALUES (?1, ?2, ?3, ?4)",
            params![current_timestamp_ms()?, selector, assessment.name, i64::from(assessment.assessment.is_some())],
        ).context("failed to insert reachability assessment")?;
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
        Ok(())
    }

    pub(crate) fn latest_reachability_assessments(
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

    pub(crate) fn record_benchmark(&self, record: &BenchmarkRecord<'_>) -> Result<()> {
        let recorded_at_ms = current_timestamp_ms()?;
        let delay_ms = record.delay_ms.map(|value| value as i64);
        self.connection
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
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
                params![
                    recorded_at_ms,
                    record.selector,
                    record.node,
                    record.filter,
                    delay_ms,
                    if record.completed { 1_i64 } else { 0_i64 },
                    record.job_kind,
                ],
            )
            .context("failed to insert benchmark result into SQLite")?;
        if let Err(error) = self.maybe_prune_benchmark_history(recorded_at_ms) {
            eprintln!("warning: failed to prune old benchmark history: {error:#}");
        }
        Ok(())
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
}

fn current_timestamp_ms() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time is before UNIX epoch")?
        .as_millis() as i64)
}

fn reset_legacy_database(path: &Path) -> Result<()> {
    if !path.exists() || path == Path::new(":memory:") {
        return Ok(());
    }
    let legacy = Connection::open(path)
        .and_then(|connection| {
            connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        })
        .unwrap_or_default()
        != NODE_QUALITY_SCHEMA_VERSION;
    if !legacy {
        return Ok(());
    }
    std::fs::remove_file(path)
        .with_context(|| format!("failed to delete legacy SQLite database {}", path.display()))?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        if sidecar.exists() {
            std::fs::remove_file(&sidecar).with_context(|| {
                format!(
                    "failed to delete legacy SQLite sidecar {}",
                    sidecar.display()
                )
            })?;
        }
    }
    Ok(())
}

fn configure_benchmark_connection(connection: &Connection, path: &Path) -> Result<()> {
    connection
        .busy_timeout(SQLITE_BUSY_TIMEOUT)
        .with_context(|| format!("failed to set SQLite busy timeout for {}", path.display()))?;
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
    use super::{BenchmarkRecord, BenchmarkStore};
    use crate::controller::{NodeReachabilityAssessment, ProbeOutcome, ReachabilityAssessment};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
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
    }

    #[test]
    fn first_node_quality_open_recreates_legacy_database() {
        let path = test_db_path();
        let legacy = rusqlite::Connection::open(&path).expect("open legacy database");
        legacy
            .execute_batch("CREATE TABLE legacy_latency (delay INTEGER); INSERT INTO legacy_latency VALUES (42);")
            .expect("create legacy schema");
        drop(legacy);

        let store = BenchmarkStore::open(&path).expect("open node-quality store");
        let legacy_exists: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'legacy_latency'",
                [],
                |row| row.get(0),
            )
            .expect("inspect recreated schema");
        let version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();

        assert_eq!(legacy_exists, 0);
        assert_eq!(version, 2);
        drop(store);
        remove_test_db(&path);
    }

    #[test]
    fn reachability_assessment_round_trips_factual_attempts() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open node-quality store");
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
