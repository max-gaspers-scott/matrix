//! Minimal Matrix client-server API client acting as the appservice
//! (masquerading as puppets via `?user_id=`).

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::Config;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
pub struct Matrix {
    http: reqwest::Client,
    base: String,
    as_token: String,
    server_name: String,
    prefix: String,
}

/// Percent-encode one URL path segment (room ids contain `!` and `:`).
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Map an arbitrary Bens Chat username to a valid, unique, lowercase Matrix
/// localpart fragment: `a-z0-9.-` kept, `A`→`_a`, `_`→`__`, anything else `=xx`.
pub fn escape_localpart(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        match c {
            'a'..='z' | '0'..='9' | '.' | '-' => out.push(c),
            'A'..='Z' => {
                out.push('_');
                out.push(c.to_ascii_lowercase());
            }
            '_' => out.push_str("__"),
            _ => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("={b:02x}"));
                }
            }
        }
    }
    out
}

#[derive(Deserialize)]
struct MatrixError {
    errcode: Option<String>,
    error: Option<String>,
}

impl Matrix {
    pub fn new(cfg: &Config, http: reqwest::Client) -> Self {
        Self {
            http,
            base: cfg.homeserver.trim_end_matches('/').to_owned(),
            as_token: cfg.as_token.clone(),
            server_name: cfg.server_name.clone(),
            prefix: cfg.puppet_prefix.clone(),
        }
    }

    pub fn puppet_localpart(&self, bc_user: &str) -> String {
        format!("{}{}", self.prefix, escape_localpart(bc_user))
    }

    pub fn puppet_id(&self, bc_user: &str) -> String {
        format!("@{}:{}", self.puppet_localpart(bc_user), self.server_name)
    }

    /// True for any user in our exclusive namespace (loop B guard).
    pub fn is_puppet(&self, user_id: &str) -> bool {
        user_id.starts_with(&format!("@{}", self.prefix))
            && user_id.ends_with(&format!(":{}", self.server_name))
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        as_user: Option<&str>,
        body: Option<Value>,
    ) -> Result<Value, Error> {
        let mut req = self
            .http
            .request(method, format!("{}{}", self.base, path))
            .bearer_auth(&self.as_token);
        if let Some(u) = as_user {
            req = req.query(&[("user_id", u)]);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await?;
        let status = res.status();
        let text = res.text().await?;
        if status.is_success() {
            Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
        } else {
            let e: MatrixError = serde_json::from_str(&text).unwrap_or(MatrixError {
                errcode: None,
                error: Some(text),
            });
            Err(format!(
                "{path}: {status} {} {}",
                e.errcode.unwrap_or_default(),
                e.error.unwrap_or_default()
            )
            .into())
        }
    }

    /// Register a puppet via the appservice; already-registered is fine.
    pub async fn ensure_registered(&self, localpart: &str) -> Result<(), Error> {
        let body = json!({
            "type": "m.login.application_service",
            "username": localpart,
            "inhibit_login": true,
        });
        match self
            .call(
                reqwest::Method::POST,
                "/_matrix/client/v3/register",
                None,
                Some(body),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("M_USER_IN_USE") => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn join(&self, room_id: &str, as_user: &str) -> Result<(), Error> {
        self.call(
            reqwest::Method::POST,
            &format!("/_matrix/client/v3/join/{}", enc(room_id)),
            Some(as_user),
            Some(json!({})),
        )
        .await
        .map(|_| ())
    }

    /// Send an `m.room.message`. `txn_id` makes retries idempotent.
    pub async fn send_message(
        &self,
        room_id: &str,
        as_user: &str,
        txn_id: &str,
        content: Value,
    ) -> Result<String, Error> {
        let v = self
            .call(
                reqwest::Method::PUT,
                &format!(
                    "/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
                    enc(room_id),
                    enc(txn_id)
                ),
                Some(as_user),
                Some(content),
            )
            .await?;
        v["event_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "send: no event_id in response".into())
    }

    pub async fn joined_rooms(&self, as_user: &str) -> Result<Vec<String>, Error> {
        let v = self
            .call(
                reqwest::Method::GET,
                "/_matrix/client/v3/joined_rooms",
                Some(as_user),
                None,
            )
            .await?;
        Ok(v["joined_rooms"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|r| r.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub async fn room_name(&self, room_id: &str, as_user: &str) -> Option<String> {
        self.call(
            reqwest::Method::GET,
            &format!(
                "/_matrix/client/v3/rooms/{}/state/m.room.name/",
                enc(room_id)
            ),
            Some(as_user),
            None,
        )
        .await
        .ok()?["name"]
            .as_str()
            .map(str::to_owned)
    }

    pub async fn displayname(&self, user_id: &str) -> Option<String> {
        self.call(
            reqwest::Method::GET,
            &format!("/_matrix/client/v3/profile/{}/displayname", enc(user_id)),
            None,
            None,
        )
        .await
        .ok()?["displayname"]
            .as_str()
            .map(str::to_owned)
    }
}
