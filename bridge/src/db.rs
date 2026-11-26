//! SQLite state. All ids are stored as TEXT.

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;
use uuid::Uuid;

#[derive(Clone)]
pub struct Db(SqlitePool);

#[derive(Clone, Debug)]
pub struct Chat {
    pub chat_id: Uuid,
    pub room_id: String,
    pub network: String,
    pub owner: String,
}

fn chat_from_row(r: sqlx::sqlite::SqliteRow) -> Option<Chat> {
    Some(Chat {
        chat_id: Uuid::parse_str(r.get::<&str, _>("chat_id")).ok()?,
        room_id: r.get("room_id"),
        network: r.get("network"),
        owner: r.get("owner"),
    })
}

impl Db {
    pub async fn open(url: &str) -> Result<Self, sqlx::Error> {
        if let Some(path) = url.strip_prefix("sqlite://")
            && let Some(dir) = std::path::Path::new(path).parent()
        {
            let _ = std::fs::create_dir_all(dir);
        }
        let opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self(pool))
    }

    // ------------------------------------------------------------ users
    pub async fn upsert_user(&self, bc_user: &str, matrix_user: &str) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO users (bc_user, matrix_user) VALUES (?, ?)
             ON CONFLICT(bc_user) DO UPDATE SET matrix_user = excluded.matrix_user",
        )
        .bind(bc_user)
        .bind(matrix_user)
        .execute(&self.0)
        .await?;
        Ok(())
    }

    pub async fn users(&self) -> sqlx::Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT bc_user, matrix_user FROM users")
            .fetch_all(&self.0)
            .await?;
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    // ------------------------------------------------------------ chats
    pub async fn insert_chat(&self, c: &Chat) -> sqlx::Result<()> {
        sqlx::query("INSERT INTO chats (chat_id, room_id, network, owner) VALUES (?, ?, ?, ?)")
            .bind(c.chat_id.to_string())
            .bind(&c.room_id)
            .bind(&c.network)
            .bind(&c.owner)
            .execute(&self.0)
            .await?;
        // the root message belongs to its own chat
        self.record_message(c.chat_id, c.chat_id, None).await
    }

    pub async fn delete_chat(&self, chat_id: Uuid) -> sqlx::Result<bool> {
        let id = chat_id.to_string();
        sqlx::query("DELETE FROM messages WHERE chat_id = ?")
            .bind(&id)
            .execute(&self.0)
            .await?;
        let r = sqlx::query("DELETE FROM chats WHERE chat_id = ?")
            .bind(&id)
            .execute(&self.0)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn chats(&self) -> sqlx::Result<Vec<Chat>> {
        let rows = sqlx::query("SELECT * FROM chats")
            .fetch_all(&self.0)
            .await?;
        Ok(rows.into_iter().filter_map(chat_from_row).collect())
    }

    pub async fn chat_by_id(&self, chat_id: Uuid) -> sqlx::Result<Option<Chat>> {
        let r = sqlx::query("SELECT * FROM chats WHERE chat_id = ?")
            .bind(chat_id.to_string())
            .fetch_optional(&self.0)
            .await?;
        Ok(r.and_then(chat_from_row))
    }

    pub async fn chat_by_room(&self, room_id: &str) -> sqlx::Result<Option<Chat>> {
        let r = sqlx::query("SELECT * FROM chats WHERE room_id = ?")
            .bind(room_id)
            .fetch_optional(&self.0)
            .await?;
        Ok(r.and_then(chat_from_row))
    }

    // ------------------------------------------------------------ messages
    /// Remember that `message_id` belongs to `chat_id` (and optionally which
    /// Matrix event it was bridged to/from). Never overwrites a known event id.
    pub async fn record_message(
        &self,
        message_id: Uuid,
        chat_id: Uuid,
        event_id: Option<&str>,
    ) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO messages (bc_message_id, chat_id, event_id) VALUES (?, ?, ?)
             ON CONFLICT(bc_message_id) DO UPDATE
               SET event_id = COALESCE(messages.event_id, excluded.event_id)",
        )
        .bind(message_id.to_string())
        .bind(chat_id.to_string())
        .bind(event_id)
        .execute(&self.0)
        .await?;
        Ok(())
    }

    pub async fn chat_of_message(&self, message_id: Uuid) -> sqlx::Result<Option<Uuid>> {
        let r: Option<String> =
            sqlx::query_scalar("SELECT chat_id FROM messages WHERE bc_message_id = ?")
                .bind(message_id.to_string())
                .fetch_optional(&self.0)
                .await?;
        Ok(r.and_then(|s| Uuid::parse_str(&s).ok()))
    }

    pub async fn message_bridged(&self, message_id: Uuid) -> sqlx::Result<bool> {
        let r: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM messages WHERE bc_message_id = ? AND event_id IS NOT NULL",
        )
        .bind(message_id.to_string())
        .fetch_optional(&self.0)
        .await?;
        Ok(r.is_some())
    }

    pub async fn event_seen(&self, event_id: &str) -> sqlx::Result<bool> {
        let r: Option<i64> = sqlx::query_scalar("SELECT 1 FROM messages WHERE event_id = ?")
            .bind(event_id)
            .fetch_optional(&self.0)
            .await?;
        Ok(r.is_some())
    }

    // ------------------------------------------------------------ txns
    /// Returns false if this transaction was already processed.
    pub async fn claim_txn(&self, txn_id: &str) -> sqlx::Result<bool> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let r = sqlx::query("INSERT OR IGNORE INTO txns (txn_id, received_at) VALUES (?, ?)")
            .bind(txn_id)
            .bind(now)
            .execute(&self.0)
            .await?;
        // keep the table small: homeservers only retry recent transactions
        sqlx::query("DELETE FROM txns WHERE received_at < ?")
            .bind(now - 7 * 24 * 3600)
            .execute(&self.0)
            .await?;
        Ok(r.rows_affected() == 1)
    }
}
