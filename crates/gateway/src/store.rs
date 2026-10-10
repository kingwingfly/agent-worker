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

    pub fn approvals(&self) -> Result<Vec<Approval>> {
        let rows = self.texts("SELECT body AS v FROM approvals", Vec::new())?;
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
