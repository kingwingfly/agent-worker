//! What a session keeps in its Durable Object's SQLite database.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use worker::{Result, SqlStorage, SqlStorageValue};

/// A permission prompt or clarifying question waiting for the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub request_id: String,
    pub turn_id: String,
    pub tool_name: String,
    pub input: Value,
    /// The Telegram message carrying the buttons.
    pub message_id: i64,
    /// When the request is answered with a denial on the user's behalf.
    pub deadline_ms: u64,
    /// Answers collected so far, for a set of clarifying questions.
    #[serde(default)]
    pub answers: Map<String, Value>,
}

/// A conversation of this chat that has been saved at least once, and so can be gone
/// back to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Conversation {
    /// Claude's session id.
    pub id: String,
    /// The message it began with.
    pub title: String,
    pub used_ms: u64,
    /// Its newest checkpoint: the conversation as it was left.
    pub head: u64,
}

/// A conversation and its files as they were when a turn ended. The archive is in R2;
/// this is what the session knows about it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Checkpoint {
    /// When it was saved, which is also what names it.
    pub at_ms: u64,
    /// The checkpoint the turn started from; zero for a conversation's first turn, which
    /// started from nothing. `None` if what it started from was not kept.
    pub parent: Option<u64>,
    /// The message the turn answered.
    pub text: String,
}

#[derive(Deserialize)]
struct TextRow {
    v: String,
}

#[derive(Deserialize)]
struct CountRow {
    n: u64,
}

pub struct Store {
    sql: SqlStorage,
}

impl Store {
    pub fn open(sql: SqlStorage) -> Result<Self> {
        for table in [
            "CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS approvals (request_id TEXT PRIMARY KEY, body TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS usage (day INTEGER PRIMARY KEY, tokens INTEGER NOT NULL)",
            "CREATE TABLE IF NOT EXISTS remembered (key TEXT PRIMARY KEY)",
            "CREATE TABLE IF NOT EXISTS conversations (id TEXT PRIMARY KEY, title TEXT NOT NULL, \
             used_ms INTEGER NOT NULL, head INTEGER NOT NULL)",
            "CREATE TABLE IF NOT EXISTS checkpoints (conversation TEXT NOT NULL, \
             at_ms INTEGER NOT NULL, parent INTEGER, text TEXT NOT NULL, \
             PRIMARY KEY (conversation, at_ms))",
        ] {
            sql.exec(table, None)?;
        }
        Ok(Self { sql })
    }

    fn texts(&self, query: &str, bindings: Vec<SqlStorageValue>) -> Result<Vec<String>> {
        let rows: Vec<TextRow> = self.sql.exec(query, bindings)?.to_array()?;
        Ok(rows.into_iter().map(|row| row.v).collect())
    }

    pub fn load<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let rows = self.texts("SELECT v FROM kv WHERE k = ?", vec![key.into()])?;
        Ok(rows
            .first()
            .and_then(|json| serde_json::from_str(json).ok()))
    }

    pub fn save<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let json = serde_json::to_string(value)?;
        self.sql.exec(
            "INSERT INTO kv (k, v) VALUES (?, ?) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            vec![key.into(), json.into()],
        )?;
        Ok(())
    }

    /// The prompts that wait for an answer, in the order they were asked in.
    pub fn approvals(&self) -> Result<Vec<Approval>> {
        let rows = self.texts("SELECT body AS v FROM approvals ORDER BY rowid", Vec::new())?;
        Ok(rows
            .iter()
            .filter_map(|json| serde_json::from_str(json).ok())
            .collect())
    }

    pub fn approval(&self, request_id: &str) -> Result<Option<Approval>> {
        let rows = self.texts(
            "SELECT body AS v FROM approvals WHERE request_id = ?",
            vec![request_id.into()],
        )?;
        Ok(rows
            .first()
            .and_then(|json| serde_json::from_str(json).ok()))
    }

    pub fn put_approval(&self, approval: &Approval) -> Result<()> {
        let json = serde_json::to_string(approval)?;
        self.sql.exec(
            "INSERT INTO approvals (request_id, body) VALUES (?, ?) \
             ON CONFLICT(request_id) DO UPDATE SET body = excluded.body",
            vec![approval.request_id.as_str().into(), json.into()],
        )?;
        Ok(())
    }

    pub fn delete_approval(&self, request_id: &str) -> Result<()> {
        self.sql.exec(
            "DELETE FROM approvals WHERE request_id = ?",
            vec![request_id.into()],
        )?;
        Ok(())
    }

    pub fn clear_approvals(&self) -> Result<()> {
        self.sql.exec("DELETE FROM approvals", None)?;
        Ok(())
    }

    /// Records an "Always" answer under its key, see [`crate::remember::key`].
    pub fn remember(&self, key: &str) -> Result<()> {
        self.sql.exec(
            "INSERT OR IGNORE INTO remembered (key) VALUES (?)",
            vec![key.into()],
        )?;
        Ok(())
    }

    pub fn is_remembered(&self, key: &str) -> Result<bool> {
        let rows = self.texts(
            "SELECT key AS v FROM remembered WHERE key = ?",
            vec![key.into()],
        )?;
        Ok(!rows.is_empty())
    }

    pub fn forget_all(&self) -> Result<()> {
        self.sql.exec("DELETE FROM remembered", None)?;
        Ok(())
    }

    /// Records that a turn of `conversation` was saved as `checkpoint`, which becomes
    /// its head. The first one also gives the conversation its title.
    pub fn put_checkpoint(&self, conversation: &str, checkpoint: &Checkpoint) -> Result<()> {
        let at_ms = checkpoint.at_ms as i64;
        let parent = match checkpoint.parent {
            Some(parent) => (parent as i64).into(),
            None => SqlStorageValue::Null,
        };
        self.sql.exec(
            "INSERT OR REPLACE INTO checkpoints (conversation, at_ms, parent, text) \
             VALUES (?, ?, ?, ?)",
            vec![
                conversation.into(),
                at_ms.into(),
                parent,
                checkpoint.text.as_str().into(),
            ],
        )?;
        self.sql.exec(
            "INSERT INTO conversations (id, title, used_ms, head) VALUES (?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET used_ms = excluded.used_ms, head = excluded.head",
            vec![
                conversation.into(),
                checkpoint.text.as_str().into(),
                at_ms.into(),
                at_ms.into(),
            ],
        )?;
        Ok(())
    }

    /// The checkpoints kept for a conversation, newest first.
    pub fn checkpoints(&self, conversation: &str) -> Result<Vec<Checkpoint>> {
        self.sql
            .exec(
                "SELECT at_ms, parent, text FROM checkpoints WHERE conversation = ? \
                 ORDER BY at_ms DESC",
                vec![conversation.into()],
            )?
            .to_array()
    }

    pub fn delete_checkpoint(&self, conversation: &str, at_ms: u64) -> Result<()> {
        self.sql.exec(
            "DELETE FROM checkpoints WHERE conversation = ? AND at_ms = ?",
            vec![conversation.into(), (at_ms as i64).into()],
        )?;
        Ok(())
    }

    /// The saved conversations, the one used last first.
    pub fn conversations(&self) -> Result<Vec<Conversation>> {
        self.sql
            .exec(
                "SELECT id, title, used_ms, head FROM conversations ORDER BY used_ms DESC",
                None,
            )?
            .to_array()
    }

    pub fn conversation(&self, id: &str) -> Result<Option<Conversation>> {
        let rows: Vec<Conversation> = self
            .sql
            .exec(
                "SELECT id, title, used_ms, head FROM conversations WHERE id = ?",
                vec![id.into()],
            )?
            .to_array()?;
        Ok(rows.into_iter().next())
    }

    /// Moves a conversation's head, to an older checkpoint or to the one used just now.
    pub fn set_head(&self, id: &str, head: u64, used_ms: u64) -> Result<()> {
        self.sql.exec(
            "UPDATE conversations SET head = ?, used_ms = ? WHERE id = ?",
            vec![(head as i64).into(), (used_ms as i64).into(), id.into()],
        )?;
        Ok(())
    }

    /// Forgets a conversation and its checkpoints. The archives are the caller's to
    /// delete.
    pub fn delete_conversation(&self, id: &str) -> Result<()> {
        self.sql.exec(
            "DELETE FROM checkpoints WHERE conversation = ?",
            vec![id.into()],
        )?;
        self.sql
            .exec("DELETE FROM conversations WHERE id = ?", vec![id.into()])?;
        Ok(())
    }

    pub fn add_usage(&self, day: u64, tokens: u64) -> Result<()> {
        self.sql.exec(
            "INSERT INTO usage (day, tokens) VALUES (?, ?) \
             ON CONFLICT(day) DO UPDATE SET tokens = tokens + excluded.tokens",
            vec![(day as i64).into(), (tokens as i64).into()],
        )?;
        Ok(())
    }

    pub fn usage(&self, day: u64) -> Result<u64> {
        let rows: Vec<CountRow> = self
            .sql
            .exec(
                "SELECT tokens AS n FROM usage WHERE day = ?",
                vec![(day as i64).into()],
            )?
            .to_array()?;
        Ok(rows.first().map_or(0, |row| row.n))
    }
}
