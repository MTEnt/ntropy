pub mod schema;

use async_sqlite::{Client, ClientBuilder, JournalMode};

pub struct Database {
    pub writer: Client,
    pub reader: Client,
}

impl Database {
    pub async fn open(path: &str) -> Result<Self, String> {
        let writer = ClientBuilder::new()
            .path(path)
            .journal_mode(JournalMode::Wal)
            .open()
            .await
            .map_err(|e| format!("Database writer failed to open at {}: {}", path, e))?;

        writer
            .conn(|conn| {
                conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
                Self::run_migrations(conn)?;
                Ok(())
            })
            .await
            .map_err(|e| format!("Database migration failed: {}", e))?;

        let reader = ClientBuilder::new()
            .path(path)
            .journal_mode(JournalMode::Wal)
            .open()
            .await
            .map_err(|e| format!("Database reader failed to open at {}: {}", path, e))?;

        reader
            .conn(|conn| {
                conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
                Ok(())
            })
            .await
            .map_err(|e| format!("Database reader pragma failed: {}", e))?;

        Ok(Self { writer, reader })
    }

    fn run_migrations(
        conn: &async_sqlite::rusqlite::Connection,
    ) -> Result<(), async_sqlite::rusqlite::Error> {
        conn.execute_batch(schema::MIGRATIONS_TABLE)?;
        let current_version: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_versions",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        for migration in schema::MIGRATIONS {
            if migration.version > current_version {
                let tx = conn.unchecked_transaction()?;
                tx.execute_batch(migration.sql)?;
                tx.execute(
                    "INSERT INTO schema_versions (version, description, applied_at) VALUES (?1, ?2, ?3)",
                    async_sqlite::rusqlite::params![
                        migration.version,
                        migration.description,
                        chrono::Utc::now().to_rfc3339()
                    ]
                )?;
                tx.commit()?;
            }
        }
        Ok(())
    }

    // --- Session CRUD Queries ---

    pub async fn get_all_sessions(&self) -> Result<Vec<(String, String, String, String)>, String> {
        self.reader
            .conn(|conn| {
                let mut stmt = conn.prepare("SELECT id, title, created_at, updated_at FROM sessions ORDER BY updated_at DESC")?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?;
                let mut res = Vec::new();
                for r in rows {
                    res.push(r?);
                }
                Ok(res)
            })
            .await
            .map_err(|e| format!("get_all_sessions failed: {}", e))
    }

    pub async fn create_session(&self, id: String, title: String) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        self.writer
            .conn(move |conn| {
                conn.execute(
                    "INSERT INTO sessions (id, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)
                     ON CONFLICT(id) DO UPDATE SET title = excluded.title, updated_at = excluded.updated_at",
                    async_sqlite::rusqlite::params![id, title, now],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| format!("create_session failed: {}", e))
    }

    pub async fn delete_session(&self, id: String) -> Result<(), String> {
        self.writer
            .conn(move |conn| {
                conn.execute(
                    "DELETE FROM sessions WHERE id = ?1",
                    async_sqlite::rusqlite::params![id],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| format!("delete_session failed: {}", e))
    }

    // --- Message CRUD Queries ---

    pub async fn get_session_messages(
        &self,
        session_id: String,
    ) -> Result<Vec<(String, String, String)>, String> {
        self.reader
            .conn(move |conn| {
                let mut stmt = conn.prepare("SELECT role, content, created_at FROM messages WHERE session_id = ?1 ORDER BY created_at ASC")?;
                let rows = stmt.query_map(async_sqlite::rusqlite::params![session_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?;
                let mut res = Vec::new();
                for r in rows {
                    res.push(r?);
                }
                Ok(res)
            })
            .await
            .map_err(|e| format!("get_session_messages failed: {}", e))
    }

    pub async fn add_session_message(
        &self,
        session_id: String,
        role: String,
        content: String,
    ) -> Result<i64, String> {
        let now = chrono::Utc::now().to_rfc3339();
        self.writer
            .conn(move |conn| {
                conn.execute(
                    "INSERT INTO messages (session_id, role, content, created_at) VALUES (?1, ?2, ?3, ?4)",
                    async_sqlite::rusqlite::params![session_id, role, content, now],
                )?;
                let id = conn.last_insert_rowid();
                // Update updated_at of the session
                conn.execute(
                    "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                    async_sqlite::rusqlite::params![now, session_id],
                )?;
                Ok(id)
            })
            .await
            .map_err(|e| format!("add_session_message failed: {}", e))
    }

    pub async fn update_session_message(&self, id: i64, content: String) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        self.writer
            .conn(move |conn| {
                let session_id: String = conn.query_row(
                    "SELECT session_id FROM messages WHERE id = ?1",
                    async_sqlite::rusqlite::params![id],
                    |row| row.get(0),
                )?;
                conn.execute(
                    "UPDATE messages SET content = ?1 WHERE id = ?2",
                    async_sqlite::rusqlite::params![content, id],
                )?;
                conn.execute(
                    "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                    async_sqlite::rusqlite::params![now, session_id],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| format!("update_session_message failed: {}", e))
    }

    // --- Task CRUD Queries ---

    pub async fn get_all_tasks(
        &self,
    ) -> Result<Vec<(String, String, String, String, String, String)>, String> {
        self.reader
            .conn(|conn| {
                let mut stmt = conn.prepare("SELECT id, session_id, text, status, created_at, updated_at FROM tasks ORDER BY created_at ASC")?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?;
                let mut res = Vec::new();
                for r in rows {
                    res.push(r?);
                }
                Ok(res)
            })
            .await
            .map_err(|e| format!("get_all_tasks failed: {}", e))
    }

    pub async fn create_task(
        &self,
        id: String,
        session_id: String,
        text: String,
    ) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        self.writer
            .conn(move |conn| {
                conn.execute(
                    "INSERT INTO tasks (id, session_id, text, status, created_at, updated_at) VALUES (?1, ?2, ?3, 'pending', ?4, ?4)",
                    async_sqlite::rusqlite::params![id, session_id, text, now],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| format!("create_task failed: {}", e))
    }

    pub async fn update_task_status(&self, id: String, status: String) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        self.writer
            .conn(move |conn| {
                conn.execute(
                    "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
                    async_sqlite::rusqlite::params![status, now, id],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| format!("update_task_status failed: {}", e))
    }

    pub async fn delete_task(&self, id: String) -> Result<(), String> {
        self.writer
            .conn(move |conn| {
                conn.execute(
                    "DELETE FROM tasks WHERE id = ?1",
                    async_sqlite::rusqlite::params![id],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| format!("delete_task failed: {}", e))
    }

    // --- Skills CRUD Queries ---

    pub async fn get_all_skills(
        &self,
    ) -> Result<Vec<(String, String, String, String, bool)>, String> {
        self.reader
            .conn(|conn| {
                let mut stmt = conn.prepare("SELECT name, description, trigger_tags, definition, enabled FROM skills ORDER BY name ASC")?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i32>(4)? != 0,
                    ))
                })?;
                let mut res = Vec::new();
                for r in rows {
                    res.push(r?);
                }
                Ok(res)
            })
            .await
            .map_err(|e| format!("get_all_skills failed: {}", e))
    }

    pub async fn create_or_update_skill(
        &self,
        name: String,
        description: String,
        trigger_tags: String,
        definition: String,
    ) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        self.writer
            .conn(move |conn| {
                conn.execute(
                    "INSERT INTO skills (name, description, trigger_tags, definition, enabled, created_at)
                     VALUES (?1, ?2, ?3, ?4, 1, ?5)
                     ON CONFLICT(name) DO UPDATE SET
                        description = excluded.description,
                        trigger_tags = excluded.trigger_tags,
                        definition = excluded.definition",
                    async_sqlite::rusqlite::params![name, description, trigger_tags, definition, now],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| format!("create_or_update_skill failed: {}", e))
    }

    pub async fn delete_skill(&self, name: String) -> Result<(), String> {
        self.writer
            .conn(move |conn| {
                conn.execute(
                    "DELETE FROM skills WHERE name = ?1",
                    async_sqlite::rusqlite::params![name],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| format!("delete_skill failed: {}", e))
    }
}
