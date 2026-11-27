use rusqlite::Connection;
use std::sync::{Arc, Mutex};

const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("../migrations/0001_init.sql")),
    ("0002_hardening", include_str!("../migrations/0002_hardening.sql")),
    ("0003_ui_auth", include_str!("../migrations/0003_ui_auth.sql")),
    ("0004_memory", include_str!("../migrations/0004_memory.sql")),
    ("0005_github", include_str!("../migrations/0005_github.sql")),
    ("0006_workspaces", include_str!("../migrations/0006_workspaces.sql")),
    ("0007_notes", include_str!("../migrations/0007_notes.sql")),
    (
        "0008_workspace_invites",
        include_str!("../migrations/0008_workspace_invites.sql"),
    ),
];

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    pub fn migrate(&self) -> rusqlite::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL
            );",
        )?;
        for (i, (name, sql)) in MIGRATIONS.iter().enumerate() {
            let version = (i + 1) as i64;
            let applied: i64 = conn.query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = ?1",
                [version],
                |r| r.get(0),
            )?;
            if applied == 0 {
                conn.execute_batch(sql)?;
                conn.execute(
                    "INSERT INTO schema_migrations (version, name) VALUES (?1, ?2)",
                    rusqlite::params![version, name],
                )?;
            }
        }
        Ok(())
    }

    pub fn with<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let mut conn = self.0.lock().unwrap();
        f(&mut conn)
    }

    pub fn try_with<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, crate::error::ApiError>,
    ) -> Result<T, crate::error::ApiError> {
        let mut conn = self.0.lock().unwrap();
        f(&mut conn)
    }
}
