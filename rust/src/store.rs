use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::models::StoreDocument;

/// Persistent store compatible with the Electron desktop build:
/// `%APPDATA%\ClipVault\clipvault\clipvault-store.json`
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
    path: PathBuf,
}

impl Store {
    pub fn discover() -> Result<Self> {
        let dir = default_data_dir()?.join("ClipVault").join("clipvault");
        Ok(Self::from_dir(dir))
    }

    pub fn from_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            path: dir.join("clipvault-store.json"),
            dir,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    pub fn load(&self) -> Result<StoreDocument> {
        match fs::read(&self.path) {
            Ok(raw) => match serde_json::from_slice::<StoreDocument>(&raw) {
                Ok(document) => Ok(document),
                Err(error) => {
                    let backup = self.path.with_extension("json.bad");
                    let _ = fs::rename(&self.path, &backup);
                    eprintln!(
                        "[警告] 数据文件解析失败，已备份到 {}：{error}",
                        backup.display()
                    );
                    Ok(StoreDocument::default())
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(StoreDocument::default())
            }
            Err(error) => Err(error).with_context(|| format!("读取 {} 失败", self.path.display())),
        }
    }

    /// Writes through a temp file so a crash mid-write cannot corrupt history.
    pub fn save(&self, document: &StoreDocument) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("创建数据目录 {} 失败", self.dir.display()))?;
        let json = serde_json::to_vec_pretty(document).context("序列化数据失败")?;
        let temp = self.path.with_extension("json.tmp");
        fs::write(&temp, &json).with_context(|| format!("写入 {} 失败", temp.display()))?;
        fs::rename(&temp, &self.path)
            .with_context(|| format!("替换 {} 失败", self.path.display()))?;
        Ok(())
    }
}

fn default_data_dir() -> Result<PathBuf> {
    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(appdata));
        }
        bail!("未找到 APPDATA 环境变量，无法确定数据目录。");
    }

    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(config));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home).join(".config"));
    }
    bail!("未找到 HOME 环境变量，无法确定数据目录。");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{HistoryEntry, Settings};

    #[test]
    fn missing_file_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::from_dir(dir.path());
        let document = store.load().unwrap();
        assert!(document.history.is_empty());
        assert!(document.settings.sync_enabled);
        assert!(!document.device.name.is_empty());
        assert!(!document.server_identity.token.is_empty());
    }

    #[test]
    fn round_trips_document() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::from_dir(dir.path());

        let mut document = StoreDocument::default();
        document.device.name = "Test-PC".to_string();
        document.settings = Settings {
            history_limit: 42,
            sync_enabled: false,
            ..Settings::default()
        };
        document.history.push(HistoryEntry {
            id: "entry-1".into(),
            mime_type: "text/plain".into(),
            preview: "hello".into(),
            text: Some("hello".into()),
            image_base64: None,
            created_at: 1,
            source_device_id: "device-1".into(),
            source_device_name: "Test-PC".into(),
            sha256: "abc".into(),
            direction: "local".into(),
        });

        store.save(&document).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(loaded.device.name, "Test-PC");
        assert_eq!(loaded.settings.history_limit, 42);
        assert!(!loaded.settings.sync_enabled);
        assert_eq!(loaded.history.len(), 1);
        assert_eq!(loaded.history[0].text.as_deref(), Some("hello"));
    }

    #[test]
    fn writes_electron_compatible_field_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::from_dir(dir.path());
        let document = StoreDocument::default();
        store.save(&document).unwrap();

        let raw = fs::read_to_string(store.path()).unwrap();
        assert!(raw.contains("\"serverIdentity\""));
        assert!(raw.contains("\"rememberedPairings\""));
        assert!(raw.contains("\"historyLimit\""));
        assert!(raw.contains("\"serverId\""));
    }

    #[test]
    fn corrupt_file_is_backed_up() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::from_dir(dir.path());
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(store.path(), b"{ not json").unwrap();

        let document = store.load().unwrap();
        assert!(document.history.is_empty());
        assert!(store.path().with_extension("json.bad").exists());
    }
}
