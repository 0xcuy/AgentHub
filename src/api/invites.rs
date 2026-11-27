//! Workspace invite links (audit gap P1-1): until now every signup created a
//! new private empty workspace, so there was no way to join an existing team.
//! The workspace admin mints a one-time token (7-day expiry); a signup that
//! presents a valid token joins THAT workspace as a member instead of
//! creating a new one. See `ui::signup` for the accepting side.

use super::require_agent;
use crate::error::ApiError;
use crate::models::{gen_token, now_iso, now_plus, Agent};
use crate::state::AppState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;

/// Invites live for 7 days; expired and already-used tokens are rejected.
const INVITE_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 86400);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInvite {
    /// Origin used to render the human-readable invite link (e.g.
    /// "https://hub.sieralabs.com"). Optional: the response always carries
    /// the token itself and a relative `/invite/<token>` path.
    pub base_url: Option<String>,
}

/// One row of `workspace_invites`, as returned to the admin.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Invite {
    pub token: String,
    pub workspace_id: i64,
    pub created_by: i64,
    pub expires_at: String,
    pub used_at: Option<String>,
    pub created_at: String,
}

/// Used/expired/active status is derived at read time, not stored.
fn invite_json(invite: &Invite) -> serde_json::Value {
    let status = if invite.used_at.is_some() {
        "used"
    } else if invite.expires_at.as_str() <= now_iso().as_str() {
        "expired"
    } else {
        "active"
    };
    serde_json::json!({
        "token": invite.token,
        "workspace_id": invite.workspace_id,
        "created_by": invite.created_by,
        "expires_at": invite.expires_at,
        "used_at": invite.used_at,
        "created_at": invite.created_at,
        "status": status,
        "link": format!("/invite/{}", invite.token),
    })
}

/// The caller must be the admin of their workspace: the agent linked 1:1 to
/// the user who owns it (the same identity that bootstrapped #general).
/// Other members get 403; a workspace without a linked owner (legacy rows)
/// has no admin and rejects everyone.
fn require_workspace_admin(c: &rusqlite::Connection, agent: &Agent) -> Result<(), ApiError> {
    let admin_agent_id: Option<i64> = c
        .query_row(
            "SELECT u.agent_id FROM workspaces w JOIN users u ON u.id = w.owner_user_id WHERE w.id = ?1",
            [agent.workspace_id],
            |r| r.get(0),
        )
        .optional()?;
    if admin_agent_id != Some(agent.id) {
        return Err(ApiError::forbidden_msg(
            "only the workspace admin can manage invites",
        ));
    }
    Ok(())
}

/// POST /v1/workspaces/invites — mint a one-time invite token (admin only).
pub async fn create_invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Option<Json<CreateInvite>>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let base_url = req
        .and_then(|Json(r)| r.base_url)
        .map(|b| b.trim().trim_end_matches('/').to_string())
        .filter(|b| b.starts_with("https://") || b.starts_with("http://"));
    let now = now_iso();
    let expires_at = now_plus(INVITE_TTL);
    let invite = state.db.try_with(|c| {
        require_workspace_admin(c, &agent)?;
        let invite = Invite {
            token: gen_token(),
            workspace_id: agent.workspace_id,
            created_by: agent.id,
            expires_at,
            used_at: None,
            created_at: now,
        };
        c.execute(
            "INSERT INTO workspace_invites (token, workspace_id, created_by, expires_at, used_at, created_at) VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            params![invite.token, invite.workspace_id, invite.created_by, invite.expires_at, invite.created_at],
        )?;
        Ok(invite)
    })?;
    let mut v = invite_json(&invite);
    if let Some(base) = base_url {
        v["link"] = serde_json::Value::String(format!("{base}/invite/{}", invite.token));
    }
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "invite": v })),
    ))
}

/// GET /v1/workspaces/invites — list every invite of the caller's workspace,
/// newest first, with used/expired status (admin only).
pub async fn list_invites(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let invites = state.db.try_with(|c| -> Result<Vec<Invite>, ApiError> {
        require_workspace_admin(c, &agent)?;
        let mut stmt = c.prepare(
            "SELECT token, workspace_id, created_by, expires_at, used_at, created_at FROM workspace_invites WHERE workspace_id = ?1 ORDER BY created_at DESC, rowid DESC",
        )?;
        let rows = stmt
            .query_map([agent.workspace_id], |r| {
                Ok(Invite {
                    token: r.get(0)?,
                    workspace_id: r.get(1)?,
                    created_by: r.get(2)?,
                    expires_at: r.get(3)?,
                    used_at: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<Invite>>>()?;
        Ok(rows)
    })?;
    let invites: Vec<serde_json::Value> = invites.iter().map(invite_json).collect();
    Ok(Json(serde_json::json!({ "invites": invites })))
}

/// Consume a signup's invite token inside the signup transaction: resolve and
/// validate it, move the freshly created agent into that workspace as a
/// member of #general, and mark the token used (single-use). Returns the
/// joined workspace's id, name and slug. Unknown tokens read as invalid
/// (400), already-used as 409 and expired as 410.
pub fn accept_signup_invite(
    tx: &rusqlite::Transaction<'_>,
    agent_id: i64,
    token: &str,
    now: &str,
) -> Result<(i64, String, String), ApiError> {
    let invite: Option<(i64, String, Option<String>)> = tx
        .query_row(
            "SELECT workspace_id, expires_at, used_at FROM workspace_invites WHERE token = ?1",
            [token],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((workspace_id, expires_at, used_at)) = invite else {
        return Err(ApiError::bad_request("this invite link is not valid"));
    };
    if used_at.is_some() {
        return Err(ApiError::conflict("this invite link has already been used"));
    }
    if expires_at.as_str() <= now {
        return Err(ApiError::with_code(
            StatusCode::GONE,
            "invite_expired",
            "this invite link has expired",
        ));
    }
    let (ws_name, ws_slug): (String, String) = tx.query_row(
        "SELECT name, slug FROM workspaces WHERE id = ?1",
        [workspace_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    tx.execute(
        "UPDATE agents SET workspace_id = ?1 WHERE id = ?2",
        params![workspace_id, agent_id],
    )?;
    let channel_id: i64 = tx
        .query_row(
            "SELECT id FROM channels WHERE workspace_id = ?1 AND name = 'general' ORDER BY id LIMIT 1",
            [workspace_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| {
            ApiError::with_code(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "invited workspace has no general channel",
            )
        })?;
    tx.execute(
        "INSERT INTO channel_members (channel_id, agent_id, joined_at, role) VALUES (?1, ?2, ?3, 'member')",
        params![channel_id, agent_id, now],
    )?;
    tx.execute(
        "UPDATE workspace_invites SET used_at = ?1 WHERE token = ?2 AND used_at IS NULL",
        params![now, token],
    )?;
    Ok((workspace_id, ws_name, ws_slug))
}

/// GET /ui/api/invite-info/{token} — public, read-only status check that
/// backs the banner on the /invite/<token> landing page. Never echoes the
/// workspace name unless the token is live; every invalid shape reads as
/// "unknown" so used and mistyped tokens are indistinguishable to probers.
pub async fn invite_info(
    State(state): State<AppState>,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let token = token.trim().to_string();
    let row: Option<(String, Option<String>, String)> = state.db.with(|c| {
        c.query_row(
            "SELECT w.name, i.used_at, i.expires_at FROM workspace_invites i JOIN workspaces w ON w.id = i.workspace_id WHERE i.token = ?1",
            [&token],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
    })?;
    let Some((workspace_name, used_at, expires_at)) = row else {
        return Ok(Json(serde_json::json!({ "status": "unknown" })));
    };
    let status = if used_at.is_some() {
        "used"
    } else if expires_at.as_str() <= now_iso().as_str() {
        "expired"
    } else {
        "active"
    };
    Ok(Json(serde_json::json!({
        "status": status,
        "workspace_name": if status == "active" { serde_json::Value::String(workspace_name) } else { serde_json::Value::Null },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::build_router;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tower::util::Oneshot;

    type RouterT = axum::Router;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_app() -> (RouterT, crate::db::Db) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let db = crate::db::Db::open(&format!(
            "file:aghub-invites-test-{n}-{}?mode=memory&cache=private",
            std::process::id()
        ))
        .or_else(|_| crate::db::Db::open(":memory:"))
        .expect("open test db");
        db.migrate().expect("migrate");
        // Workspace-era bootstrap: POST /v1/agents requires a caller token
        // (open self-registration was a cross-workspace leak), so seed one
        // agent directly; it becomes the caller for register() below.
        db.with(|c| {
            c.execute(
                "INSERT INTO agents (handle, name, token, webhook_url, created_at, workspace_id) VALUES ('bootstrap', 'Bootstrap', 'test-bootstrap-token', NULL, ?1, 1)",
                [now_iso()],
            )
        })
        .expect("seed bootstrap agent");
        let router = build_router(crate::state::AppState::new(
            db.clone(),
            reqwest::Client::builder().build().unwrap(),
        ));
        (router, db)
    }

    async fn call(
        router: &RouterT,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let req = match body {
            // A bodyless call must not claim a JSON content type: axum's
            // Option<Json<T>> only defaults to None for a missing one.
            Some(b) => req
                .header("Content-Type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let res = Oneshot::new(router.clone(), req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, v)
    }

    /// Human signup: the new user owns its own workspace and is its admin.
    async fn signup_user(
        router: &RouterT,
        email: &str,
        handle: &str,
        invite: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut body = json!({
            "email": email,
            "password": "correct horse battery",
            "name": "Test User",
            "handle": handle,
        });
        if let Some(t) = invite {
            body["invite_token"] = Value::String(t.to_string());
        }
        call(router, "POST", "/ui/signup", None, Some(body)).await
    }

    #[tokio::test]
    async fn invite_flow_admin_creates_member_joins() {
        let (router, _db) = test_app();
        // Admin signs up (owns its workspace) and mints an invite.
        let (status, admin) =
            signup_user(&router, "admin@example.com", "invites-admin", None).await;
        assert_eq!(status, StatusCode::OK, "admin signup failed: {admin}");
        let admin_token = admin["agent"]["token"].as_str().unwrap();
        let admin_workspace = admin["workspace"]["id"].as_i64().unwrap();

        let (status, v) = call(
            &router,
            "POST",
            "/v1/workspaces/invites",
            Some(admin_token),
            Some(json!({ "base_url": "https://hub.example.com" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "create invite failed: {v}");
        let invite = &v["invite"];
        assert_eq!(invite["workspace_id"], admin_workspace);
        assert_eq!(invite["used_at"], Value::Null);
        assert_eq!(invite["status"], "active");
        assert_eq!(
            invite["link"],
            format!(
                "https://hub.example.com/invite/{}",
                invite["token"].as_str().unwrap()
            )
        );

        // The public banner endpoint reports the live token as active and
        // names the workspace it joins.
        let (status, v) = call(
            &router,
            "GET",
            &format!("/ui/api/invite-info/{}", invite["token"].as_str().unwrap()),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["status"], "active");
        assert_eq!(v["workspace_name"], admin["workspace"]["name"]);

        // Listing shows the invite, newest first.
        let (status, v) = call(
            &router,
            "GET",
            "/v1/workspaces/invites",
            Some(admin_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["invites"].as_array().unwrap().len(), 1);
        assert_eq!(v["invites"][0]["token"], invite["token"]);

        // Non-admin agents (registered into the same workspace) get 403.
        let member_token = call(
            &router,
            "POST",
            "/v1/agents",
            Some(admin_token),
            Some(json!({ "name": "Member Bot", "handle": "invites-member-bot" })),
        )
        .await
        .1["token"]
            .as_str()
            .unwrap()
            .to_string();
        for method in ["POST", "GET"] {
            let (status, v) = call(
                &router,
                method,
                "/v1/workspaces/invites",
                Some(&member_token),
                Some(json!({})),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} by non-admin: {v}");
        }
        // And without a token at all: 401.
        let (status, _) = call(&router, "GET", "/v1/workspaces/invites", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Signing up with the invite joins the ADMIN's workspace...
        let (status, joined) = signup_user(
            &router,
            "member@example.com",
            "invites-member",
            Some(invite["token"].as_str().unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "invite signup failed: {joined}");
        assert_eq!(joined["workspace"]["id"], admin_workspace);
        assert_eq!(joined["workspace"]["slug"], admin["workspace"]["slug"]);

        // ...as a member of the workspace's #general channel.
        let member = joined["agent"]["token"].as_str().unwrap();
        let (status, v) = call(&router, "GET", "/v1/channels", Some(member), None).await;
        assert_eq!(status, StatusCode::OK, "channels failed: {v}");
        let channels = v["channels"].as_array().unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0]["name"], "general");

        // ...and the token is single-use: a second signup with it is rejected.
        let (status, v) = signup_user(
            &router,
            "second@example.com",
            "invites-second",
            Some(invite["token"].as_str().unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "reuse must be rejected: {v}");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already been used"));
        // The list now reports the invite as used.
        let (status, v) = call(
            &router,
            "GET",
            "/v1/workspaces/invites",
            Some(admin_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["invites"][0]["status"], "used");
        assert!(v["invites"][0]["used_at"].is_string());
    }

    #[tokio::test]
    async fn expired_invite_is_rejected() {
        let (router, db) = test_app();
        let (status, admin) = signup_user(
            &router,
            "expired-admin@example.com",
            "invites-expired-admin",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "admin signup failed: {admin}");
        let admin_token = admin["agent"]["token"].as_str().unwrap();
        let (status, v) = call(
            &router,
            "POST",
            "/v1/workspaces/invites",
            Some(admin_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "create invite failed: {v}");
        let token = v["invite"]["token"].as_str().unwrap().to_string();

        // Backdate the invite past its 7-day expiry.
        let past = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        db.with(|c| {
            c.execute(
                "UPDATE workspace_invites SET expires_at = ?1 WHERE token = ?2",
                rusqlite::params![past, token],
            )
        })
        .expect("backdate invite");

        let (status, v) =
            signup_user(&router, "late@example.com", "invites-late", Some(&token)).await;
        assert_eq!(
            status,
            StatusCode::GONE,
            "expired invite must be rejected: {v}"
        );
        assert!(v["error"]["message"].as_str().unwrap().contains("expired"));
        // The rejected signup must not have created an account.
        let (status, _) = call(&router, "GET", "/v1/agents/me", Some("unused"), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // And the invite still shows as expired, not used.
        let (status, v) = call(
            &router,
            "GET",
            "/v1/workspaces/invites",
            Some(admin_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["invites"][0]["status"], "expired");
    }

    #[tokio::test]
    async fn unknown_invite_token_is_rejected() {
        let (router, _db) = test_app();
        let (status, v) = signup_user(
            &router,
            "ghost@example.com",
            "invites-ghost",
            Some("not-a-real-token"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "unknown token: {v}");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not valid"));
        // The public banner endpoint reports unknown tokens without detail.
        let (status, v) = call(
            &router,
            "GET",
            "/ui/api/invite-info/not-a-real-token",
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["status"], "unknown");
        assert_eq!(v["workspace_name"], Value::Null);
    }
}
