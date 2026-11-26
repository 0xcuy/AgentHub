use crate::db::Db;
use crate::models::now_iso;
use rusqlite::params;

pub fn enqueue_runs(
    tx: &rusqlite::Transaction,
    message_id: i64,
    agent_ids: &[i64],
    trigger_type: &str,
) -> Result<Vec<i64>, rusqlite::Error> {
    let now = now_iso();
    let mut ids = Vec::new();
    for agent_id in agent_ids {
        let has_webhook: i64 = tx.query_row(
            "SELECT COUNT(*) FROM agents WHERE id = ?1 AND webhook_url IS NOT NULL",
            [agent_id],
            |r| r.get(0),
        )?;
        if has_webhook > 0 {
            let id: i64 = tx.query_row(
                "INSERT INTO agent_runs (agent_id, message_id, trigger_type, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4) RETURNING id",
                params![agent_id, message_id, trigger_type, now],
                |r| r.get(0),
            )?;
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Deliver a run to the agent's webhook: POST payload with timeout,
/// up to 3 attempts with exponential backoff, then mark delivered/failed.
pub async fn deliver_run(db: Db, http: reqwest::Client, run_id: i64) {
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
    let payload = &payload;

    let mut last_error = String::new();
    let mut success = false;
    for attempt in 1..=3 {
        let res = http
            .post(&url)
            .timeout(std::time::Duration::from_secs(10))
            .json(payload)
            .send()
            .await;
        match res {
            Ok(resp) if resp.status().is_success() => {
                let _ = db.with(|c| {
                    c.execute(
                        "UPDATE agent_runs SET status = 'delivered', attempts = ?1, updated_at = ?2 WHERE id = ?3",
                        params![attempt, now_iso(), run_id],
                    )
                });
                success = true;
                break;
            }
            Ok(resp) => {
                last_error = format!("webhook returned HTTP {}", resp.status());
                tracing::warn!(run_id, attempt, error = %last_error, "webhook attempt failed");
            }
            Err(e) => {
                last_error = e.to_string();
                tracing::warn!(run_id, attempt, error = %last_error, "webhook attempt failed");
            }
        }
        if attempt < 3 && !success {
            tokio::time::sleep(std::time::Duration::from_millis(200 * 2u64.pow(attempt))).await;
        }
    }
    if !success {
        let _ = db.with(|c| {
            c.execute(
                "UPDATE agent_runs SET status = 'failed', attempts = 3, last_error = ?1, updated_at = ?2 WHERE id = ?3",
                params![last_error, now_iso(), run_id],
            )
        });
    }
}
