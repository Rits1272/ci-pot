//! The local copy of the events.
//!
//! One SQLite table holds every Job Result that has been fetched, exactly as
//! it was signed, and only if its signature verifies. The ledger refers to
//! these events by id, so every charge can be traced back to the event behind it.

use std::path::Path;

use anyhow::{Context, Result};
use nostr::prelude::*;
use rusqlite::{params, Connection};

pub struct Store {
    database: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let database =
            Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        database.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                 id         TEXT PRIMARY KEY,
                 kind       INTEGER NOT NULL,
                 created_at INTEGER NOT NULL,
                 json       TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS events_by_kind_time ON events (kind, created_at);",
        )?;
        Ok(Store { database })
    }

    /// Keeps an event if its signature is valid and it is new. Returns whether it was kept.
    pub fn keep(&self, event: &Event) -> Result<bool> {
        if event.verify().is_err() {
            return Ok(false);
        }
        let added = self.database.execute(
            "INSERT OR IGNORE INTO events (id, kind, created_at, json) VALUES (?1, ?2, ?3, ?4)",
            params![
                event.id.to_hex(),
                event.kind.as_u16(),
                event.created_at.as_secs() as i64,
                event.as_json()
            ],
        )?;
        Ok(added == 1)
    }

    /// Every stored event of `kind` created at or after `since`.
    pub fn events(&self, kind: u16, since: u64) -> Result<Vec<Event>> {
        let mut query = self.database.prepare(
            "SELECT json FROM events WHERE kind = ?1 AND created_at >= ?2 ORDER BY created_at",
        )?;
        let rows = query.query_map(params![kind, since as i64], |row| row.get::<_, String>(0))?;
        let mut events = Vec::new();
        for json in rows {
            events.push(Event::from_json(json?).context("a stored event no longer parses")?);
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{fixture, JOB_RESULT};

    fn store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Store::open(&dir.path().join("events.sqlite")).unwrap(), dir)
    }

    #[test]
    fn an_event_is_kept_once() {
        let (store, _dir) = store();
        let event = fixture("job_native.json");
        assert!(store.keep(&event).unwrap());
        assert!(!store.keep(&event).unwrap());
        assert_eq!(store.events(JOB_RESULT, 0).unwrap(), vec![event]);
    }

    #[test]
    fn an_event_with_a_broken_signature_is_not_kept() {
        let (store, _dir) = store();
        let genuine = fixture("job_native.json").as_json();
        let forged = genuine.replace(r#"["conclusion","success"]"#, r#"["conclusion","failure"]"#);
        assert_ne!(forged, genuine);
        assert!(!store.keep(&Event::from_json(forged).unwrap()).unwrap());
        assert!(store.events(JOB_RESULT, 0).unwrap().is_empty());
    }

    #[test]
    fn events_before_the_cutoff_are_not_returned() {
        let (store, _dir) = store();
        let event = fixture("job_native.json");
        store.keep(&event).unwrap();
        let after = event.created_at.as_secs() + 1;
        assert!(store.events(JOB_RESULT, after).unwrap().is_empty());
    }
}
