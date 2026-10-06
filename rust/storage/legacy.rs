//! The one-time import of connector records from the legacy SQLite room history.

use std::fs;
use std::io;

use serde_json::{json, Map, Value};

use super::PrivateDir;

impl PrivateDir {
    pub(crate) fn import_legacy_connectors(&self) -> io::Result<Value> {
        let path = self.file("room-history.sqlite3");
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(json!({"connectors": {}}));
            }
            Err(error) => return Err(error),
            Ok(meta) if !meta.file_type().is_file() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "legacy SQLite path is not a file",
                ));
            }
            Ok(_) => {}
        }
        let sqlite_error = |error| io::Error::new(io::ErrorKind::InvalidData, error);
        let connection =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(sqlite_error)?;
        let mut query = connection
            .prepare("SELECT id, token_hash, host, created, last_seen, revoked FROM connectors")
            .map_err(sqlite_error)?;
        let rows = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(sqlite_error)?;
        let mut connectors = Map::new();
        for row in rows {
            let row = row.map_err(sqlite_error)?;
            connectors.insert(
                row.0,
                json!({"token_hash": row.1, "host": row.2,
                "created": row.3, "last_seen": row.4, "revoked": row.5}),
            );
        }
        let state = json!({"connectors": connectors});
        self.write_json("room-state.json", &state)?;
        Ok(state)
    }
}
