use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

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
}

impl BenchmarkStore {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = Connection::open(path.as_ref()).with_context(|| {
            format!("failed to open SQLite database {}", path.as_ref().display())
        })?;
        let store = Self { connection };
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
                CREATE INDEX IF NOT EXISTS idx_benchmark_results_selector_node
                    ON benchmark_results(selector, node);
                "#,
            )
            .context("failed to initialize benchmark_results SQLite schema")?;
        Ok(())
    }

    pub(crate) fn record_benchmark(&self, record: &BenchmarkRecord<'_>) -> Result<()> {
        let recorded_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system time is before UNIX epoch")?
            .as_millis() as i64;
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
        Ok(())
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
                        AND completed != 0
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

#[cfg(test)]
mod tests {
    use super::{BenchmarkRecord, BenchmarkStore};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_db_path() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("sing-box-tui-test-{nanos}.sqlite3"))
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

        let _ = std::fs::remove_file(path);
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

        let _ = std::fs::remove_file(path);
    }
}
