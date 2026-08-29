use std::{
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::domain::FleetSnapshot;

const DATABASE_FILE: &str = "telemetry.sqlite3";
const QUEUE_CAPACITY: usize = 2_048;
const DETAIL_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const HOURLY_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
const LIFECYCLE_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
const HOST_SAMPLE_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const HOUR_MS: i64 = 60 * 60 * 1_000;
const DAY_MS: i64 = 24 * HOUR_MS;

pub type SharedTelemetry = Arc<TelemetryStore>;

#[derive(Clone, Debug)]
pub struct RequestTelemetry {
    pub completed_at_ms: i64,
    pub host_id: String,
    pub model_id: String,
    pub client: String,
    pub outcome: String,
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub prompt_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tokens_per_second: Option<f32>,
}

#[derive(Clone, Debug)]
pub struct LifecycleTelemetry {
    pub occurred_at_ms: i64,
    pub host_id: String,
    pub model_id: Option<String>,
    pub action: String,
    pub outcome: String,
    pub duration_ms: u64,
    pub forced: bool,
}

#[derive(Clone, Debug)]
struct HostSample {
    sampled_at_ms: i64,
    host_id: String,
    online: bool,
    loaded_model_id: Option<String>,
    active_requests: u32,
    memory_used_bytes: Option<u64>,
    memory_total_bytes: Option<u64>,
    tokens_per_second: Option<f32>,
}

enum TelemetryWrite {
    Request(RequestTelemetry),
    Lifecycle(LifecycleTelemetry),
    Hosts(Vec<HostSample>),
    Flush(mpsc::SyncSender<()>),
}

pub struct TelemetryStore {
    path: PathBuf,
    sender: mpsc::SyncSender<TelemetryWrite>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TelemetrySummary {
    pub range_hours: u32,
    pub generated_at_ms: i64,
    pub request_count: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub average_tokens_per_second: Option<f64>,
    pub average_ttft_ms: Option<f64>,
    pub models: Vec<ModelTelemetrySummary>,
    pub recent_lifecycle: Vec<LifecycleEventSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelTelemetrySummary {
    pub host_id: String,
    pub model_id: String,
    pub request_count: u64,
    pub output_tokens: u64,
    pub average_tokens_per_second: Option<f64>,
    pub average_ttft_ms: Option<f64>,
    pub failed_requests: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct LifecycleEventSummary {
    pub occurred_at_ms: i64,
    pub host_id: String,
    pub model_id: Option<String>,
    pub action: String,
    pub outcome: String,
    pub duration_ms: u64,
    pub forced: bool,
}

impl TelemetryStore {
    pub fn new(config_dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(config_dir).map_err(|error| {
            format!(
                "failed to create telemetry directory {}: {error}",
                config_dir.display()
            )
        })?;
        let path = config_dir.join(DATABASE_FILE);
        let connection = open_database(&path)?;
        initialize_schema(&connection)?;
        drop(connection);

        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let writer_path = path.clone();
        thread::Builder::new()
            .name("agent-relay-telemetry".into())
            .spawn(move || writer_loop(&writer_path, receiver))
            .map_err(|error| format!("failed to start telemetry writer: {error}"))?;
        Ok(Self { path, sender })
    }

    pub fn record_request(&self, event: RequestTelemetry) {
        let _ = self.sender.try_send(TelemetryWrite::Request(event));
    }

    pub fn record_lifecycle(&self, event: LifecycleTelemetry) {
        let _ = self.sender.try_send(TelemetryWrite::Lifecycle(event));
    }

    pub fn record_host_snapshot(&self, snapshot: &FleetSnapshot) {
        let sampled_at_ms = now_ms();
        let samples = snapshot
            .hosts
            .iter()
            .map(|host| HostSample {
                sampled_at_ms,
                host_id: host.id.clone(),
                online: host.connection != crate::domain::ConnectionState::Offline,
                loaded_model_id: host.loaded_model_id.clone(),
                active_requests: host.active_requests,
                memory_used_bytes: host.memory_used_bytes,
                memory_total_bytes: host.memory_total_bytes,
                tokens_per_second: host.tokens_per_second,
            })
            .collect();
        let _ = self.sender.try_send(TelemetryWrite::Hosts(samples));
    }

    pub fn summary(&self, range_hours: u32) -> Result<TelemetrySummary, String> {
        if !(1..=24 * 7).contains(&range_hours) {
            return Err("telemetry range must be between 1 and 168 hours".into());
        }
        self.flush();
        let connection = open_database(&self.path)?;
        let since = now_ms().saturating_sub(i64::from(range_hours) * HOUR_MS);
        let totals = connection
            .query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN outcome != 'success' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(prompt_tokens), 0),
                        COALESCE(SUM(output_tokens), 0),
                        AVG(tokens_per_second), AVG(ttft_ms)
                 FROM request_events WHERE completed_at_ms >= ?1",
                [since],
                |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, u64>(3)?,
                        row.get::<_, u64>(4)?,
                        row.get::<_, Option<f64>>(5)?,
                        row.get::<_, Option<f64>>(6)?,
                    ))
                },
            )
            .map_err(database_error)?;

        let mut model_query = connection
            .prepare(
                "SELECT host_id, model_id, COUNT(*), COALESCE(SUM(output_tokens), 0),
                        AVG(tokens_per_second), AVG(ttft_ms),
                        COALESCE(SUM(CASE WHEN outcome != 'success' THEN 1 ELSE 0 END), 0)
                 FROM request_events WHERE completed_at_ms >= ?1
                 GROUP BY host_id, model_id
                 ORDER BY COUNT(*) DESC, host_id, model_id LIMIT 12",
            )
            .map_err(database_error)?;
        let models = model_query
            .query_map([since], |row| {
                Ok(ModelTelemetrySummary {
                    host_id: row.get(0)?,
                    model_id: row.get(1)?,
                    request_count: row.get(2)?,
                    output_tokens: row.get(3)?,
                    average_tokens_per_second: row.get(4)?,
                    average_ttft_ms: row.get(5)?,
                    failed_requests: row.get(6)?,
                })
            })
            .map_err(database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;

        let mut lifecycle_query = connection
            .prepare(
                "SELECT occurred_at_ms, host_id, model_id, action, outcome, duration_ms, forced
                 FROM lifecycle_events ORDER BY occurred_at_ms DESC LIMIT 8",
            )
            .map_err(database_error)?;
        let recent_lifecycle = lifecycle_query
            .query_map([], |row| {
                Ok(LifecycleEventSummary {
                    occurred_at_ms: row.get(0)?,
                    host_id: row.get(1)?,
                    model_id: row.get(2)?,
                    action: row.get(3)?,
                    outcome: row.get(4)?,
                    duration_ms: row.get(5)?,
                    forced: row.get(6)?,
                })
            })
            .map_err(database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;

        Ok(TelemetrySummary {
            range_hours,
            generated_at_ms: now_ms(),
            request_count: totals.0,
            successful_requests: totals.1,
            failed_requests: totals.2,
            prompt_tokens: totals.3,
            output_tokens: totals.4,
            average_tokens_per_second: totals.5,
            average_ttft_ms: totals.6,
            models,
            recent_lifecycle,
        })
    }

    pub fn prometheus(&self, snapshot: &FleetSnapshot) -> Result<String, String> {
        self.flush();
        let connection = open_database(&self.path)?;
        let (request_count, output_tokens, failed_requests) = connection
            .query_row(
                "SELECT COALESCE(SUM(request_count), 0),
                        COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(failed_requests), 0)
                 FROM (
                   SELECT COUNT(*) AS request_count, COALESCE(SUM(output_tokens), 0) AS output_tokens,
                          COALESCE(SUM(CASE WHEN outcome != 'success' THEN 1 ELSE 0 END), 0) AS failed_requests
                     FROM request_events
                   UNION ALL
                   SELECT COALESCE(SUM(request_count), 0), COALESCE(SUM(output_tokens), 0),
                          COALESCE(SUM(CASE WHEN outcome != 'success' THEN request_count ELSE 0 END), 0)
                     FROM request_rollups_hourly
                   UNION ALL
                   SELECT COALESCE(SUM(request_count), 0), COALESCE(SUM(output_tokens), 0),
                          COALESCE(SUM(CASE WHEN outcome != 'success' THEN request_count ELSE 0 END), 0)
                     FROM request_rollups_daily
                 )",
                [],
                |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, u64>(2)?,
                    ))
                },
            )
            .map_err(database_error)?;
        let mut output = String::from(
            "# HELP agent_relay_requests_total Generation requests observed by this Agent Relay node.\n\
# TYPE agent_relay_requests_total counter\n",
        );
        output.push_str(&format!("agent_relay_requests_total {request_count}\n"));
        output.push_str(
            "# HELP agent_relay_output_tokens_total Generated tokens observed by this Agent Relay node.\n\
# TYPE agent_relay_output_tokens_total counter\n",
        );
        output.push_str(&format!(
            "agent_relay_output_tokens_total {output_tokens}\n"
        ));
        output.push_str(
            "# HELP agent_relay_failed_requests_total Failed or cancelled generation requests.\n\
# TYPE agent_relay_failed_requests_total counter\n",
        );
        output.push_str(&format!(
            "agent_relay_failed_requests_total {failed_requests}\n"
        ));
        output.push_str(
            "# HELP agent_relay_peer_up Whether an Agent Relay host is currently reachable.\n\
# TYPE agent_relay_peer_up gauge\n",
        );
        for host in &snapshot.hosts {
            let up = u8::from(host.connection != crate::domain::ConnectionState::Offline);
            output.push_str(&format!(
                "agent_relay_peer_up{{host=\"{}\"}} {}\n",
                prometheus_escape(&host.id),
                up
            ));
        }
        output.push_str(
            "# HELP agent_relay_active_requests Current active generation requests.\n\
# TYPE agent_relay_active_requests gauge\n",
        );
        for host in &snapshot.hosts {
            output.push_str(&format!(
                "agent_relay_active_requests{{host=\"{}\"}} {}\n",
                prometheus_escape(&host.id),
                host.active_requests
            ));
        }
        output.push_str(
            "# HELP agent_relay_memory_used_bytes Current reported memory use.\n\
# TYPE agent_relay_memory_used_bytes gauge\n",
        );
        for host in &snapshot.hosts {
            if let Some(bytes) = host.memory_used_bytes {
                output.push_str(&format!(
                    "agent_relay_memory_used_bytes{{host=\"{}\"}} {}\n",
                    prometheus_escape(&host.id),
                    bytes
                ));
            }
        }
        output.push_str(
            "# HELP agent_relay_memory_total_bytes Total reported memory capacity.\n\
# TYPE agent_relay_memory_total_bytes gauge\n",
        );
        for host in &snapshot.hosts {
            if let Some(bytes) = host.memory_total_bytes {
                output.push_str(&format!(
                    "agent_relay_memory_total_bytes{{host=\"{}\"}} {}\n",
                    prometheus_escape(&host.id),
                    bytes
                ));
            }
        }
        output.push_str(
            "# HELP agent_relay_tokens_per_second Latest completed generation rate.\n\
# TYPE agent_relay_tokens_per_second gauge\n",
        );
        for host in &snapshot.hosts {
            if let Some(rate) = host.tokens_per_second {
                output.push_str(&format!(
                    "agent_relay_tokens_per_second{{host=\"{}\"}} {}\n",
                    prometheus_escape(&host.id),
                    rate
                ));
            }
        }
        output.push_str(
            "# HELP agent_relay_model_loaded Whether a model profile is currently loaded.\n\
# TYPE agent_relay_model_loaded gauge\n",
        );
        for host in &snapshot.hosts {
            if let Some(model) = &host.loaded_model_id {
                output.push_str(&format!(
                    "agent_relay_model_loaded{{host=\"{}\",model=\"{}\"}} 1\n",
                    prometheus_escape(&host.id),
                    prometheus_escape(model)
                ));
            }
        }
        Ok(output)
    }

    fn flush(&self) {
        let (sender, receiver) = mpsc::sync_channel(0);
        if self.sender.try_send(TelemetryWrite::Flush(sender)).is_ok() {
            let _ = receiver.recv_timeout(Duration::from_secs(1));
        }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn open_database(path: &Path) -> Result<Connection, String> {
    let connection = Connection::open(path).map_err(database_error)?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(database_error)?;
    Ok(connection)
}

fn initialize_schema(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS request_events (
                 completed_at_ms INTEGER NOT NULL,
                 host_id TEXT NOT NULL,
                 model_id TEXT NOT NULL,
                 client TEXT NOT NULL,
                 outcome TEXT NOT NULL,
                 duration_ms INTEGER NOT NULL,
                 ttft_ms INTEGER,
                 prompt_tokens INTEGER,
                 output_tokens INTEGER,
                 tokens_per_second REAL
             );
             CREATE INDEX IF NOT EXISTS request_events_time
                 ON request_events(completed_at_ms);
             CREATE INDEX IF NOT EXISTS request_events_model
                 ON request_events(host_id, model_id, completed_at_ms);
             CREATE TABLE IF NOT EXISTS request_rollups_hourly (
                 bucket_ms INTEGER NOT NULL,
                 host_id TEXT NOT NULL,
                 model_id TEXT NOT NULL,
                 client TEXT NOT NULL,
                 outcome TEXT NOT NULL,
                 request_count INTEGER NOT NULL,
                 prompt_tokens INTEGER NOT NULL,
                 output_tokens INTEGER NOT NULL,
                 duration_ms INTEGER NOT NULL,
                 ttft_ms INTEGER NOT NULL,
                 ttft_count INTEGER NOT NULL,
                 tokens_per_second_sum REAL NOT NULL,
                 tokens_per_second_count INTEGER NOT NULL,
                 PRIMARY KEY(bucket_ms, host_id, model_id, client, outcome)
             );
             CREATE TABLE IF NOT EXISTS request_rollups_daily (
                 bucket_ms INTEGER NOT NULL,
                 host_id TEXT NOT NULL,
                 model_id TEXT NOT NULL,
                 client TEXT NOT NULL,
                 outcome TEXT NOT NULL,
                 request_count INTEGER NOT NULL,
                 prompt_tokens INTEGER NOT NULL,
                 output_tokens INTEGER NOT NULL,
                 duration_ms INTEGER NOT NULL,
                 ttft_ms INTEGER NOT NULL,
                 ttft_count INTEGER NOT NULL,
                 tokens_per_second_sum REAL NOT NULL,
                 tokens_per_second_count INTEGER NOT NULL,
                 PRIMARY KEY(bucket_ms, host_id, model_id, client, outcome)
             );
             CREATE TABLE IF NOT EXISTS lifecycle_events (
                 occurred_at_ms INTEGER NOT NULL,
                 host_id TEXT NOT NULL,
                 model_id TEXT,
                 action TEXT NOT NULL,
                 outcome TEXT NOT NULL,
                 duration_ms INTEGER NOT NULL,
                 forced INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS lifecycle_events_time
                 ON lifecycle_events(occurred_at_ms);
             CREATE TABLE IF NOT EXISTS host_samples (
                 sampled_at_ms INTEGER NOT NULL,
                 host_id TEXT NOT NULL,
                 online INTEGER NOT NULL,
                 loaded_model_id TEXT,
                 active_requests INTEGER NOT NULL,
                 memory_used_bytes INTEGER,
                 memory_total_bytes INTEGER,
                 tokens_per_second REAL
             );
             CREATE INDEX IF NOT EXISTS host_samples_time
                 ON host_samples(sampled_at_ms);",
        )
        .map_err(database_error)
}

fn writer_loop(path: &Path, receiver: mpsc::Receiver<TelemetryWrite>) {
    let Ok(mut connection) = open_database(path) else {
        return;
    };
    let _ = compact(&mut connection);
    let mut writes = 0_u32;
    while let Ok(event) = receiver.recv() {
        let result = match event {
            TelemetryWrite::Request(event) => insert_request(&connection, &event),
            TelemetryWrite::Lifecycle(event) => insert_lifecycle(&connection, &event),
            TelemetryWrite::Hosts(samples) => insert_host_samples(&mut connection, &samples),
            TelemetryWrite::Flush(sender) => {
                let _ = sender.send(());
                continue;
            }
        };
        if let Err(error) = result {
            eprintln!("failed to persist Agent Relay telemetry: {error}");
        }
        writes = writes.wrapping_add(1);
        if writes.is_multiple_of(256) {
            if let Err(error) = compact(&mut connection) {
                eprintln!("failed to compact Agent Relay telemetry: {error}");
            }
        }
    }
}

fn insert_request(connection: &Connection, event: &RequestTelemetry) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO request_events VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event.completed_at_ms,
                event.host_id,
                event.model_id,
                event.client,
                event.outcome,
                event.duration_ms,
                event.ttft_ms,
                event.prompt_tokens,
                event.output_tokens,
                event.tokens_per_second,
            ],
        )
        .map(|_| ())
        .map_err(database_error)
}

fn insert_lifecycle(connection: &Connection, event: &LifecycleTelemetry) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO lifecycle_events VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                event.occurred_at_ms,
                event.host_id,
                event.model_id,
                event.action,
                event.outcome,
                event.duration_ms,
                event.forced,
            ],
        )
        .map(|_| ())
        .map_err(database_error)
}

fn insert_host_samples(connection: &mut Connection, samples: &[HostSample]) -> Result<(), String> {
    let transaction = connection.transaction().map_err(database_error)?;
    {
        let mut statement = transaction
            .prepare("INSERT INTO host_samples VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")
            .map_err(database_error)?;
        for sample in samples {
            statement
                .execute(params![
                    sample.sampled_at_ms,
                    sample.host_id,
                    sample.online,
                    sample.loaded_model_id,
                    sample.active_requests,
                    sample.memory_used_bytes,
                    sample.memory_total_bytes,
                    sample.tokens_per_second,
                ])
                .map_err(database_error)?;
        }
    }
    transaction.commit().map_err(database_error)
}

fn compact(connection: &mut Connection) -> Result<(), String> {
    let now = now_ms();
    let detail_cutoff = now.saturating_sub(DETAIL_RETENTION_MS);
    let hourly_cutoff = now.saturating_sub(HOURLY_RETENTION_MS);
    let transaction = connection.transaction().map_err(database_error)?;
    transaction
        .execute(
            "INSERT INTO request_rollups_hourly
             SELECT (completed_at_ms / ?1) * ?1, host_id, model_id, client, outcome,
                    COUNT(*), COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(duration_ms), 0), COALESCE(SUM(ttft_ms), 0), COUNT(ttft_ms),
                    COALESCE(SUM(tokens_per_second), 0), COUNT(tokens_per_second)
             FROM request_events WHERE completed_at_ms < ?2
             GROUP BY 1, host_id, model_id, client, outcome
             ON CONFLICT(bucket_ms, host_id, model_id, client, outcome) DO UPDATE SET
               request_count = request_count + excluded.request_count,
               prompt_tokens = prompt_tokens + excluded.prompt_tokens,
               output_tokens = output_tokens + excluded.output_tokens,
               duration_ms = duration_ms + excluded.duration_ms,
               ttft_ms = ttft_ms + excluded.ttft_ms,
               ttft_count = ttft_count + excluded.ttft_count,
               tokens_per_second_sum = tokens_per_second_sum + excluded.tokens_per_second_sum,
               tokens_per_second_count = tokens_per_second_count + excluded.tokens_per_second_count",
            params![HOUR_MS, detail_cutoff],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "DELETE FROM request_events WHERE completed_at_ms < ?1",
            [detail_cutoff],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "INSERT INTO request_rollups_daily
             SELECT (bucket_ms / ?1) * ?1, host_id, model_id, client, outcome,
                    SUM(request_count), SUM(prompt_tokens), SUM(output_tokens), SUM(duration_ms),
                    SUM(ttft_ms), SUM(ttft_count), SUM(tokens_per_second_sum),
                    SUM(tokens_per_second_count)
             FROM request_rollups_hourly WHERE bucket_ms < ?2
             GROUP BY 1, host_id, model_id, client, outcome
             ON CONFLICT(bucket_ms, host_id, model_id, client, outcome) DO UPDATE SET
               request_count = request_count + excluded.request_count,
               prompt_tokens = prompt_tokens + excluded.prompt_tokens,
               output_tokens = output_tokens + excluded.output_tokens,
               duration_ms = duration_ms + excluded.duration_ms,
               ttft_ms = ttft_ms + excluded.ttft_ms,
               ttft_count = ttft_count + excluded.ttft_count,
               tokens_per_second_sum = tokens_per_second_sum + excluded.tokens_per_second_sum,
               tokens_per_second_count = tokens_per_second_count + excluded.tokens_per_second_count",
            params![DAY_MS, hourly_cutoff],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "DELETE FROM request_rollups_hourly WHERE bucket_ms < ?1",
            [hourly_cutoff],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "DELETE FROM lifecycle_events WHERE occurred_at_ms < ?1",
            [now.saturating_sub(LIFECYCLE_RETENTION_MS)],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "DELETE FROM host_samples WHERE sampled_at_ms < ?1",
            [now.saturating_sub(HOST_SAMPLE_RETENTION_MS)],
        )
        .map_err(database_error)?;
    transaction.commit().map_err(database_error)
}

fn prometheus_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn database_error(error: rusqlite::Error) -> String {
    format!("telemetry database error: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agent-relay-telemetry-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn stores_request_summaries_without_content() {
        let directory = test_directory("summary");
        let _ = std::fs::remove_dir_all(&directory);
        let store = TelemetryStore::new(&directory).expect("telemetry store");
        store.record_request(RequestTelemetry {
            completed_at_ms: now_ms(),
            host_id: "gpu-host".into(),
            model_id: "coding-large".into(),
            client: "opencode".into(),
            outcome: "success".into(),
            duration_ms: 2_000,
            ttft_ms: Some(250),
            prompt_tokens: Some(100),
            output_tokens: Some(50),
            tokens_per_second: Some(25.0),
        });
        let summary = store.summary(24).expect("summary");
        assert_eq!(summary.request_count, 1);
        assert_eq!(summary.output_tokens, 50);
        assert_eq!(summary.models[0].model_id, "coding-large");

        let connection = open_database(&directory.join(DATABASE_FILE)).expect("database");
        let columns = connection
            .prepare("PRAGMA table_info(request_events)")
            .expect("table info")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("columns")
            .collect::<Result<Vec<_>, _>>()
            .expect("column names");
        assert!(!columns
            .iter()
            .any(|name| name == "prompt" || name == "response"));
        drop(connection);
        drop(store);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn prometheus_output_escapes_host_labels() {
        assert_eq!(prometheus_escape("lab\\\"one"), "lab\\\\\\\"one");
    }
}
