//! Stockage local SQLite : profils, catalogue en cache, favoris, réglages.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::xtream::{Category, Channel, Credentials};

#[derive(Debug, Clone)]
pub struct Profile {
    pub id: i64,
    pub name: String,
    pub server: String,
    pub username: String,
    pub password: String,
    pub color: i32,
    pub last_sync: i64,
}

impl Profile {
    pub fn credentials(&self) -> Credentials {
        Credentials {
            server: self.server.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
        }
    }
}

pub fn data_dir() -> PathBuf {
    directories::ProjectDirs::from("fr", "FlowTV", "Flow TV")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn cache_dir() -> PathBuf {
    directories::ProjectDirs::from("fr", "FlowTV", "Flow TV")
        .map(|d| d.cache_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("cache"))
}

pub fn db_path() -> PathBuf {
    data_dir().join("flowtv.db")
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS profiles (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 name TEXT NOT NULL,
                 server TEXT NOT NULL,
                 username TEXT NOT NULL,
                 password TEXT NOT NULL,
                 color INTEGER NOT NULL DEFAULT 0,
                 last_sync INTEGER NOT NULL DEFAULT 0,
                 created INTEGER NOT NULL DEFAULT (strftime('%s','now'))
             );
             CREATE TABLE IF NOT EXISTS categories (
                 profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                 category_id TEXT NOT NULL,
                 name TEXT NOT NULL,
                 position INTEGER NOT NULL,
                 PRIMARY KEY (profile_id, category_id)
             );
             CREATE TABLE IF NOT EXISTS channels (
                 profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                 stream_id INTEGER NOT NULL,
                 num INTEGER NOT NULL,
                 name TEXT NOT NULL,
                 logo TEXT NOT NULL,
                 category_id TEXT NOT NULL,
                 epg_id TEXT NOT NULL,
                 position INTEGER NOT NULL,
                 PRIMARY KEY (profile_id, stream_id)
             );
             CREATE TABLE IF NOT EXISTS favorites (
                 profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                 stream_id INTEGER NOT NULL,
                 added INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                 PRIMARY KEY (profile_id, stream_id)
             );
             CREATE TABLE IF NOT EXISTS settings (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        Ok(Self { conn })
    }

    // ---- Profils -------------------------------------------------------------

    pub fn profiles(&self) -> rusqlite::Result<Vec<Profile>> {
        let mut st = self.conn.prepare(
            "SELECT id, name, server, username, password, color, last_sync FROM profiles ORDER BY id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Profile {
                id: r.get(0)?,
                name: r.get(1)?,
                server: r.get(2)?,
                username: r.get(3)?,
                password: r.get(4)?,
                color: r.get(5)?,
                last_sync: r.get(6)?,
            })
        })?;
        rows.collect()
    }

    pub fn profile(&self, id: i64) -> rusqlite::Result<Option<Profile>> {
        Ok(self.profiles()?.into_iter().find(|p| p.id == id))
    }

    pub fn save_profile(&self, p: &Profile) -> rusqlite::Result<i64> {
        if p.id > 0 {
            let old = self.profile(p.id)?;
            self.conn.execute(
                "UPDATE profiles SET name=?2, server=?3, username=?4, password=?5, color=?6 WHERE id=?1",
                params![p.id, p.name, p.server, p.username, p.password, p.color],
            )?;
            // Si le compte a changé, le cache n'est plus valable.
            if let Some(old) = old {
                if old.server != p.server || old.username != p.username || old.password != p.password {
                    self.clear_catalog(p.id)?;
                }
            }
            Ok(p.id)
        } else {
            self.conn.execute(
                "INSERT INTO profiles (name, server, username, password, color) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![p.name, p.server, p.username, p.password, p.color],
            )?;
            Ok(self.conn.last_insert_rowid())
        }
    }

    pub fn delete_profile(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM profiles WHERE id=?1", [id])?;
        Ok(())
    }

    fn clear_catalog(&self, profile_id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM channels WHERE profile_id=?1", [profile_id])?;
        self.conn.execute("DELETE FROM categories WHERE profile_id=?1", [profile_id])?;
        self.conn.execute("UPDATE profiles SET last_sync=0 WHERE id=?1", [profile_id])?;
        Ok(())
    }

    // ---- Catalogue -----------------------------------------------------------

    pub fn categories(&self, profile_id: i64) -> rusqlite::Result<Vec<Category>> {
        let mut st = self
            .conn
            .prepare("SELECT category_id, name FROM categories WHERE profile_id=?1 ORDER BY position")?;
        let rows = st.query_map([profile_id], |r| Ok(Category { id: r.get(0)?, name: r.get(1)? }))?;
        rows.collect()
    }

    pub fn channels(&self, profile_id: i64) -> rusqlite::Result<Vec<Channel>> {
        let mut st = self.conn.prepare(
            "SELECT stream_id, num, name, logo, category_id, epg_id FROM channels
             WHERE profile_id=?1 ORDER BY position",
        )?;
        let rows = st.query_map([profile_id], |r| {
            Ok(Channel {
                stream_id: r.get(0)?,
                num: r.get(1)?,
                name: r.get(2)?,
                logo: r.get(3)?,
                category_id: r.get(4)?,
                epg_id: r.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn replace_catalog(
        &mut self,
        profile_id: i64,
        categories: &[Category],
        channels: &[Channel],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM categories WHERE profile_id=?1", [profile_id])?;
        tx.execute("DELETE FROM channels WHERE profile_id=?1", [profile_id])?;
        {
            let mut st = tx.prepare(
                "INSERT OR REPLACE INTO categories (profile_id, category_id, name, position) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (i, c) in categories.iter().enumerate() {
                st.execute(params![profile_id, c.id, c.name, i as i64])?;
            }
            let mut st = tx.prepare(
                "INSERT OR REPLACE INTO channels (profile_id, stream_id, num, name, logo, category_id, epg_id, position)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (i, c) in channels.iter().enumerate() {
                st.execute(params![
                    profile_id,
                    c.stream_id,
                    c.num,
                    c.name,
                    c.logo,
                    c.category_id,
                    c.epg_id,
                    i as i64
                ])?;
            }
        }
        tx.execute("UPDATE profiles SET last_sync=strftime('%s','now') WHERE id=?1", [profile_id])?;
        tx.commit()
    }

    // ---- Favoris -------------------------------------------------------------

    pub fn favorites(&self, profile_id: i64) -> rusqlite::Result<Vec<i64>> {
        let mut st =
            self.conn.prepare("SELECT stream_id FROM favorites WHERE profile_id=?1 ORDER BY added, rowid")?;
        let rows = st.query_map([profile_id], |r| r.get(0))?;
        rows.collect()
    }

    pub fn set_favorite(&self, profile_id: i64, stream_id: i64, fav: bool) -> rusqlite::Result<()> {
        if fav {
            self.conn.execute(
                "INSERT OR IGNORE INTO favorites (profile_id, stream_id) VALUES (?1, ?2)",
                params![profile_id, stream_id],
            )?;
        } else {
            self.conn.execute(
                "DELETE FROM favorites WHERE profile_id=?1 AND stream_id=?2",
                params![profile_id, stream_id],
            )?;
        }
        Ok(())
    }

    // ---- Réglages ------------------------------------------------------------

    pub fn setting(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) {
        let _ = self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let path = std::env::temp_dir().join(format!("flowtv-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut s = Store::open(&path).unwrap();
        let id = s
            .save_profile(&Profile {
                id: 0,
                name: "Salon".into(),
                server: "http://x".into(),
                username: "u".into(),
                password: "p".into(),
                color: 2,
                last_sync: 0,
            })
            .unwrap();
        let cats = vec![Category { id: "1".into(), name: "France".into() }];
        let chans = vec![Channel {
            stream_id: 42,
            num: 1,
            name: "TF1".into(),
            logo: String::new(),
            category_id: "1".into(),
            epg_id: String::new(),
        }];
        s.replace_catalog(id, &cats, &chans).unwrap();
        s.set_favorite(id, 42, true).unwrap();
        assert_eq!(s.channels(id).unwrap().len(), 1);
        assert_eq!(s.favorites(id).unwrap(), vec![42]);
        s.delete_profile(id).unwrap();
        assert!(s.channels(id).unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
