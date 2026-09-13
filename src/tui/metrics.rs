use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Maximum interval between samples (in ms) before considering it an observation gap.
/// If time between two samples exceeds this, do not draw a continuous line between them.
pub(crate) const OBSERVATION_GAP_THRESHOLD_MS: i64 = 6_000;

/// Retention window for metric history (30 minutes in ms).
pub(crate) const METRIC_RETENTION_WINDOW_MS: i64 = 30 * 60 * 1_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrafficSample {
    pub(crate) recorded_at_ms: i64,
    pub(crate) down_bytes_per_sec: u64,
    pub(crate) up_bytes_per_sec: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LatencySample {
    pub(crate) recorded_at_ms: i64,
    pub(crate) selector: String,
    pub(crate) node_name: String,
    pub(crate) latency_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RouteInterval {
    pub(crate) id: i64,
    pub(crate) selector: String,
    pub(crate) node_name: String,
    pub(crate) started_at_ms: i64,
    pub(crate) ended_at_ms: Option<i64>,
    pub(crate) interval_index: usize,
}

pub(crate) struct MetricStore {
    conn: Connection,
    _db_path: PathBuf,
    // In-memory caches for fast frame rendering
    traffic_cache: Vec<TrafficSample>,
    latency_cache: Vec<LatencySample>,
    route_intervals: Vec<RouteInterval>,
    last_prune_ms: i64,
}

impl MetricStore {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db_path = path.as_ref().to_path_buf();
        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open metric database at {}", db_path.display()))?;

        // Enable WAL mode and busy timeout for concurrent safety
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("failed to set WAL mode for metric store")?;
        conn.busy_timeout(Duration::from_secs(5))
            .context("failed to set busy timeout for metric store")?;

        let mut store = Self {
            conn,
            _db_path: db_path,
            traffic_cache: Vec::new(),
            latency_cache: Vec::new(),
            route_intervals: Vec::new(),
            last_prune_ms: 0,
        };

        store.init_schema()?;
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("failed to open in-memory metric store")?;
        let mut store = Self {
            conn,
            _db_path: PathBuf::from(":memory:"),
            traffic_cache: Vec::new(),
            latency_cache: Vec::new(),
            route_intervals: Vec::new(),
            last_prune_ms: 0,
        };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&mut self) -> Result<()> {
        self.conn
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS route_intervals (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    selector TEXT NOT NULL,
                    node_name TEXT NOT NULL,
                    started_at_ms INTEGER NOT NULL,
                    ended_at_ms INTEGER,
                    interval_index INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX IF NOT EXISTS idx_route_intervals_time ON route_intervals(started_at_ms);

                CREATE TABLE IF NOT EXISTS traffic_samples (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    recorded_at_ms INTEGER NOT NULL,
                    down_bytes_per_sec INTEGER NOT NULL,
                    up_bytes_per_sec INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_traffic_samples_time ON traffic_samples(recorded_at_ms);

                CREATE TABLE IF NOT EXISTS latency_samples (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    recorded_at_ms INTEGER NOT NULL,
                    selector TEXT NOT NULL,
                    node_name TEXT NOT NULL,
                    latency_ms INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_latency_samples_time ON latency_samples(recorded_at_ms);
                "#,
            )
            .context("failed to initialize metric SQLite schema")?;
        Ok(())
    }

    /// Load the last 30 minutes from SQLite into memory cache on startup.
    pub(crate) fn load_recent_history(&mut self, now_ms: i64) -> Result<()> {
        let cutoff_ms = now_ms.saturating_sub(METRIC_RETENTION_WINDOW_MS);

        // Load traffic samples
        let mut traffic_stmt = self.conn.prepare(
            "SELECT recorded_at_ms, down_bytes_per_sec, up_bytes_per_sec
             FROM traffic_samples
             WHERE recorded_at_ms >= ?1
             ORDER BY recorded_at_ms ASC",
        )?;
        let traffic_rows = traffic_stmt.query_map(params![cutoff_ms], |row| {
            Ok(TrafficSample {
                recorded_at_ms: row.get(0)?,
                down_bytes_per_sec: row.get::<_, i64>(1)? as u64,
                up_bytes_per_sec: row.get::<_, i64>(2)? as u64,
            })
        })?;
        self.traffic_cache.clear();
        for sample in traffic_rows {
            self.traffic_cache.push(sample?);
        }

        // Load latency samples
        let mut latency_stmt = self.conn.prepare(
            "SELECT recorded_at_ms, selector, node_name, latency_ms
             FROM latency_samples
             WHERE recorded_at_ms >= ?1
             ORDER BY recorded_at_ms ASC",
        )?;
        let latency_rows = latency_stmt.query_map(params![cutoff_ms], |row| {
            Ok(LatencySample {
                recorded_at_ms: row.get(0)?,
                selector: row.get(1)?,
                node_name: row.get(2)?,
                latency_ms: row.get::<_, i64>(3)? as u64,
            })
        })?;
        self.latency_cache.clear();
        for sample in latency_rows {
            self.latency_cache.push(sample?);
        }

        // Load route intervals (overlapping the 30-minute window)
        let mut route_stmt = self.conn.prepare(
            "SELECT id, selector, node_name, started_at_ms, ended_at_ms, interval_index
             FROM route_intervals
             WHERE ended_at_ms IS NULL OR ended_at_ms >= ?1
             ORDER BY started_at_ms ASC",
        )?;
        let route_rows = route_stmt.query_map(params![cutoff_ms], |row| {
            Ok(RouteInterval {
                id: row.get(0)?,
                selector: row.get(1)?,
                node_name: row.get(2)?,
                started_at_ms: row.get(3)?,
                ended_at_ms: row.get(4)?,
                interval_index: row.get::<_, i64>(5)? as usize,
            })
        })?;
        self.route_intervals.clear();
        for interval in route_rows {
            self.route_intervals.push(interval?);
        }

        Ok(())
    }

    /// Record a core traffic sample and append to in-memory cache.
    pub(crate) fn record_traffic(
        &mut self,
        now_ms: i64,
        down_bps: u64,
        up_bps: u64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO traffic_samples (recorded_at_ms, down_bytes_per_sec, up_bytes_per_sec)
             VALUES (?1, ?2, ?3)",
            params![now_ms, down_bps as i64, up_bps as i64],
        )?;

        let sample = TrafficSample {
            recorded_at_ms: now_ms,
            down_bytes_per_sec: down_bps,
            up_bytes_per_sec: up_bps,
        };
        self.traffic_cache.push(sample);
        self.maybe_prune(now_ms)?;
        Ok(())
    }

    /// Record a route latency measurement and append to in-memory cache.
    pub(crate) fn record_latency(
        &mut self,
        now_ms: i64,
        selector: &str,
        node_name: &str,
        latency_ms: u64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO latency_samples (recorded_at_ms, selector, node_name, latency_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![now_ms, selector, node_name, latency_ms as i64],
        )?;

        let sample = LatencySample {
            recorded_at_ms: now_ms,
            selector: selector.to_string(),
            node_name: node_name.to_string(),
            latency_ms,
        };
        self.latency_cache.push(sample);
        self.maybe_prune(now_ms)?;
        Ok(())
    }

    /// Notify that the active route/node changed.
    /// Closes the previous interval (if any) and starts a new one with incremented index.
    pub(crate) fn record_route_switch(
        &mut self,
        now_ms: i64,
        selector: &str,
        node_name: &str,
    ) -> Result<()> {
        // If current active interval is identical, do nothing
        if let Some(last) = self.route_intervals.last() {
            if last.ended_at_ms.is_none() && last.selector == selector && last.node_name == node_name {
                return Ok(());
            }
        }

        let next_index = if let Some(last) = self.route_intervals.last() {
            last.interval_index + 1
        } else {
            let highest: Option<i64> = self.conn.query_row(
                "SELECT interval_index FROM route_intervals ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            ).optional()?;
            highest.map_or(0, |idx| (idx + 1) as usize)
        };

        // Close any currently open intervals
        self.conn.execute(
            "UPDATE route_intervals SET ended_at_ms = ?1 WHERE ended_at_ms IS NULL",
            params![now_ms],
        )?;
        for interval in &mut self.route_intervals {
            if interval.ended_at_ms.is_none() {
                interval.ended_at_ms = Some(now_ms);
            }
        }

        // Insert new interval
        self.conn.execute(
            "INSERT INTO route_intervals (selector, node_name, started_at_ms, ended_at_ms, interval_index)
             VALUES (?1, ?2, ?3, NULL, ?4)",
            params![selector, node_name, now_ms, next_index as i64],
        )?;
        let id = self.conn.last_insert_rowid();

        self.route_intervals.push(RouteInterval {
            id,
            selector: selector.to_string(),
            node_name: node_name.to_string(),
            started_at_ms: now_ms,
            ended_at_ms: None,
            interval_index: next_index,
        });

        self.maybe_prune(now_ms)?;
        Ok(())
    }

    /// Prune entries older than 30 minutes from SQLite and in-memory caches.
    /// Runs at most once every 60 seconds.
    fn maybe_prune(&mut self, now_ms: i64) -> Result<()> {
        if now_ms.saturating_sub(self.last_prune_ms) < 60_000 {
            return Ok(());
        }
        self.last_prune_ms = now_ms;
        let cutoff_ms = now_ms.saturating_sub(METRIC_RETENTION_WINDOW_MS);

        // Prune SQLite
        self.conn.execute(
            "DELETE FROM traffic_samples WHERE recorded_at_ms < ?1",
            params![cutoff_ms],
        )?;
        self.conn.execute(
            "DELETE FROM latency_samples WHERE recorded_at_ms < ?1",
            params![cutoff_ms],
        )?;
        self.conn.execute(
            "DELETE FROM route_intervals WHERE ended_at_ms IS NOT NULL AND ended_at_ms < ?1",
            params![cutoff_ms],
        )?;

        // Prune in-memory cache
        self.traffic_cache.retain(|s| s.recorded_at_ms >= cutoff_ms);
        self.latency_cache.retain(|s| s.recorded_at_ms >= cutoff_ms);
        self.route_intervals
            .retain(|i| i.ended_at_ms.is_none() || i.ended_at_ms.unwrap_or(0) >= cutoff_ms);

        Ok(())
    }

    pub(crate) fn traffic_samples(&self) -> &[TrafficSample] {
        &self.traffic_cache
    }

    pub(crate) fn latency_samples(&self) -> &[LatencySample] {
        &self.latency_cache
    }

    pub(crate) fn route_intervals(&self) -> &[RouteInterval] {
        &self.route_intervals
    }

    /// Get current active route interval index for color selection (0: Cyan, 1: Yellow, etc.)
    #[allow(dead_code)]
    pub(crate) fn current_interval_index(&self) -> usize {
        self.route_intervals
            .last()
            .map_or(0, |interval| interval.interval_index)
    }

    /// Checks whether two consecutive sample timestamps have a missing observation gap.
    pub(crate) fn has_gap(prev_ms: i64, next_ms: i64) -> bool {
        (next_ms - prev_ms).abs() > OBSERVATION_GAP_THRESHOLD_MS
    }
}

pub(crate) fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metric_store_traffic_and_persistence() {
        let mut store = MetricStore::open_in_memory().expect("open in-memory");
        let now = 1_000_000;

        store.record_traffic(now, 1024, 512).unwrap();
        store.record_traffic(now + 1000, 2048, 1024).unwrap();
        assert_eq!(store.traffic_samples().len(), 2);
        assert_eq!(store.traffic_samples()[0].down_bytes_per_sec, 1024);
        assert_eq!(store.traffic_samples()[1].down_bytes_per_sec, 2048);

        // Gap detection
        assert!(!MetricStore::has_gap(now, now + 1000));
        assert!(MetricStore::has_gap(now, now + 10_000));
    }

    #[test]
    fn test_metric_store_route_switching_and_color_index() {
        let mut store = MetricStore::open_in_memory().expect("open in-memory");
        let now = 1_000_000;

        store.record_route_switch(now, "Proxy", "Node-A").unwrap();
        assert_eq!(store.current_interval_index(), 0);

        // Idempotent switch to same node does not advance index
        store.record_route_switch(now + 1000, "Proxy", "Node-A").unwrap();
        assert_eq!(store.current_interval_index(), 0);
        assert_eq!(store.route_intervals().len(), 1);

        // Switch to Node-B advances index to 1 (Yellow)
        store.record_route_switch(now + 2000, "Proxy", "Node-B").unwrap();
        assert_eq!(store.current_interval_index(), 1);
        assert_eq!(store.route_intervals().len(), 2);
        assert_eq!(store.route_intervals()[0].ended_at_ms, Some(now + 2000));
        assert_eq!(store.route_intervals()[1].ended_at_ms, None);

        // Switch to Node-C advances index to 2 (Cyan)
        store.record_route_switch(now + 5000, "Proxy", "Node-C").unwrap();
        assert_eq!(store.current_interval_index(), 2);
    }

    #[test]
    fn test_metric_store_latency_and_prune() {
        let mut store = MetricStore::open_in_memory().expect("open in-memory");
        let base = 2_000_000;

        store.record_latency(base, "Proxy", "Node-A", 45).unwrap();
        store.record_latency(base + 1000, "Proxy", "Node-A", 48).unwrap();
        assert_eq!(store.latency_samples().len(), 2);

        // Trigger prune with time moving forward past retention window (30 mins = 1_800_000 ms)
        let future = base + METRIC_RETENTION_WINDOW_MS + 100_000;
        store.record_latency(future, "Proxy", "Node-A", 50).unwrap();
        // Since last_prune_ms was 0, it pruned records older than future - 1_800_000
        assert_eq!(store.latency_samples().len(), 1);
        assert_eq!(store.latency_samples()[0].latency_ms, 50);
    }
}
