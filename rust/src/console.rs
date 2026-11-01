use chrono::{Local, TimeZone};

use crate::models::{HistoryEntry, PairingPayload, RememberedPairing, Settings};
use crate::service::PeerInfo;

pub fn format_time(milliseconds: i64) -> String {
    Local
        .timestamp_millis_opt(milliseconds)
        .single()
        .map(|value| value.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "-".to_string())
}

pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let shortened: String = text.chars().take(max_chars).collect();
    format!("{shortened}…")
}

pub fn print_pairing(payload: &PairingPayload, show_qr: bool, as_json: bool) {
    if as_json {
        if let Ok(raw) = serde_json::to_string_pretty(payload) {
            println!("{raw}");
        }
        return;
    }

    println!("[配对] 设备: {}", payload.server_name);
    println!("[配对] 地址: {}:{}", payload.host, payload.port);
    println!("[配对] 配对码: {}", payload.pairing_code);

    if show_qr {
        let raw = serde_json::to_string(payload).unwrap_or_default();
        println!("[配对] 用手机 ClipVault 扫描下方二维码，或手动输入 6 位配对码：");
        if let Err(error) = qr2term::print_qr(&raw) {
            eprintln!("[配对] 二维码渲染失败：{error}");
            println!("[配对] 二维码内容：{raw}");
        }
    } else {
        println!(
            "[配对] 二维码内容: {}",
            serde_json::to_string(payload).unwrap_or_default()
        );
    }
    println!();
}

pub fn print_history(entries: &[HistoryEntry], as_json: bool) {
    if as_json {
        if let Ok(raw) = serde_json::to_string_pretty(entries) {
            println!("{raw}");
        }
        return;
    }

    if entries.is_empty() {
        println!("（暂无历史记录）");
        return;
    }

    for (index, entry) in entries.iter().enumerate() {
        let direction = match entry.direction.as_str() {
            "inbound" => "接收",
            "outbound" => "发送",
            _ => "本地",
        };
        println!(
            "{:>3}. [{}] {} · {} · {} · {}",
            index + 1,
            format_time(entry.created_at),
            entry.kind_label(),
            entry.source_device_name,
            direction,
            truncate(&entry.short_preview(), 60)
        );
    }
}

pub fn print_peers(peers: &[PeerInfo]) {
    if peers.is_empty() {
        println!("（暂无已连接设备）");
        return;
    }

    for peer in peers {
        println!(
            "- {} ({}) {} · {} · 最后活动 {} · ID {}",
            peer.name,
            peer.platform,
            peer.host,
            peer.status,
            format_time(peer.last_seen),
            truncate(&peer.id, 8)
        );
    }
}

pub fn print_settings(settings: &Settings, data_dir: &std::path::Path, as_json: bool) {
    if as_json {
        let value = serde_json::json!({
            "syncEnabled": settings.sync_enabled,
            "historyLimit": settings.history_limit,
            "serverPort": settings.server_port,
            "launchAtStartup": settings.launch_at_startup,
            "minimizeToTray": settings.minimize_to_tray,
            "theme": settings.theme,
            "dataDir": data_dir.display().to_string(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return;
    }

    println!("数据目录: {}", data_dir.display());
    println!(
        "剪贴板同步: {}",
        if settings.sync_enabled {
            "开启"
        } else {
            "暂停"
        }
    );
    println!("历史上限: {}", settings.history_limit);
    println!("监听端口: {}（固定）", settings.server_port);
    println!(
        "开机自启: {}（仅保留字段，CLI 版本不注册自启）",
        if settings.launch_at_startup {
            "开启"
        } else {
            "关闭"
        }
    );
}

pub fn print_info(
    device_id: &str,
    device_name: &str,
    platform: &str,
    data_dir: &std::path::Path,
    pairing_code: &str,
    remembered: &[RememberedPairing],
    history_count: usize,
) {
    println!("数据目录: {}", data_dir.display());
    println!("设备名称: {device_name} ({platform})");
    println!("设备 ID: {device_id}");
    println!("当前配对码: {pairing_code}");
    println!("历史记录: {history_count} 条");
    if remembered.is_empty() {
        println!("已记忆配对: 无");
    } else {
        println!("已记忆配对:");
        for item in remembered {
            println!(
                "  - {} ({}) {}:{} · 上次连接 {}",
                item.server_name,
                item.platform,
                item.host,
                item.port,
                format_time(item.last_connected_at)
            );
        }
    }
}

pub fn print_help() {
    println!(
        r#"可用命令:
  status              查看运行状态与已连接设备
  qr                  重新打印本机二维码与配对码
  refresh             刷新令牌与配对码
  pair <码|JSON>      连接另一台设备（6 位配对码或二维码 JSON）
  peers               列出已连接设备
  history [n]         查看最近 n 条历史记录
  copy <序号|ID>      复制指定历史记录并发送给已连接设备
  clear               清空历史记录
  disconnect [ID|all] 断开指定设备或全部设备
  sync on|off         开启或暂停剪贴板同步
  limit <n>           设置历史上限（20-1000）
  help                显示本帮助
  quit                退出（等同 Ctrl+C）"#
    );
}

pub fn print_startup_hint(data_dir: &std::path::Path) {
    println!("[提示] 配对完成后，手机和电脑的文本/图片剪贴板会双向同步。");
    println!(
        "[提示] 配置与历史保存在：{} 下的 clipvault-store.json",
        data_dir.display()
    );
    println!("[提示] 输入 help 查看命令，Ctrl+C 退出。");
}
