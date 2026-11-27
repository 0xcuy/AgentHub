use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct Agent {
    pub id: i64,
    pub handle: String,
    pub name: String,
    pub webhook_url: Option<String>,
    /// Free-form short role (e.g. "mandor", "QA", "worker"); empty by default.
    pub role: String,
    pub created_at: String,
    pub workspace_id: i64,
}

/// A tenant boundary: 1 email = 1 workspace. Everything an agent can see —
/// channels, DMs, memories, repos — is scoped to its workspace.
#[derive(Serialize)]
pub struct Workspace {
    pub id: i64,
    pub name: String,
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
    /// Absent = keep the current webhook, null = clear it, string = set it.
    #[serde(default, deserialize_with = "present_or_null")]
    pub webhook_url: Option<Option<String>>,
    #[serde(default)]
    pub role: Option<String>,
}

fn present_or_null<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(d).map(Some)
}

#[derive(Serialize)]
pub struct Channel {
    pub id: i64,
    pub name: String,
    pub topic: Option<String>,
    pub created_at: String,
    pub post_policy: String,
    /// How channel messages wake members' webhooks: "mentions" (default —
    /// only @-mentions of the member's handle, or explicit wake requests) or
    /// "all" (every message wakes every webhook member).
    pub wake_mode: String,
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
    pub wake_mode: Option<String>,
}

pub fn valid_post_policy(p: &str) -> bool {
    matches!(p, "members" | "admins")
}

/// Channel wake modes: "mentions" wakes a member's webhook only when the
/// message @-mentions that member (or the sender requests a wake); "all"
/// wakes every webhook member on every message.
pub fn valid_wake_mode(m: &str) -> bool {
    matches!(m, "mentions" | "all")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddMembers {
    pub agent_ids: Vec<i64>,
}

/// The message a reply answers, trimmed to a short snippet for list views.
#[derive(Clone, Debug, Serialize)]
pub struct MessageReply {
    pub id: i64,
    pub sender_handle: String,
    pub body: String,
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
    pub urgent: bool,
    pub reply_to: Option<MessageReply>,
    pub edited_at: Option<String>,
    pub task_status: Option<String>,
}

/// Reply context is a preview: keep the first 140 characters and mark the cut.
pub fn reply_snippet(body: &str) -> String {
    let mut out: String = body.chars().take(140).collect();
    if out.chars().count() < body.chars().count() {
        out.push('…');
    }
    out
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMessage {
    pub body: String,
    /// Explicit wake request: in "mentions" mode, send this to every webhook
    /// member of the channel regardless of @-mentions.
    #[serde(default)]
    pub wake: Option<bool>,
    #[serde(default)]
    pub urgent: Option<bool>,
    /// The message this one answers; must be in the same channel/DM.
    #[serde(default)]
    pub reply_to_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditMessage {
    pub body: String,
}

/// `status: null` (or an omitted field) clears the task status.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetTaskStatus {
    #[serde(default)]
    pub status: Option<String>,
}

pub const TASK_STATUSES: [&str; 3] = ["claimed", "blocked", "done"];

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

#[derive(Clone, Debug, Serialize)]
pub struct Memory {
    pub id: i64,
    pub agent_id: i64,
    pub agent_handle: String,
    pub scope: String,
    pub channel_id: Option<i64>,
    pub kind: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source_message_id: Option<i64>,
    pub source_channel_id: Option<i64>,
    pub created_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMemory {
    pub content: String,
    pub kind: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub scope: Option<String>,
    pub channel_id: Option<i64>,
    pub source_message_id: Option<i64>,
    pub source_channel_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct MemoryQuery {
    pub query: Option<String>,
    pub kind: Option<String>,
    pub scope: Option<String>,
    pub channel_id: Option<i64>,
    pub limit: Option<i64>,
}

pub const MEMORY_KINDS: &[&str] = &["fakta", "keputusan", "preferensi"];
pub const MEMORY_SCOPES: &[&str] = &["private", "channel"];

pub fn valid_memory_kind(k: &str) -> bool {
    MEMORY_KINDS.contains(&k)
}

pub fn valid_memory_scope(s: &str) -> bool {
    MEMORY_SCOPES.contains(&s)
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

/// A notebook entry from `notes`, scoped to its owning agent.
#[derive(Clone, Debug, Serialize)]
pub struct Note {
    pub id: i64,
    pub owner: i64,
    pub title: Option<String>,
    pub content: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateNote {
    pub content: String,
    pub title: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateNote {
    pub content: Option<String>,
    pub title: Option<String>,
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

/// A channel's linked GitHub repository, as stored in `github_repos`.
#[derive(Clone, Debug, Serialize)]
pub struct GitHubRepo {
    pub id: i64,
    pub channel_id: i64,
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub linked_by: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// One cached pull request from `github_pulls`.
#[derive(Clone, Debug, Serialize)]
pub struct GitHubPull {
    pub number: i64,
    pub title: String,
    pub state: String,
    pub draft: bool,
    pub author: Option<String>,
    pub url: String,
    pub fetched_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkGitHubRepo {
    pub owner: String,
    pub name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePullRequest {
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: Option<String>,
}

#[derive(Deserialize)]
pub struct PullsQuery {
    pub state: Option<String>,
    pub limit: Option<i64>,
}

pub const PULL_STATES: &[&str] = &["open", "closed", "all"];

pub fn valid_pull_state(s: &str) -> bool {
    PULL_STATES.contains(&s)
}
