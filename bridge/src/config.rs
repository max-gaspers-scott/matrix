//! Configuration from environment variables (a `.env` in the working
//! directory or any parent is loaded first, so running from the repo picks up
//! the secrets written by `scripts/setup.sh`).

use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub struct Config {
    /// Homeserver client-server API base, e.g. `http://localhost:8008`.
    pub homeserver: String,
    /// Matrix server_name, e.g. `bridge.local`.
    pub server_name: String,
    /// We authenticate to the homeserver with this.
    pub as_token: String,
    /// The homeserver authenticates to us with this.
    pub hs_token: String,
    /// Where the appservice HTTP endpoint listens.
    pub listen: SocketAddr,
    /// SQLite URL for our own state.
    pub database_url: String,
    /// Bens Chat backend base URL.
    pub bc_url: String,
    /// Bens Chat account the bridge posts as (must be a participant of every bridged chat).
    pub bc_user: String,
    pub bc_password: String,
    /// Localpart prefix of our puppets (`@bc_alice:server`). Must match the registration regex.
    pub puppet_prefix: String,
    /// mautrix provisioning API bases.
    pub whatsapp_provision: String,
    pub gmessages_provision: String,
    pub provision_secret: String,
}

fn var(name: &str, default: Option<&str>) -> Result<String, String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => default
            .map(str::to_owned)
            .ok_or_else(|| format!("missing required env var {name}")),
    }
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let _ = dotenvy::dotenv();
        let listen = var("LISTEN_ADDR", Some("0.0.0.0:29400"))?;
        Ok(Self {
            homeserver: var("HOMESERVER_URL", Some("http://localhost:8008"))?,
            server_name: var("SERVER_NAME", Some("bridge.local"))?,
            as_token: var("BC_AS_TOKEN", None)?,
            hs_token: var("BC_HS_TOKEN", None)?,
            listen: listen
                .parse()
                .map_err(|e| format!("bad LISTEN_ADDR {listen}: {e}"))?,
            database_url: var("DATABASE_URL", Some("sqlite://data/bridge/bridge.db"))?,
            bc_url: var("BENS_CHAT_URL", Some("http://localhost:9821"))?,
            // Bot::no_responce still hard-codes t0/aaa (see PLAN.md blockers),
            // so the defaults match it until the shared crate reads env vars.
            bc_user: var("BENS_CHAT_USER", Some("t0"))?,
            bc_password: var("BENS_CHAT_PASSWORD", Some("aaa"))?,
            puppet_prefix: var("PUPPET_PREFIX", Some("bc_"))?,
            whatsapp_provision: var("WHATSAPP_PROVISION_URL", Some("http://localhost:29318"))?,
            gmessages_provision: var("GMESSAGES_PROVISION_URL", Some("http://localhost:29336"))?,
            provision_secret: var("PROVISION_SECRET", Some(""))?,
        })
    }

    pub fn provision_url(&self, network: &str) -> Option<&str> {
        match network {
            "whatsapp" => Some(&self.whatsapp_provision),
            "gmessages" => Some(&self.gmessages_provision),
            _ => None,
        }
    }
}

/// Networks we bridge, and how they're labelled in Bens Chat.
pub fn network_label(network: &str) -> &str {
    match network {
        "whatsapp" => "WhatsApp",
        "gmessages" => "Google Messages",
        other => other,
    }
}
