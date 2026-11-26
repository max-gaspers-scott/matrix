//! Path A: Bens Chat → Matrix (→ WhatsApp / Google Messages).
//!
//! The `Bot` socket thread only pushes onto a channel; this single worker
//! drains it in order, so messages reach the portal room in the order they
//! were sent.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bens_chat_shared::Message;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use uuid::Uuid;

use crate::State;
use crate::db::Chat;

/// Don't refetch every bridged chat more often than this when a message with
/// an unknown parent shows up (most of those are just non-bridged chats).
const REFRESH_EVERY: Duration = Duration::from_secs(30);

pub async fn run(mut rx: UnboundedReceiver<Message>, st: Arc<State>) {
    let mut last_refresh: Option<Instant> = None;
    while let Some(msg) = rx.recv().await {
        if let Err(e) = handle(&st, &msg, &mut last_refresh).await {
            tracing::error!(message_id = %msg.message_id, "bens chat → matrix failed: {e}");
        }
    }
    // The shared crate's Bot thread died (it panics on bad payloads or a lost
    // connection). Exit so the supervisor restarts us with a fresh socket.
    tracing::error!("Bens Chat socket thread stopped; exiting");
    std::process::exit(1);
}

/// Load every bridged chat's message tree into the message→chat cache.
pub async fn preload(st: &State) {
    let chats = match st.db.chats().await {
        Ok(c) => c,
        Err(e) => return tracing::error!("loading chats: {e}"),
    };
    for chat in chats {
        match st.bc.chat_tree(chat.chat_id).await {
            Ok(msgs) => {
                for m in &msgs {
                    if let Err(e) = st.db.record_message(m.message_id, chat.chat_id, None).await {
                        tracing::error!("caching message: {e}");
                    }
                }
                tracing::info!(chat = %chat.chat_id, messages = msgs.len(), "cached chat");
            }
            Err(e) => tracing::warn!(chat = %chat.chat_id, "could not load chat tree: {e}"),
        }
    }
}

async fn resolve_chat(
    st: &State,
    msg: &Message,
    last_refresh: &mut Option<Instant>,
) -> Result<Option<Uuid>, crate::Error> {
    let Some(parent) = msg.parent_id else {
        // a root message is a chat of its own
        return Ok(st.db.chat_by_id(msg.message_id).await?.map(|c| c.chat_id));
    };
    if let Some(c) = st.db.chat_by_id(parent).await? {
        return Ok(Some(c.chat_id));
    }
    if let Some(c) = st.db.chat_of_message(parent).await? {
        return Ok(Some(c));
    }
    // Unknown parent: either a non-bridged chat, or we missed something.
    if last_refresh.is_none_or(|t| t.elapsed() >= REFRESH_EVERY) {
        *last_refresh = Some(Instant::now());
        preload(st).await;
        return Ok(st.db.chat_of_message(parent).await?);
    }
    Ok(None)
}

/// Bens Chat content → Matrix `m.room.message` content. `None` = not bridged.
fn to_matrix(msg: &Message, chat: &Chat) -> Option<Value> {
    let body = if let Some(t) = msg.content.get("text").and_then(Value::as_str) {
        t.to_owned()
    } else {
        let u = msg.content.get("url").and_then(Value::as_str)?;
        // images: sent as a link until media bridging (phase 5)
        u.to_owned()
    };
    if body.trim().is_empty() {
        return None;
    }
    // The portal is driven by the owner's puppet; anyone else in the Bens
    // Chat chat is attributed inline.
    let body = if msg.sender_name == chat.owner {
        body
    } else {
        format!("{}: {body}", msg.sender_name)
    };
    Some(json!({"msgtype": "m.text", "body": body}))
}

async fn handle(
    st: &State,
    msg: &Message,
    last_refresh: &mut Option<Instant>,
) -> Result<(), crate::Error> {
    let Some(chat_id) = resolve_chat(st, msg, last_refresh).await? else {
        tracing::debug!(message_id = %msg.message_id, "not in a bridged chat");
        return Ok(());
    };
    // Always cache, even for messages we skip: later replies may point at them.
    st.db.record_message(msg.message_id, chat_id, None).await?;

    // Loop A: our own posts (from Matrix) must not go back out.
    if msg.sender_name == st.bc.user() {
        return Ok(());
    }
    if st.db.message_bridged(msg.message_id).await? {
        return Ok(());
    }
    let Some(chat) = st.db.chat_by_id(chat_id).await? else {
        return Ok(());
    };
    let Some(content) = to_matrix(msg, &chat) else {
        tracing::debug!(message_id = %msg.message_id, "content type not bridged");
        return Ok(());
    };
    let puppet = st.mx.puppet_id(&chat.owner);
    // Bens Chat message id as txn id → a retry can't double-send.
    let event_id = st
        .mx
        .send_message(&chat.room_id, &puppet, &msg.message_id.to_string(), content)
        .await?;
    st.db
        .record_message(msg.message_id, chat_id, Some(&event_id))
        .await?;
    tracing::info!(chat = %chat_id, %event_id, "bens chat → {}", chat.network);
    Ok(())
}
