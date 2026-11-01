use serde::{Deserialize, Serialize};

use crate::pairing;

pub const DEFAULT_SERVER_PORT: u16 = 49372;
pub const DISCOVERY_PORT: u16 = 49373;
pub const DEFAULT_HISTORY_LIMIT: usize = 200;
pub const MIN_HISTORY_LIMIT: usize = 20;
pub const MAX_HISTORY_LIMIT: usize = 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_role() -> String {
    "desktop".to_string()
}

impl Default for DeviceInfo {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: default_device_name(),
            platform: platform_name().to_string(),
            role: default_role(),
        }
    }
}

impl DeviceInfo {
    pub fn normalize(mut self) -> Self {
        if self.id.trim().is_empty() {
            self.id = uuid::Uuid::new_v4().to_string();
        }
        if self.name.trim().is_empty() {
            self.name = default_device_name();
        }
        if self.platform.trim().is_empty() {
            self.platform = platform_name().to_string();
        }
        if self.role.trim().is_empty() {
            self.role = default_role();
        }
        self
    }
}

pub fn default_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "ClipVault Desktop".to_string())
}

pub fn platform_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default = "default_true")]
    pub sync_enabled: bool,
    #[serde(default = "default_true")]
    pub launch_at_startup: bool,
    #[serde(default = "default_true")]
    pub minimize_to_tray: bool,
    #[serde(default = "default_history_limit")]
    pub history_limit: usize,
    #[serde(default = "default_server_port")]
    pub server_port: u16,
    #[serde(default = "default_theme")]
    pub theme: String,
}

fn default_true() -> bool {
    true
}

fn default_history_limit() -> usize {
    DEFAULT_HISTORY_LIMIT
}

fn default_server_port() -> u16 {
    DEFAULT_SERVER_PORT
}

fn default_theme() -> String {
    "hoj-light".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sync_enabled: true,
            launch_at_startup: true,
            minimize_to_tray: true,
            history_limit: DEFAULT_HISTORY_LIMIT,
            server_port: DEFAULT_SERVER_PORT,
            theme: default_theme(),
        }
    }
}

impl Settings {
    pub fn clamped(mut self) -> Self {
        self.history_limit = clamp_history_limit(self.history_limit);
        self.server_port = DEFAULT_SERVER_PORT;
        self
    }
}

pub fn clamp_history_limit(value: usize) -> usize {
    value.clamp(MIN_HISTORY_LIMIT, MAX_HISTORY_LIMIT)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub preview: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_base64: Option<String>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub source_device_id: String,
    #[serde(default)]
    pub source_device_name: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub direction: String,
}

impl HistoryEntry {
    pub fn is_image(&self) -> bool {
        self.mime_type == "image/png"
    }

    pub fn signature(&self) -> String {
        format!(
            "{}:{}",
            if self.is_image() { "image" } else { "text" },
            self.sha256
        )
    }

    pub fn kind_label(&self) -> &'static str {
        if self.is_image() {
            "图片"
        } else {
            "文本"
        }
    }

    pub fn short_preview(&self) -> String {
        if self.is_image() {
            "PNG".to_string()
        } else {
            self.preview.clone()
        }
    }

    /// Fills in the fields older clients may omit and rejects unusable payloads.
    pub fn normalize(mut self) -> Option<Self> {
        if self.mime_type.trim().is_empty() {
            self.mime_type = if self.text.is_some() {
                "text/plain".to_string()
            } else if self.image_base64.is_some() {
                "image/png".to_string()
            } else {
                return None;
            };
        }

        if self.mime_type == "text/plain" && self.text.as_deref().unwrap_or("").is_empty() {
            return None;
        }
        if self.mime_type == "image/png" && self.image_base64.as_deref().unwrap_or("").is_empty() {
            return None;
        }
        if self.mime_type != "text/plain" && self.mime_type != "image/png" {
            return None;
        }

        if self.id.trim().is_empty() {
            self.id = uuid::Uuid::new_v4().to_string();
        }
        if self.created_at <= 0 {
            self.created_at = pairing::now_ms();
        }
        if self.preview.trim().is_empty() {
            self.preview = if let Some(text) = &self.text {
                pairing::preview_text(text)
            } else {
                "PNG 图片".to_string()
            };
        }
        if self.source_device_id.trim().is_empty() {
            self.source_device_id = "unknown".to_string();
        }
        if self.source_device_name.trim().is_empty() {
            self.source_device_name = "未知设备".to_string();
        }
        if self.direction.trim().is_empty() {
            self.direction = "local".to_string();
        }
        if self.sha256.trim().is_empty() {
            self.sha256 = if let Some(text) = &self.text {
                pairing::sha256_hex(text.as_bytes())
            } else if let Some(image) = &self.image_base64 {
                pairing::sha256_hex(image.as_bytes())
            } else {
                String::new()
            };
        }

        Some(self)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingPayload {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub server_id: String,
    #[serde(default)]
    pub server_name: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub pairing_code: String,
    #[serde(default)]
    pub pairing_secret: String,
    #[serde(default)]
    pub issued_at: i64,
}

fn default_version() -> u32 {
    1
}

impl From<&RememberedPairing> for PairingPayload {
    fn from(value: &RememberedPairing) -> Self {
        Self {
            version: value.version,
            host: value.host.clone(),
            port: value.port,
            server_id: value.server_id.clone(),
            server_name: value.server_name.clone(),
            platform: value.platform.clone(),
            token: value.token.clone(),
            pairing_code: value.pairing_code.clone(),
            pairing_secret: value.pairing_secret.clone(),
            issued_at: pairing::now_ms(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RememberedPairing {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub server_id: String,
    #[serde(default)]
    pub server_name: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub pairing_code: String,
    #[serde(default)]
    pub pairing_secret: String,
    #[serde(default)]
    pub issued_at: i64,
    #[serde(default)]
    pub last_connected_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerIdentity {
    #[serde(default)]
    pub server_id: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub pairing_code: String,
}

impl Default for ServerIdentity {
    fn default() -> Self {
        Self {
            server_id: uuid::Uuid::new_v4().to_string(),
            token: uuid::Uuid::new_v4().to_string(),
            pairing_code: pairing::build_pairing_code(
                &pairing::local_address(),
                &pairing::new_secret(),
            ),
        }
    }
}

impl ServerIdentity {
    pub fn normalize(mut self, local_address: &str) -> Self {
        if self.server_id.trim().is_empty() {
            self.server_id = uuid::Uuid::new_v4().to_string();
        }
        if self.token.trim().is_empty() {
            self.token = uuid::Uuid::new_v4().to_string();
        }
        if self.pairing_code.trim().is_empty() {
            self.pairing_code = pairing::build_pairing_code(local_address, &pairing::new_secret());
        }
        self
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreDocument {
    #[serde(default)]
    pub device: DeviceInfo,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
    #[serde(default)]
    pub server_identity: ServerIdentity,
    #[serde(default)]
    pub remembered_pairings: Vec<RememberedPairing>,
    #[serde(default)]
    pub default_pairing_id: String,
}
