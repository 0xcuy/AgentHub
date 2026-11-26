use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct Agent {
    pub id: i64,
    pub handle: String,
    pub name: String,
    pub webhook_url: Option<String>,
    pub created_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAgent {
    pub name: String,
    pub handle: String,
    pub webhook_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAgent {
    pub webhook_url: Option<String>,
}

#[derive(Serialize)]
pub struct Channel {
    pub id: i64,
    pub name: String,
    pub topic: Option<String>,
    pub created_at: String,
    pub post_policy: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateChannel {
    pub name: String,
    pub topic: Option<String>,
    #[serde(default)]
    pub member_ids: Vec<i64>,
    #[serde(default)]
    pub post_policy: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateChannel {
    pub topic: Option<String>,
    pub post_policy: Option<String>,
}

pub fn valid_post_policy(p: &str) -> bool {
    matches!(p, "members" | "admins")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddMembers {
    pub agent_ids: Vec<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Message {
    pub id: i64,
    pub channel_id: Option<i64>,
    pub sender_id: i64,
    pub sender_handle: String,
    pub recipient_id: Option<i64>,
    pub body: String,
    pub created_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMessage {
    pub body: String,
}

#[derive(Serialize)]
pub struct InboxItem {
    pub receipt_id: i64,
    pub read_at: Option<String>,
    pub message: Message,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadInbox {
    pub message_ids: Option<Vec<i64>>,
    pub all: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MailMessage {
    pub id: i64,
    pub sender_id: i64,
    pub sender_handle: String,
    pub recipient_id: i64,
    pub subject: String,
    pub body: String,
    pub read_at: Option<String>,
    pub created_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMail {
    pub to_agent_id: Option<i64>,
    pub to_handle: Option<String>,
    pub subject: String,
    pub body: String,
}

#[derive(Serialize)]
pub struct AgentRun {
    pub id: i64,
    pub agent_id: i64,
    pub message_id: Option<i64>,
    pub trigger_type: String,
    pub status: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub result: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRun {
    pub message_id: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteRun {
    pub status: String,
    pub result: Option<String>,
}

#[derive(Deserialize)]
pub struct Page {
    pub after_id: Option<i64>,
    pub limit: Option<i64>,
    pub unread: Option<bool>,
}

pub fn page_params(p: &Page) -> (i64, i64, bool) {
    let limit = p.limit.unwrap_or(50).clamp(1, 100);
    (p.after_id.unwrap_or(0), limit, p.unread.unwrap_or(false))
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn now_plus(d: std::time::Duration) -> String {
    let later = chrono::Utc::now()
        + chrono::Duration::from_std(d).unwrap_or(chrono::Duration::seconds(60));
    later.to_rfc3339()
}

pub fn gen_token() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::thread_rng().gen();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn valid_handle(h: &str) -> bool {
    (2..=32).contains(&h.len())
        && h.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}
