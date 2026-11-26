//! Path B: (WhatsApp / Google Messages →) Matrix → Bens Chat.
//!
//! The homeserver pushes transactions to the axum endpoint; the handler only
//! dedupes the transaction id and queues the events, then returns 200. A
//! single worker processes the queue in order.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State as AxState};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::State;

#[derive(Clone)]
struct Http {
    st: Arc<State>,
    tx: UnboundedSender<Value>,
}

pub fn router(st: Arc<State>, tx: UnboundedSender<Value>) -> Router {
    Router::new()
        .route("/_matrix/app/v1/transactions/{txn}", put(transaction))
        .route("/transactions/{txn}", put(transaction)) // legacy path
        .route("/_matrix/app/v1/users/{user_id}", get(query_user))
        .route("/_matrix/app/v1/rooms/{alias}", get(query_room))
        .route("/_matrix/app/v1/ping", post(ping))
        .route("/health", get(|| async { "ok" }))
        .with_state(Http { st, tx })
}

#[derive(Deserialize)]
struct AuthQuery {
    access_token: Option<String>,
}

fn authorized(st: &State, headers: &HeaderMap, q: &AuthQuery) -> bool {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    bearer.or(q.access_token.as_deref()) == Some(st.cfg.hs_token.as_str())
}

fn forbidden() -> (StatusCode, Json<Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(json!({"errcode": "M_FORBIDDEN", "error": "bad hs_token"})),
    )
}

async fn transaction(
    AxState(h): AxState<Http>,
    Path(txn): Path<String>,
    Query(q): Query<AuthQuery>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !authorized(&h.st, &headers, &q) {
        return forbidden();
    }
    match h.st.db.claim_txn(&txn).await {
        Ok(true) => {
            if let Some(events) = body["events"].as_array() {
                for ev in events {
                    let _ = h.tx.send(ev.clone());
                }
            }
        }
        Ok(false) => tracing::debug!(txn, "duplicate transaction"),
        Err(e) => {
            // let the homeserver retry later
            tracing::error!(txn, "recording transaction: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"errcode": "M_UNKNOWN"})),
            );
        }
    }
    (StatusCode::OK, Json(json!({})))
}

async fn query_user(
    AxState(h): AxState<Http>,
    Path(user_id): Path<String>,
    Query(q): Query<AuthQuery>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    if !authorized(&h.st, &headers, &q) {
        return forbidden();
    }
    // We only create puppets for Bens Chat users we know about.
    let known =
        h.st.db
            .users()
            .await
            .map(|u| u.iter().any(|(_, m)| *m == user_id))
            .unwrap_or(false);
    if known
        && let Some(lp) = user_id.strip_prefix('@').and_then(|s| s.split(':').next())
        && h.st.mx.ensure_registered(lp).await.is_ok()
    {
        return (StatusCode::OK, Json(json!({})));
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({"errcode": "M_NOT_FOUND"})),
    )
}

async fn query_room(
    AxState(h): AxState<Http>,
    Query(q): Query<AuthQuery>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    if !authorized(&h.st, &headers, &q) {
        return forbidden();
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({"errcode": "M_NOT_FOUND"})),
    )
}

async fn ping(
    AxState(h): AxState<Http>,
    Query(q): Query<AuthQuery>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    if !authorized(&h.st, &headers, &q) {
        return forbidden();
    }
    (StatusCode::OK, Json(json!({})))
}

// ------------------------------------------------------------------ worker

pub async fn run(mut rx: UnboundedReceiver<Value>, st: Arc<State>) {
    let mut names: HashMap<String, String> = HashMap::new();
    while let Some(ev) = rx.recv().await {
        let id = ev["event_id"].as_str().unwrap_or("?").to_owned();
        if let Err(e) = handle(&st, &ev, &mut names).await {
            tracing::error!(event_id = id, "matrix → bens chat failed: {e}");
        }
    }
}

async fn handle(
    st: &State,
    ev: &Value,
    names: &mut HashMap<String, String>,
) -> Result<(), crate::Error> {
    match ev["type"].as_str() {
        Some("m.room.member") => on_member(st, ev).await,
        Some("m.room.message") => on_message(st, ev, names).await,
        _ => Ok(()),
    }
}

/// Auto-accept invites to our puppets (portal rooms created by mautrix).
async fn on_member(st: &State, ev: &Value) -> Result<(), crate::Error> {
    let (Some(target), Some(room)) = (ev["state_key"].as_str(), ev["room_id"].as_str()) else {
        return Ok(());
    };
    if ev["content"]["membership"] != "invite" || !st.mx.is_puppet(target) {
        return Ok(());
    }
    if let Some(lp) = target.strip_prefix('@').and_then(|s| s.split(':').next()) {
        st.mx.ensure_registered(lp).await?;
    }
    st.mx.join(room, target).await?;
    let mapped = st.db.chat_by_room(room).await?.is_some();
    tracing::info!(
        room,
        puppet = target,
        inviter = ev["sender"].as_str().unwrap_or("?"),
        mapped,
        "joined portal room{}",
        if mapped {
            ""
        } else {
            " (not mapped to a Bens Chat chat yet — see `bc-bridge rooms`)"
        }
    );
    Ok(())
}

/// Text to post into Bens Chat for a Matrix message, or None to skip it.
fn body_text(content: &Value) -> Option<String> {
    // edits (phase 5) — skip so they don't show up as duplicate messages
    if content["m.relates_to"]["rel_type"] == "m.replace" {
        return None;
    }
    let body = content["body"].as_str()?.trim();
    if body.is_empty() {
        return None;
    }
    let text = match content["msgtype"].as_str()? {
        "m.text" | "m.notice" => body.to_owned(),
        "m.emote" => format!("* {body}"),
        // media (phase 5): placeholder until MinIO <-> Matrix media is bridged
        "m.image" => format!("[image] {body}"),
        "m.video" => format!("[video] {body}"),
        "m.audio" => format!("[audio] {body}"),
        "m.file" => format!("[file] {body}"),
        "m.location" => format!("[location] {body}"),
        _ => body.to_owned(),
    };
    Some(text)
}

async fn sender_name(st: &State, user_id: &str, names: &mut HashMap<String, String>) -> String {
    if let Some(n) = names.get(user_id) {
        return n.clone();
    }
    let n = st.mx.displayname(user_id).await.unwrap_or_else(|| {
        user_id
            .trim_start_matches('@')
            .split(':')
            .next()
            .unwrap_or(user_id)
            .to_owned()
    });
    if names.len() > 10_000 {
        names.clear();
    }
    names.insert(user_id.to_owned(), n.clone());
    n
}

async fn on_message(
    st: &State,
    ev: &Value,
    names: &mut HashMap<String, String>,
) -> Result<(), crate::Error> {
    let (Some(sender), Some(room), Some(event_id)) = (
        ev["sender"].as_str(),
        ev["room_id"].as_str(),
        ev["event_id"].as_str(),
    ) else {
        return Ok(());
    };
    // Loop B: what our puppets send came from Bens Chat in the first place.
    if st.mx.is_puppet(sender) {
        return Ok(());
    }
    let Some(chat) = st.db.chat_by_room(room).await? else {
        return Ok(());
    };
    if st.db.event_seen(event_id).await? {
        return Ok(());
    }
    let Some(text) = body_text(&ev["content"]) else {
        return Ok(());
    };
    let name = sender_name(st, sender, names).await;
    let id = st
        .bc
        .send(
            Some(chat.chat_id),
            json!({"text": format!("{name}: {text}")}),
        )
        .await?;
    st.db
        .record_message(id, chat.chat_id, Some(event_id))
        .await?;
    tracing::info!(chat = %chat.chat_id, event_id, "{} → bens chat", chat.network);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::body_text;
    use serde_json::json;

    #[test]
    fn bodies() {
        assert_eq!(
            body_text(&json!({"msgtype": "m.text", "body": "hi"})).as_deref(),
            Some("hi")
        );
        assert_eq!(
            body_text(&json!({"msgtype": "m.image", "body": "cat.jpg"})).as_deref(),
            Some("[image] cat.jpg")
        );
        assert!(
            body_text(&json!({"msgtype": "m.text", "body": "* x",
                "m.relates_to": {"rel_type": "m.replace"}}))
            .is_none()
        );
        assert!(body_text(&json!({})).is_none());
        assert!(body_text(&json!({"msgtype": "m.text", "body": "  "})).is_none());
    }
}
