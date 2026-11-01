use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::models::{PairingPayload, RememberedPairing};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

pub fn preview_text(text: &str) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() > 96 {
        let truncated: String = flattened.chars().take(96).collect();
        format!("{truncated}...")
    } else {
        flattened
    }
}

/// First non-loopback IPv4 address, matching the Electron behaviour of picking
/// the first external IPv4 interface.
pub fn local_address() -> String {
    if let Ok(interfaces) = local_ip_address::list_afinet_netifas() {
        for (_name, address) in interfaces {
            let std::net::IpAddr::V4(ipv4) = address else {
                continue;
            };
            if !ipv4.is_loopback() && !ipv4.is_link_local() {
                return ipv4.to_string();
            }
        }
    }
    if let Ok(std::net::IpAddr::V4(ipv4)) = local_ip_address::local_ip() {
        return ipv4.to_string();
    }
    "127.0.0.1".to_string()
}

pub fn parse_ipv4(address: &str) -> Option<[u8; 4]> {
    let parts: Vec<u8> = address
        .trim()
        .split('.')
        .map(|part| part.parse::<u8>().ok())
        .collect::<Option<Vec<_>>>()?;
    if parts.len() != 4 {
        return None;
    }
    Some([parts[0], parts[1], parts[2], parts[3]])
}

pub fn last3(value: &str) -> String {
    let digits: String = value
        .chars()
        .filter(|character| character.is_ascii_digit())
        .collect();
    let tail: String = digits
        .chars()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{tail:0>3}")
}

pub fn new_secret() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let value = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % 1000;
    format!("{value:03}")
}

/// The 6-digit code is `<last IPv4 octet as 3 digits><3-digit secret>`.
pub fn build_pairing_code(address: &str, secret: &str) -> String {
    let host_code = parse_ipv4(address)
        .map(|parts| format!("{:03}", parts[3]))
        .unwrap_or_else(|| "000".to_string());
    format!("{host_code}{}", last3(secret))
}

pub fn resolve_host_from_pairing_code(local_address: &str, pairing_code: &str) -> Result<String> {
    let local = parse_ipv4(local_address)
        .ok_or_else(|| anyhow!("当前设备未连接到局域网，请先连接同一 Wi-Fi 或有线网络。"))?;

    let digits: String = pairing_code
        .chars()
        .filter(|character| character.is_ascii_digit())
        .collect();
    if digits.len() != 6 {
        bail!("配对码格式不正确，请输入有效的 6 位配对码。");
    }

    let suffix: u16 = digits[..3]
        .parse()
        .context("配对码格式不正确，请输入有效的 6 位配对码。")?;
    if suffix == 0 || suffix > 254 {
        bail!("配对码格式不正确，请输入有效的 6 位配对码。");
    }

    Ok(format!("{}.{}.{}.{}", local[0], local[1], local[2], suffix))
}

pub fn normalize_remembered(value: &Value) -> Option<RememberedPairing> {
    let host = value
        .get("host")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let port = value.get("port").and_then(Value::as_u64).unwrap_or(0);
    let server_id = value
        .get("serverId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let pairing_code = value
        .get("pairingCode")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let mut pairing_secret = value
        .get("pairingSecret")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    if pairing_secret.is_empty() && !pairing_code.is_empty() {
        pairing_secret = last3(&pairing_code);
    }

    if host.is_empty()
        || port == 0
        || port > u16::MAX as u64
        || server_id.is_empty()
        || (token.is_empty() && pairing_secret.is_empty())
    {
        return None;
    }

    Some(RememberedPairing {
        version: 1,
        host,
        port: port as u16,
        server_id,
        server_name: value
            .get("serverName")
            .and_then(Value::as_str)
            .unwrap_or("ClipVault 设备")
            .to_string(),
        platform: value
            .get("platform")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        token,
        pairing_code,
        pairing_secret,
        issued_at: value
            .get("issuedAt")
            .and_then(Value::as_i64)
            .unwrap_or_else(now_ms),
        last_connected_at: value
            .get("lastConnectedAt")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    })
}

/// Accepts either a 6-digit pairing code or the JSON payload behind the QR code.
pub fn parse_pairing_input(
    input: &str,
    local_address: &str,
    port: u16,
) -> Result<(PairingPayload, bool)> {
    let normalized = input.trim();
    if normalized.is_empty() {
        bail!("请输入配对码或二维码内容。");
    }

    let digits: String = normalized
        .chars()
        .filter(|character| character.is_ascii_digit())
        .collect();

    if digits.len() == normalized.chars().count() && digits.len() == 6 {
        let host = resolve_host_from_pairing_code(local_address, &digits)?;
        let payload = PairingPayload {
            version: 1,
            host,
            port,
            server_id: format!("pairing-{}", digits[..3].parse::<u8>().unwrap_or(0)),
            server_name: "ClipVault 设备".to_string(),
            platform: "unknown".to_string(),
            token: String::new(),
            pairing_code: digits.clone(),
            pairing_secret: last3(&digits),
            issued_at: now_ms(),
        };
        return Ok((payload, true));
    }

    let mut payload: PairingPayload = serde_json::from_str(normalized)
        .with_context(|| "请扫描本软件生成的二维码，或输入 6 位配对码。".to_string())?;

    if payload.pairing_secret.is_empty() {
        payload.pairing_secret = last3(&payload.pairing_code);
    }
    if payload.server_id.trim().is_empty() {
        payload.server_id = format!("pairing-{}", payload.host);
    }
    if payload.server_name.trim().is_empty() {
        payload.server_name = "ClipVault 设备".to_string();
    }
    if payload.platform.trim().is_empty() {
        payload.platform = "unknown".to_string();
    }
    if payload.port == 0 {
        payload.port = port;
    }
    if payload.host.trim().is_empty()
        || payload.port == 0
        || (payload.token.is_empty() && payload.pairing_secret.is_empty())
    {
        bail!("配对内容无效。");
    }

    Ok((payload, false))
}

pub fn is_self(
    payload: &PairingPayload,
    server_id: &str,
    local_address: &str,
    local_secret: &str,
) -> bool {
    payload.server_id == server_id
        || (payload.host == local_address && payload.pairing_secret == local_secret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_pairing_code_from_last_octet_and_secret() {
        assert_eq!(build_pairing_code("192.168.31.86", "314"), "086314");
        assert_eq!(build_pairing_code("10.0.0.5", "7"), "005007");
        assert_eq!(build_pairing_code("not-an-ip", "314"), "000314");
    }

    #[test]
    fn resolves_host_from_pairing_code() {
        assert_eq!(
            resolve_host_from_pairing_code("192.168.31.5", "086314").unwrap(),
            "192.168.31.86"
        );
    }

    #[test]
    fn rejects_bad_pairing_codes() {
        let local = "192.168.31.5";
        assert!(resolve_host_from_pairing_code(local, "123").is_err());
        assert!(resolve_host_from_pairing_code(local, "000314").is_err());
        assert!(resolve_host_from_pairing_code(local, "255314").is_err());
        assert!(resolve_host_from_pairing_code("not-an-ip", "086314").is_err());
    }

    #[test]
    fn preview_is_flattened_and_truncated() {
        assert_eq!(preview_text("  hello\n\n  world  "), "hello world");
        let long = "a".repeat(120);
        let preview = preview_text(&long);
        assert_eq!(preview.chars().count(), 99);
        assert!(preview.ends_with("..."));
    }

    #[test]
    fn normalizes_remembered_pairings() {
        let value = json!({
            "host": "192.168.31.86",
            "port": 49372,
            "serverId": "server-1",
            "serverName": "Office-PC",
            "platform": "windows",
            "token": "token-1",
            "pairingCode": "086314",
            "lastConnectedAt": 123,
        });

        let pairing = normalize_remembered(&value).unwrap();
        assert_eq!(pairing.pairing_secret, "314");
        assert_eq!(pairing.port, 49372);

        assert!(normalize_remembered(&json!({"host": "1.2.3.4"})).is_none());
    }

    #[test]
    fn parses_pairing_code_input() {
        let (payload, from_code) = parse_pairing_input("086314", "192.168.31.5", 49372).unwrap();
        assert!(from_code);
        assert_eq!(payload.host, "192.168.31.86");
        assert_eq!(payload.pairing_secret, "314");
    }

    #[test]
    fn parses_qr_json_input() {
        let input = serde_json::to_string(&json!({
            "version": 1,
            "host": "192.168.31.86",
            "port": 49372,
            "serverId": "server-1",
            "serverName": "Office-PC",
            "platform": "windows",
            "token": "token-1",
            "pairingCode": "086314",
            "issuedAt": 1,
        }))
        .unwrap();

        let (payload, from_code) = parse_pairing_input(&input, "192.168.31.5", 49372).unwrap();
        assert!(!from_code);
        assert_eq!(payload.pairing_secret, "314");
        assert_eq!(payload.server_name, "Office-PC");
    }

    #[test]
    fn detects_self_pairing() {
        let payload = PairingPayload {
            version: 1,
            host: "192.168.31.86".into(),
            port: 49372,
            server_id: "server-1".into(),
            server_name: "PC".into(),
            platform: "windows".into(),
            token: "token".into(),
            pairing_code: "086314".into(),
            pairing_secret: "314".into(),
            issued_at: 1,
        };
        assert!(is_self(&payload, "server-1", "192.168.31.5", "999"));
        assert!(is_self(&payload, "server-2", "192.168.31.86", "314"));
        assert!(!is_self(&payload, "server-2", "192.168.31.86", "999"));
    }
}
