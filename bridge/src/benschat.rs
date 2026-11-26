//! Bens Chat REST client for the bridge account. Sending goes through the
//! shared crate's `SendMessage::send`; the rest are endpoints the shared
//! crate doesn't wrap.

use bens_chat_shared::{Message, SendMessage};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::Config;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct BensChat {
    http: reqwest::Client,
    base: String,
    user: String,
    password: String,
    token: Mutex<Option<String>>,
}

#[derive(Deserialize)]
struct Envelope {
    status: String,
    payload: Option<Value>,
    error: Option<String>,
}

impl BensChat {
    pub fn new(cfg: &Config, http: reqwest::Client) -> Self {
        Self {
            http,
            base: cfg.bc_url.trim_end_matches('/').to_owned(),
            user: cfg.bc_user.clone(),
            password: cfg.bc_password.clone(),
            token: Mutex::new(None),
        }
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    async fn login(&self) -> Result<String, Error> {
        let env: Envelope = self
            .http
            .post(format!("{}/auth/login", self.base))
            .json(&json!({"username": self.user, "password": self.password}))
            .send()
            .await?
            .json()
            .await?;
        match (env.status.as_str(), env.payload) {
            ("success", Some(p)) => p["token"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "login: no token".into()),
            _ => Err(format!("login failed: {}", env.error.unwrap_or_default()).into()),
        }
    }

    async fn token(&self, refresh: bool) -> Result<String, Error> {
        let mut t = self.token.lock().await;
        if refresh || t.is_none() {
            *t = Some(self.login().await?);
        }
        Ok(t.clone().unwrap_or_default())
    }

    /// Run `f` with a JWT; on failure, log in again once and retry
    /// (covers expired tokens without having to know the expiry).
    async fn with_token<T, F, Fut>(&self, f: F) -> Result<T, Error>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<T, Error>>,
    {
        match f(self.token(false).await?).await {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::debug!("bens chat call failed ({e}), re-logging in and retrying");
                f(self.token(true).await?).await
            }
        }
    }

    /// Post a message; returns the new message id.
    pub async fn send(&self, parent_id: Option<Uuid>, content: Value) -> Result<Uuid, Error> {
        let url = format!("{}/messages", self.base);
        self.with_token(|token| {
            let msg = SendMessage {
                sender_name: self.user.clone(), // ignored by the server (uses the JWT user)
                parent_id,
                content: content.clone(),
            };
            let url = url.clone();
            async move { msg.send(token, url).await }
        })
        .await
    }

    /// The chat root plus its whole subtree.
    pub async fn chat_tree(&self, chat_id: Uuid) -> Result<Vec<Message>, Error> {
        let url = format!("{}/messages", self.base);
        let env: Envelope = self
            .with_token(|token| {
                let req = self
                    .http
                    .get(&url)
                    .query(&[("parent", chat_id.to_string())])
                    .bearer_auth(token);
                async move { Ok(req.send().await?.error_for_status()?.json().await?) }
            })
            .await?;
        match (env.status.as_str(), env.payload) {
            ("success", Some(p)) => Ok(serde_json::from_value(p)?),
            _ => Err(format!("GET /messages: {}", env.error.unwrap_or_default()).into()),
        }
    }

    pub async fn add_participant(&self, chat_id: Uuid, user_name: &str) -> Result<(), Error> {
        let url = format!("{}/user-chats", self.base);
        let v: Value = self
            .with_token(|token| {
                let req = self
                    .http
                    .post(&url)
                    .json(&json!({"chat_id": chat_id, "user_name": user_name}))
                    .bearer_auth(token);
                async move { Ok(req.send().await?.error_for_status()?.json().await?) }
            })
            .await?;
        if v["res"] == "success" {
            Ok(())
        } else {
            Err(format!("POST /user-chats: {}", v["res"]).into())
        }
    }
}
