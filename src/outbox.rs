use crate::db::Db;
use crate::models::now_iso;
use rusqlite::params;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const MAX_ATTEMPTS: u32 = 3;
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Long-running outbox dispatcher: delivers queued agent runs, surviving
/// restarts because the queue lives in SQLite (`agent_runs` rows with
/// status 'queued'). Wakes immediately when a run is enqueued.
pub async fn run_dispatcher(state: crate::state::AppState) {
    let inflight: Arc<Mutex<HashSet<i64>>> = Arc::new(Mutex::new(HashSet::new()));
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = state.outbox_notify.notified() => {}
        }
        dispatch_due(&state, &inflight).await;
    }
}

/// Claim and deliver every due run; returns the ids that were attempted.
async fn dispatch_due(state: &crate::state::AppState, inflight: &Arc<Mutex<HashSet<i64>>>) -> usize {
    let due: Vec<i64> = state
        .db
        .with(|c| {
            let mut stmt = c.prepare(
                "SELECT id FROM agent_runs
                 WHERE status = 'queued' AND (next_attempt_at IS NULL OR next_attempt_at <= ?1)
                 ORDER BY id LIMIT 100",
            )?;
            stmt.query_map(params![now_iso()], |r| r.get(0))
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    let mut claimed = 0;
    for run_id in due {
        if !inflight.lock().unwrap().insert(run_id) {
            continue; // already being delivered in this process
        }
        let db = state.db.clone();
        let http = state.http.clone();
        let inflight = Arc::clone(inflight);
        tokio::spawn(async move {
            deliver_once(&db, &http, run_id).await;
            inflight.lock().unwrap().remove(&run_id);
        });
        claimed += 1;
    }
    claimed
}

/// Run due deliveries synchronously (used by tests): returns the ids attempted.
pub async fn process_due(db: &Db, http: &reqwest::Client) -> Vec<i64> {
    let due: Vec<i64> = db
        .with(|c| {
            let mut stmt = c.prepare(
                "SELECT id FROM agent_runs
                 WHERE status = 'queued' AND (next_attempt_at IS NULL OR next_attempt_at <= ?1)
                 ORDER BY id LIMIT 100",
            )?;
            stmt.query_map(params![now_iso()], |r| r.get(0))
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    for run_id in due.clone() {
        deliver_once(db, http, run_id).await;
    }
    due
}

/// One delivery attempt for a run, with the outcome persisted so retries
/// survive restarts. Called only for runs whose `next_attempt_at` is due.
pub async fn deliver_once(db: &Db, http: &reqwest::Client, run_id: i64) {
    let payload_row = db.with(|c| {
        c.query_row(
            "SELECT r.agent_id, a.webhook_url, a.handle, r.message_id, r.trigger_type,
                    m.sender_id, sa.handle, m.channel_id, m.recipient_id, m.body
             FROM agent_runs r
             JOIN agents a ON a.id = r.agent_id
             LEFT JOIN messages m ON m.id = r.message_id
             LEFT JOIN agents sa ON sa.id = m.sender_id
             WHERE r.id = ?1",
            [run_id],
            |r| {
                Ok(serde_json::json!({
                    "run_id": run_id,
                    "agent_id": r.get::<_, i64>(0)?,
                    "webhook_url": r.get::<_, Option<String>>(1)?,
                    "agent_handle": r.get::<_, String>(2)?,
                    "message_id": r.get::<_, Option<i64>>(3)?,
                    "trigger_type": r.get::<_, String>(4)?,
                    "message": match r.get::<_, Option<i64>>(5)? {
                        Some(sender_id) => serde_json::json!({
                            "sender_id": sender_id,
                            "sender_handle": r.get::<_, Option<String>>(6)?,
                            "channel_id": r.get::<_, Option<i64>>(7)?,
                            "recipient_id": r.get::<_, Option<i64>>(8)?,
                            "body": r.get::<_, Option<String>>(9)?,
                        }),
                        None => serde_json::Value::Null,
                    },
                }))
            },
        )
    });
    let payload = match payload_row {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(run_id, error = %e, "failed to load run for delivery");
            return;
        }
    };
    let url = match payload["webhook_url"].as_str() {
        Some(u) => u.to_string(),
        None => return, // run enqueued without a webhook; stays queued
    };

    let res = http
        .post(&url)
        .timeout(HTTP_TIMEOUT)
        .json(&payload)
        .send()
        .await;

    let outcome: Result<(), String> = match res {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => Err(format!("webhook returned HTTP {}", resp.status())),
        Err(e) => Err(e.to_string()),
    };

    match outcome {
        Ok(()) => {
            let _ = db.with(|c| {
                c.execute(
                    "UPDATE agent_runs SET status = 'delivered', attempts = attempts + 1,
                        next_attempt_at = NULL, last_error = NULL, updated_at = ?1
                     WHERE id = ?2",
                    params![now_iso(), run_id],
                )
            });
        }
        Err(err) => {
            tracing::warn!(run_id, error = %err, "webhook attempt failed");
            let attempts_now: i64 = db
                .with(|c| c.query_row("SELECT attempts FROM agent_runs WHERE id = ?1", [run_id], |r| r.get(0)))
                .unwrap_or(MAX_ATTEMPTS as i64);
            let next = attempts_now as u32 + 1;
            if next >= MAX_ATTEMPTS {
                let _ = db.with(|c| {
                    c.execute(
                        "UPDATE agent_runs SET status = 'failed', attempts = ?1, last_error = ?2,
                            next_attempt_at = NULL, updated_at = ?3
                         WHERE id = ?4",
                        params![next, err, now_iso(), run_id],
                    )
                });
            } else {
                let backoff = jittered_backoff(next);
                let due = crate::models::now_plus(backoff);
                tracing::info!(run_id, next_attempt_in_secs = backoff.as_secs(), "retrying later");
                let _ = db.with(|c| {
                    c.execute(
                        "UPDATE agent_runs SET attempts = ?1, next_attempt_at = ?2, last_error = ?3,
                            updated_at = ?4
                         WHERE id = ?5",
                        params![next, due, err, now_iso(), run_id],
                    )
                });
            }
        }
    }
}

/// Exponential backoff with full jitter: base 2^(attempt-1) seconds, plus a
/// random 0..base seconds, capped at 60s.
fn jittered_backoff(attempt: u32) -> Duration {
    let base = 2u64.pow(attempt.saturating_sub(1)).min(60);
    let jitter = if base > 0 { rand_u64(base) } else { 0 };
    Duration::from_secs(base + jitter)
}

fn rand_u64(max: u64) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Cheap non-crypto jitter source; only retry timing depends on it.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 20))
        .unwrap_or(0);
    nanos % max
}
