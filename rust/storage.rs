//! The single owner of private files and process locks.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use base64::Engine;
use rand::RngCore;
use serde_json::Value;
use sha2::Digest;
use tempfile::NamedTempFile;

#[derive(Clone, Debug)]
pub struct PrivateDir {
    path: PathBuf,
}

impl PrivateDir {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)?;
        let found = fs::symlink_metadata(path)?;
        if !found.file_type().is_dir()
            || found.uid() != rustix::process::getuid().as_raw()
            || found.permissions().mode() & 0o077 != 0
        {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "identity.unsafe-directory"));
        }
        Ok(Self { path: path.to_path_buf() })
    }

    pub fn open_for_report(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let found = fs::symlink_metadata(path)?;
        if !found.file_type().is_dir() || found.uid() != rustix::process::getuid().as_raw() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "identity.unsafe-directory"));
        }
        Ok(Self { path: path.to_path_buf() })
    }

    pub fn path(&self) -> &Path { &self.path }
    pub fn file(&self, name: &str) -> PathBuf { self.path.join(name) }

    pub fn read_json(&self, name: &str) -> io::Result<Option<Value>> {
        match fs::read(self.file(name)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn write_json(&self, name: &str, value: &Value) -> io::Result<()> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.write_private(name, &bytes)
    }

    pub fn write_private(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let mut staged = self.stage(bytes)?;
        staged.flush()?;
        staged.persist(self.file(name)).map_err(|error| error.error)?;
        File::open(&self.path)?.sync_all()
    }

    pub fn link_new(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let staged = self.stage(bytes)?;
        fs::hard_link(staged.path(), self.file(name))?;
        File::open(&self.path)?.sync_all()
    }

    fn stage(&self, bytes: &[u8]) -> io::Result<NamedTempFile> {
        let mut staged = NamedTempFile::new_in(&self.path)?;
        staged.as_file().set_permissions(Permissions::from_mode(0o600))?;
        staged.write_all(bytes)?;
        staged.as_file().sync_all()?;
        Ok(staged)
    }

    pub fn remove(&self, name: &str) -> io::Result<()> {
        match fs::remove_file(self.file(name)) {
            Ok(()) => File::open(&self.path)?.sync_all(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn lock(&self) -> io::Result<Option<File>> {
        let file = OpenOptions::new().read(true).write(true).create(true)
            .mode(0o600).custom_flags(libc::O_NOFOLLOW)
            .open(self.file("core.lock"))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn connector_credential(&self) -> io::Result<(String, String)> {
        let mut state = match self.read_json("room-state.json")? {
            Some(value) => value,
            None => self.import_legacy_connectors()?,
        };
        let saved = self.read_json("connector-credential.json").ok().flatten();
        if let Some(saved) = saved {
            if let (Some(id), Some(token)) = (saved.get("connector_id").and_then(Value::as_str), saved.get("token").and_then(Value::as_str)) {
                let expected = state.pointer(&format!("/connectors/{}", id.replace('~', "~0").replace('/', "~1")))
                    .and_then(|row| row.get("token_hash")).and_then(Value::as_str);
                let revoked = state.pointer(&format!("/connectors/{}", id.replace('~', "~0").replace('/', "~1")))
                    .and_then(|row| row.get("revoked")).is_some_and(|value|
                        value.as_bool().unwrap_or(false) || value.as_i64().unwrap_or_default() != 0);
                let hash = format!("{:x}", sha2::Sha256::digest(token.as_bytes()));
                if expected == Some(hash.as_str()) && !revoked {
                    return Ok((id.to_owned(), token.to_owned()));
                }
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let hash = format!("{:x}", sha2::Sha256::digest(token.as_bytes()));
        if !state.is_object() { state = serde_json::json!({"connectors": {}}); }
        let connectors = state.as_object_mut().expect("object").entry("connectors").or_insert_with(|| serde_json::json!({}));
        if !connectors.is_object() { *connectors = serde_json::json!({}); }
        let at = crate::control::devices::now();
        connectors.as_object_mut().expect("object").insert(id.clone(), serde_json::json!({
            "token_hash": hash, "created": at, "last_seen": at, "revoked": 0
        }));
        self.write_json("room-state.json", &state)?;
        self.write_json("connector-credential.json", &serde_json::json!({"connector_id": id, "token": token}))?;
        Ok((id, token))
    }

    fn import_legacy_connectors(&self) -> io::Result<Value> {
        let path = self.file("room-history.sqlite3");
        if !path.exists() { return Ok(serde_json::json!({"connectors": {}})); }
        let mut connectors = serde_json::Map::new();
        if let Ok(connection) = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
            if let Ok(mut query) = connection.prepare("SELECT id, token_hash, host, created, last_seen, revoked FROM connectors") {
                if let Ok(rows) = query.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?, row.get::<_, i64>(4)?, row.get::<_, i64>(5)?))
                }) {
                    for row in rows.flatten() {
                        connectors.insert(row.0, serde_json::json!({"token_hash": row.1, "host": row.2,
                            "created": row.3, "last_seen": row.4, "revoked": row.5}));
                    }
                }
            }
        }
        let state = serde_json::json!({"connectors": connectors});
        self.write_json("room-state.json", &state)?;
        Ok(state)
    }
}
