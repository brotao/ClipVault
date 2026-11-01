use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;

use crate::clipboard;
use crate::models::*;
use crate::pairing;
use crate::protocol;
use crate::store::Store;

const MAX_RECENT_ROUTES: usize = 512;
const ROUTE_TRIM: usize = 128;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(15);

struct PeerHandle {
    conn_id: String,
    name: String,
    platform: String,
    host: String,
    status: String,
    last_seen: i64,
    tx: mpsc::UnboundedSender<Value>,
}

#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub host: String,
    pub status: String,
    pub last_seen: i64,
}

struct Data {
    device: DeviceInfo,
    settings: Settings,
    identity: ServerIdentity,
    history: Vec<HistoryEntry>,
    peers: HashMap<String, PeerHandle>,
    remembered: HashMap<String, RememberedPairing>,
    default_pairing_id: String,
    last_signature: String,
    suppressed_signature: String,
    recent_routes: HashMap<String, i64>,
}

impl Data {
    fn to_document(&self) -> StoreDocument {
        let mut remembered: Vec<RememberedPairing> = self.remembered.values().cloned().collect();
        remembered.sort_by_key(|item| std::cmp::Reverse(item.last_connected_at));

        StoreDocument {
            device: self.device.clone(),
            settings: self.settings.clone(),
            history: self
                .history
                .iter()
                .take(self.settings.history_limit)
                .cloned()
                .collect(),
            server_identity: self.identity.clone(),
            remembered_pairings: remembered,
            default_pairing_id: self.default_pairing_id.clone(),
        }
    }

    fn peer_infos(&self) -> Vec<PeerInfo> {
        let mut peers: Vec<PeerInfo> = self
            .peers
            .iter()
            .map(|(id, peer)| PeerInfo {
                id: id.clone(),
                name: peer.name.clone(),
                platform: peer.platform.clone(),
                host: peer.host.clone(),
                status: peer.status.clone(),
                last_seen: peer.last_seen,
            })
            .collect();
        peers.sort_by_key(|peer| std::cmp::Reverse(peer.last_seen));
        peers
    }
}

pub struct Service {
    data: Mutex<Data>,
    store: Store,
    connecting: AtomicBool,
}

impl Service {
    pub fn new(store: Store) -> Result<Self> {
        let document = store.load()?;
        let needs_initial_save = !store.exists()
            || document.server_identity.server_id.trim().is_empty()
            || document.server_identity.token.trim().is_empty()
            || document.server_identity.pairing_code.trim().is_empty();
        let local_address = pairing::local_address();
        let settings = document.settings.clamped();
        let device = document.device.normalize();
        let identity = document.server_identity.normalize(&local_address);

        let mut history = document.history;
        for entry in &mut history {
            if entry.direction.trim().is_empty() {
                entry.direction = "local".to_string();
            }
        }
        history.truncate(settings.history_limit);

        let mut remembered = HashMap::new();
        for item in &document.remembered_pairings {
            let value = serde_json::to_value(item).unwrap_or(Value::Null);
            if let Some(pairing) = pairing::normalize_remembered(&value) {
                remembered.insert(pairing.server_id.clone(), pairing);
            }
        }

        let default_pairing_id = if remembered.contains_key(&document.default_pairing_id) {
            document.default_pairing_id
        } else {
            remembered.keys().next().cloned().unwrap_or_default()
        };

        let data = Data {
            device,
            settings,
            identity,
            history,
            peers: HashMap::new(),
            remembered,
            default_pairing_id,
            last_signature: String::new(),
            suppressed_signature: String::new(),
            recent_routes: HashMap::new(),
        };

        // Persist immediately on first run so the device id, token and pairing
        // secret stay stable across restarts (the Electron build does the same).
        // Never rewrite an existing store here: a second instance that fails to
        // bind must not clobber the running instance's history.
        if needs_initial_save {
            store.save(&data.to_document())?;
        }

        Ok(Self {
            data: Mutex::new(data),
            store,
            connecting: AtomicBool::new(false),
        })
    }

    pub fn store_dir(&self) -> &Path {
        self.store.dir()
    }

    pub async fn device_name(&self) -> String {
        self.data.lock().await.device.name.clone()
    }

    pub async fn port(&self) -> u16 {
        self.data.lock().await.settings.server_port
    }

    fn payload_from(&self, data: &Data) -> PairingPayload {
        let host = pairing::local_address();
        let secret = pairing::last3(&data.identity.pairing_code);
        PairingPayload {
            version: 1,
            host: host.clone(),
            port: data.settings.server_port,
            server_id: data.identity.server_id.clone(),
            server_name: data.device.name.clone(),
            platform: data.device.platform.clone(),
            token: data.identity.token.clone(),
            pairing_code: pairing::build_pairing_code(&host, &secret),
            pairing_secret: secret,
            issued_at: pairing::now_ms(),
        }
    }

    pub async fn pairing_payload(&self) -> PairingPayload {
        let data = self.data.lock().await;
        self.payload_from(&data)
    }

    pub async fn start_network(self: &Arc<Self>) -> Result<()> {
        let port = self.port().await;
        self.bind(port).await.map(|_| ())
    }

    /// Binds the TCP listener and starts accepting peers. Exposed with an
    /// explicit port so tests can use an ephemeral one.
    pub async fn bind(self: &Arc<Self>, port: u16) -> Result<std::net::SocketAddr> {
        let listener = match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                bail!("端口 {port} 已被占用，请关闭另一个 ClipVault 或占用该端口的程序后重试。");
            }
            Err(error) => {
                return Err(error).with_context(|| format!("监听 0.0.0.0:{port} 失败"));
            }
        };
        let local = listener.local_addr().context("获取监听地址失败")?;

        println!("[服务] 正在监听 0.0.0.0:{}", local.port());
        let service = self.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote)) => {
                        tracing::debug!("收到来自 {remote} 的连接");
                        let service = service.clone();
                        tokio::spawn(async move {
                            service.handle_inbound(stream).await;
                        });
                    }
                    Err(error) => tracing::warn!("接受连接失败：{error}"),
                }
            }
        });

        Ok(local)
    }

    pub fn spawn_discovery(self: &Arc<Self>) {
        let service = self.clone();
        tokio::spawn(async move {
            let socket = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)).await {
                Ok(socket) => socket,
                Err(error) => {
                    eprintln!("[警告] UDP 发现端口 {DISCOVERY_PORT} 不可用：{error}");
                    return;
                }
            };

            let mut buffer = [0u8; 4096];
            loop {
                let Ok((size, remote)) = socket.recv_from(&mut buffer).await else {
                    continue;
                };
                let Ok(request) = serde_json::from_slice::<Value>(&buffer[..size]) else {
                    continue;
                };
                if request.get("type").and_then(Value::as_str) != Some("clipvault_pairing_lookup") {
                    continue;
                }
                let code = request
                    .get("pairingCode")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let payload = {
                    let data = service.data.lock().await;
                    service.payload_from(&data)
                };
                if code != payload.pairing_code {
                    continue;
                }
                let response = json!({
                    "type": "clipvault_pairing_offer",
                    "payload": payload,
                });
                if let Ok(bytes) = serde_json::to_vec(&response) {
                    let _ = socket.send_to(&bytes, remote).await;
                }
            }
        });
    }

    pub fn spawn_clipboard(self: &Arc<Self>) {
        let (sender, mut receiver) = mpsc::unbounded_channel::<()>();
        let initial_nudge = sender.clone();
        if let Err(error) = clipboard::spawn_watcher(sender) {
            eprintln!("[警告] 剪贴板监听启动失败：{error}");
            return;
        }

        let service = self.clone();
        tokio::spawn(async move {
            // Electron records whatever is already on the clipboard at startup.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = initial_nudge.send(());

            while receiver.recv().await.is_some() {
                // Windows often posts several updates for one copy; settle first.
                tokio::time::sleep(Duration::from_millis(120)).await;
                while receiver.try_recv().is_ok() {}

                let (enabled, has_peers, device) = {
                    let data = service.data.lock().await;
                    (
                        data.settings.sync_enabled,
                        !data.peers.is_empty(),
                        data.device.clone(),
                    )
                };
                if !enabled {
                    continue;
                }

                match tokio::task::spawn_blocking(move || {
                    clipboard::read_clipboard(&device, has_peers)
                })
                .await
                {
                    Ok(Ok(Some((signature, entry)))) => {
                        service.handle_local_clipboard(signature, entry).await;
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => tracing::debug!("读取剪贴板失败：{error}"),
                    Err(error) => tracing::debug!("剪贴板任务异常：{error}"),
                }
            }
        });
    }

    pub fn spawn_reconnect(self: &Arc<Self>) {
        let service = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            loop {
                let should_try = {
                    let data = service.data.lock().await;
                    data.peers.is_empty()
                        && !data.default_pairing_id.is_empty()
                        && data.remembered.contains_key(&data.default_pairing_id)
                };

                if should_try && !service.connecting.swap(true, Ordering::SeqCst) {
                    let target = {
                        let data = service.data.lock().await;
                        data.remembered.get(&data.default_pairing_id).cloned()
                    };
                    if let Some(target) = target {
                        let payload = PairingPayload::from(&target);
                        if let Err(error) = service.connect_pair(payload, false, true).await {
                            tracing::debug!("自动重连失败：{error}");
                        }
                    }
                    service.connecting.store(false, Ordering::SeqCst);
                }

                tokio::time::sleep(RECONNECT_INTERVAL).await;
            }
        });
    }

    pub async fn status_text(&self) -> String {
        let data = self.data.lock().await;
        let payload = self.payload_from(&data);
        let state = if data.peers.is_empty() {
            "在线 · 等待新设备连接"
        } else {
            "在线 · 同步中"
        };

        let mut output = String::new();
        output.push_str(&format!("状态: {state}\n"));
        output.push_str(&format!("监听: 0.0.0.0:{}\n", data.settings.server_port));
        output.push_str(&format!("本机地址: {}\n", payload.host));
        output.push_str(&format!("配对码: {}\n", payload.pairing_code));
        output.push_str(&format!(
            "设备名称: {} ({})\n",
            data.device.name, data.device.platform
        ));
        output.push_str(&format!(
            "剪贴板同步: {}\n",
            if data.settings.sync_enabled {
                "开启"
            } else {
                "暂停"
            }
        ));
        output.push_str(&format!(
            "历史记录: {} 条 / 上限 {}\n",
            data.history.len(),
            data.settings.history_limit
        ));

        let peers = data.peer_infos();
        if peers.is_empty() {
            output.push_str("已连接设备: 无\n");
        } else {
            output.push_str("已连接设备:\n");
            for peer in peers {
                output.push_str(&format!(
                    "  - {} ({}) {} · {}\n",
                    peer.name, peer.platform, peer.host, peer.status
                ));
            }
        }
        output
    }

    pub async fn peers_snapshot(&self) -> Vec<PeerInfo> {
        self.data.lock().await.peer_infos()
    }

    pub async fn history_snapshot(&self, limit: Option<usize>) -> Vec<HistoryEntry> {
        let data = self.data.lock().await;
        let limit = limit.unwrap_or(data.history.len());
        data.history.iter().take(limit).cloned().collect()
    }

    pub async fn clear_history(&self) -> usize {
        let removed = {
            let mut data = self.data.lock().await;
            let removed = data.history.len();
            data.history.clear();
            removed
        };
        self.persist().await;
        removed
    }

    pub async fn set_sync_enabled(&self, enabled: bool) {
        {
            let mut data = self.data.lock().await;
            data.settings.sync_enabled = enabled;
        }
        self.persist().await;
    }

    pub async fn set_history_limit(&self, limit: usize) -> usize {
        let applied = {
            let mut data = self.data.lock().await;
            data.settings.history_limit = clamp_history_limit(limit);
            let applied = data.settings.history_limit;
            data.history.truncate(applied);
            applied
        };
        self.persist().await;
        applied
    }

    pub async fn refresh_pairing(&self) -> PairingPayload {
        let payload = {
            let mut data = self.data.lock().await;
            data.identity.token = uuid::Uuid::new_v4().to_string();
            data.identity.pairing_code =
                pairing::build_pairing_code(&pairing::local_address(), &pairing::new_secret());
            self.payload_from(&data)
        };
        self.persist().await;
        payload
    }

    pub async fn disconnect(&self, selector: &str) -> usize {
        let removed = {
            let mut data = self.data.lock().await;
            let ids: Vec<String> = if selector == "all" || selector == "*" {
                data.peers.keys().cloned().collect()
            } else if data.peers.contains_key(selector) {
                vec![selector.to_string()]
            } else {
                data.peers
                    .iter()
                    .filter(|(id, peer)| {
                        id.starts_with(selector)
                            || peer.name.eq_ignore_ascii_case(selector)
                            || peer.host.starts_with(selector)
                    })
                    .map(|(id, _)| id.clone())
                    .collect()
            };
            let count = ids.len();
            for id in ids {
                data.peers.remove(&id);
            }
            count
        };
        self.persist().await;
        removed
    }

    pub async fn copy_entry(&self, selector: &str) -> Result<String> {
        let copied = {
            let data = self.data.lock().await;
            let entry = if let Ok(index) = selector.parse::<usize>() {
                index
                    .checked_sub(1)
                    .and_then(|index| data.history.get(index))
                    .cloned()
            } else {
                data.history
                    .iter()
                    .find(|entry| entry.id == selector)
                    .cloned()
            };

            let Some(entry) = entry else {
                bail!("没有找到该记录，先用 `history` 查看序号。");
            };

            HistoryEntry {
                id: uuid::Uuid::new_v4().to_string(),
                created_at: pairing::now_ms(),
                source_device_id: data.device.id.clone(),
                source_device_name: data.device.name.clone(),
                direction: "outbound".to_string(),
                ..entry
            }
        };

        let preview = copied.short_preview();
        self.publish(copied, uuid::Uuid::new_v4().to_string(), None, true)
            .await?;
        Ok(preview)
    }

    pub async fn connect_input(self: &Arc<Self>, input: &str, quiet: bool) -> Result<()> {
        let local_address = pairing::local_address();
        let port = self.port().await;
        let (payload, from_code) = pairing::parse_pairing_input(input, &local_address, port)?;
        self.connect_pair(payload, from_code, quiet).await
    }

    async fn connect_pair(
        self: &Arc<Self>,
        payload: PairingPayload,
        from_code: bool,
        quiet: bool,
    ) -> Result<()> {
        let (own_id, own_secret, own_device, own_payload) = {
            let data = self.data.lock().await;
            (
                data.identity.server_id.clone(),
                pairing::last3(&data.identity.pairing_code),
                data.device.clone(),
                self.payload_from(&data),
            )
        };

        if pairing::is_self(&payload, &own_id, &pairing::local_address(), &own_secret) {
            bail!("不能连接本机生成的配对码。");
        }

        let connect_timeout = if from_code {
            Duration::from_millis(2500)
        } else {
            Duration::from_millis(6000)
        };
        let handshake_timeout = if from_code {
            Duration::from_millis(3000)
        } else {
            Duration::from_millis(6500)
        };

        if !quiet {
            println!(
                "[连接] 正在连接 {} ({}:{})…",
                payload.server_name, payload.host, payload.port
            );
        }

        let stream = timeout(
            connect_timeout,
            TcpStream::connect((payload.host.as_str(), payload.port)),
        )
        .await
        .map_err(|_| anyhow!("连接超时，请确认两台设备在同一局域网，且对方 ClipVault 正在运行。"))?
        .with_context(|| format!("连接 {}:{} 失败", payload.host, payload.port))?;
        let _ = stream.set_nodelay(true);

        let (mut read_half, mut write_half) = stream.into_split();

        let mut hello = json!({
            "type": "hello",
            "device": {
                "id": own_device.id,
                "name": own_device.name,
                "platform": own_device.platform,
                "role": own_device.role,
            },
            "pairingPayload": own_payload,
        });
        if !payload.token.is_empty() {
            hello["token"] = json!(payload.token);
        }
        if !payload.pairing_secret.is_empty() {
            hello["pairingSecret"] = json!(payload.pairing_secret);
        }
        protocol::write_frame(&mut write_half, &hello).await?;

        let mut buffer = Vec::new();
        let deadline = tokio::time::Instant::now() + handshake_timeout;
        let welcome = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!("配对码错误，或目标设备没有确认连接。");
            }
            match timeout(remaining, protocol::read_frame(&mut read_half, &mut buffer)).await {
                Ok(Ok(Some(frame))) => {
                    if frame.get("type").and_then(Value::as_str) == Some("welcome") {
                        break frame;
                    }
                }
                Ok(Ok(None)) => bail!("配对码错误，或目标设备拒绝了连接。"),
                Ok(Err(error)) => return Err(error),
                Err(_) => bail!("配对码错误，或目标设备没有确认连接。"),
            }
        };
        let server = welcome.get("server").cloned().unwrap_or(Value::Null);
        let peer_id = server
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| payload.server_id.clone());
        let name = server
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| payload.server_name.clone());
        let platform = server
            .get("platform")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| payload.platform.clone());
        let host = format!("{}:{}", payload.host, payload.port);

        let conn_id = self
            .register_peer(&peer_id, &name, &platform, &host, write_half)
            .await;

        if let Some(value) = welcome.get("pairingPayload") {
            self.remember_pairing_value(value, &peer_id, &name, &platform, &host)
                .await;
        } else {
            self.remember_pairing_payload(&payload, &peer_id, &name, &platform, &host)
                .await;
        }

        if !quiet {
            println!("[连接] 已连接到 {name} ({platform}) {host}");
        }

        let service = self.clone();
        let loop_peer_id = peer_id.clone();
        tokio::spawn(async move {
            service
                .read_loop(read_half, buffer, loop_peer_id.clone())
                .await;
            service.unregister_peer(&loop_peer_id, &conn_id).await;
        });

        Ok(())
    }

    async fn handle_inbound(self: Arc<Self>, mut stream: TcpStream) {
        let remote = stream.peer_addr().ok();
        let host = remote
            .map(|address| format!("{}:{}", address.ip(), address.port()))
            .unwrap_or_else(|| "unknown".to_string());

        let mut buffer = Vec::new();
        let hello = match timeout(
            HANDSHAKE_TIMEOUT,
            protocol::read_frame(&mut stream, &mut buffer),
        )
        .await
        {
            Ok(Ok(Some(frame))) => frame,
            Ok(Ok(None)) => return,
            Ok(Err(error)) => {
                tracing::debug!("握手读取失败：{error}");
                return;
            }
            Err(_) => return,
        };

        if hello.get("type").and_then(Value::as_str) != Some("hello") {
            return;
        }

        let (authorized, welcome) = {
            let data = self.data.lock().await;
            let token = hello.get("token").and_then(Value::as_str).unwrap_or("");
            let secret = hello
                .get("pairingSecret")
                .and_then(Value::as_str)
                .unwrap_or("");
            let authorized = (!token.is_empty() && token == data.identity.token)
                || (!secret.is_empty() && secret == pairing::last3(&data.identity.pairing_code));
            let payload = self.payload_from(&data);
            let welcome = json!({
                "type": "welcome",
                "server": {
                    "id": data.device.id,
                    "name": data.device.name,
                    "platform": data.device.platform,
                },
                "pairingPayload": payload,
                "peers": [],
            });
            (authorized, welcome)
        };

        if !authorized {
            tracing::debug!("拒绝未通过校验的连接：{host}");
            return;
        }

        if protocol::write_frame(&mut stream, &welcome).await.is_err() {
            return;
        }

        let device = hello.get("device").cloned().unwrap_or(Value::Null);
        let peer_id = device
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if peer_id.is_empty() {
            return;
        }
        let name = device
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("未命名设备")
            .to_string();
        let platform = device
            .get("platform")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let (read_half, write_half) = stream.into_split();
        let conn_id = self
            .register_peer(&peer_id, &name, &platform, &host, write_half)
            .await;

        if let Some(value) = hello.get("pairingPayload") {
            self.remember_pairing_value(value, &peer_id, &name, &platform, &host)
                .await;
        }

        println!("[连接] {name} ({platform}) 已连接 {host}");

        self.read_loop(read_half, buffer, peer_id.clone()).await;
        self.unregister_peer(&peer_id, &conn_id).await;
    }

    async fn register_peer(
        &self,
        id: &str,
        name: &str,
        platform: &str,
        host: &str,
        write_half: OwnedWriteHalf,
    ) -> String {
        let conn_id = uuid::Uuid::new_v4().to_string();
        let (sender, mut receiver) = mpsc::unbounded_channel::<Value>();

        tokio::spawn(async move {
            let mut writer = write_half;
            while let Some(message) = receiver.recv().await {
                if protocol::write_frame(&mut writer, &message).await.is_err() {
                    break;
                }
            }
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
        });

        let mut data = self.data.lock().await;
        let previous = data.peers.insert(
            id.to_string(),
            PeerHandle {
                conn_id: conn_id.clone(),
                name: name.to_string(),
                platform: platform.to_string(),
                host: host.to_string(),
                status: "online".to_string(),
                last_seen: pairing::now_ms(),
                tx: sender,
            },
        );
        drop(previous);
        conn_id
    }

    async fn unregister_peer(&self, id: &str, conn_id: &str) {
        let mut data = self.data.lock().await;
        let is_current = data
            .peers
            .get(id)
            .map(|peer| peer.conn_id == conn_id)
            .unwrap_or(false);
        if !is_current {
            return;
        }
        if let Some(peer) = data.peers.remove(id) {
            let remaining = data.peers.len();
            drop(data);
            println!(
                "[断开] {} ({}) 已断开{}",
                peer.name,
                peer.platform,
                if remaining == 0 {
                    "，等待新设备连接"
                } else {
                    ""
                }
            );
            self.persist().await;
        }
    }

    async fn read_loop(&self, mut read_half: OwnedReadHalf, mut buffer: Vec<u8>, peer_id: String) {
        loop {
            match protocol::read_frame(&mut read_half, &mut buffer).await {
                Ok(Some(message)) => {
                    if let Err(error) = self.handle_peer_message(&peer_id, message).await {
                        tracing::debug!("处理消息失败（{peer_id}）：{error}");
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::debug!("读取消息失败（{peer_id}）：{error}");
                    break;
                }
            }
        }
    }

    async fn handle_peer_message(&self, peer_id: &str, message: Value) -> Result<()> {
        match message.get("type").and_then(Value::as_str).unwrap_or("") {
            "clipboard_update" => {
                let route_id = message
                    .get("routeId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let Some(value) = message.get("entry").cloned() else {
                    return Ok(());
                };
                let Some(mut entry) = serde_json::from_value::<HistoryEntry>(value)
                    .ok()
                    .and_then(HistoryEntry::normalize)
                else {
                    tracing::debug!("忽略无法解析的剪贴板记录");
                    return Ok(());
                };
                entry.direction = "inbound".to_string();

                {
                    let mut data = self.data.lock().await;
                    if let Some(peer) = data.peers.get_mut(peer_id) {
                        peer.last_seen = pairing::now_ms();
                    }
                }

                self.publish(entry, route_id, Some(peer_id.to_string()), true)
                    .await?;
            }
            "welcome" | "hello" => {}
            other => tracing::debug!("忽略未知消息类型：{other}"),
        }
        Ok(())
    }

    async fn handle_local_clipboard(&self, signature: String, entry: HistoryEntry) {
        {
            let mut data = self.data.lock().await;
            if signature == data.last_signature {
                return;
            }
            if signature == data.suppressed_signature {
                data.last_signature = signature;
                data.suppressed_signature.clear();
                return;
            }
            data.last_signature = signature;
        }

        let _ = self
            .publish(entry, uuid::Uuid::new_v4().to_string(), None, false)
            .await;
    }

    async fn claim_route(&self, route_id: &str) -> bool {
        let mut data = self.data.lock().await;
        if data.recent_routes.contains_key(route_id) {
            return false;
        }
        data.recent_routes
            .insert(route_id.to_string(), pairing::now_ms());

        if data.recent_routes.len() > MAX_RECENT_ROUTES {
            let mut routes: Vec<(String, i64)> = data
                .recent_routes
                .iter()
                .map(|(key, value)| (key.clone(), *value))
                .collect();
            routes.sort_by_key(|(_, timestamp)| *timestamp);
            for (key, _) in routes.into_iter().take(ROUTE_TRIM) {
                data.recent_routes.remove(&key);
            }
        }
        true
    }

    async fn publish(
        &self,
        entry: HistoryEntry,
        route_id: String,
        origin: Option<String>,
        apply_locally: bool,
    ) -> Result<()> {
        if !self.claim_route(&route_id).await {
            return Ok(());
        }

        let (apply, targets) = {
            let mut data = self.data.lock().await;
            let apply = apply_locally && data.settings.sync_enabled;
            if apply {
                let signature = entry.signature();
                data.last_signature = signature.clone();
                data.suppressed_signature = signature;
            }

            data.history.retain(|item| item.sha256 != entry.sha256);
            data.history.insert(0, entry.clone());
            let limit = data.settings.history_limit;
            data.history.truncate(limit);

            let targets: Vec<(String, mpsc::UnboundedSender<Value>)> = data
                .peers
                .iter()
                .filter(|(id, _)| Some(*id) != origin.as_ref())
                .map(|(id, peer)| (id.clone(), peer.tx.clone()))
                .collect();

            (apply, targets)
        };

        if apply {
            let entry_for_write = entry.clone();
            match tokio::task::spawn_blocking(move || clipboard::write_clipboard(&entry_for_write))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    let mut data = self.data.lock().await;
                    data.suppressed_signature.clear();
                    tracing::warn!("写入剪贴板失败：{error}");
                }
                Err(error) => tracing::warn!("剪贴板任务异常：{error}"),
            }
        }

        let sent = targets.len();
        for (_peer_id, sender) in targets {
            let mut outbound = entry.clone();
            outbound.direction = "outbound".to_string();
            let _ = sender.send(json!({
                "type": "clipboard_update",
                "routeId": route_id,
                "entry": outbound,
            }));
        }

        print_sync(&entry, sent);
        self.persist().await;
        Ok(())
    }

    async fn remember_pairing_value(
        &self,
        value: &Value,
        peer_id: &str,
        peer_name: &str,
        peer_platform: &str,
        host: &str,
    ) {
        let Some(mut remembered) = pairing::normalize_remembered(value) else {
            return;
        };
        if !peer_id.is_empty() {
            remembered.server_id = peer_id.to_string();
        }
        if !peer_name.is_empty() {
            remembered.server_name = peer_name.to_string();
        }
        if !peer_platform.is_empty() && peer_platform != "unknown" {
            remembered.platform = peer_platform.to_string();
        }
        if let Some((address, _port)) = host.rsplit_once(':') {
            if !address.is_empty() {
                remembered.host = address.to_string();
            }
        }
        remembered.last_connected_at = pairing::now_ms();

        {
            let mut data = self.data.lock().await;
            data.remembered
                .insert(remembered.server_id.clone(), remembered.clone());
            data.default_pairing_id = remembered.server_id.clone();
        }
        self.persist().await;
    }

    async fn remember_pairing_payload(
        &self,
        payload: &PairingPayload,
        peer_id: &str,
        peer_name: &str,
        peer_platform: &str,
        host: &str,
    ) {
        let value = json!({
            "host": payload.host,
            "port": payload.port,
            "serverId": peer_id,
            "serverName": peer_name,
            "platform": peer_platform,
            "token": payload.token,
            "pairingCode": payload.pairing_code,
            "pairingSecret": payload.pairing_secret,
            "issuedAt": payload.issued_at,
        });
        self.remember_pairing_value(&value, peer_id, peer_name, peer_platform, host)
            .await;
    }

    async fn persist(&self) {
        let document = {
            let data = self.data.lock().await;
            data.to_document()
        };
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || store.save(&document)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!("保存数据失败：{error}"),
            Err(error) => tracing::warn!("保存任务异常：{error}"),
        }
    }
}

fn print_sync(entry: &HistoryEntry, sent: usize) {
    let kind = entry.kind_label();
    let preview = entry.short_preview();
    if entry.direction == "inbound" {
        println!("[同步] {kind} ← {} · {preview}", entry.source_device_name);
    } else if sent > 0 {
        println!("[同步] {kind} → {sent} 台设备 · {preview}");
    } else {
        println!("[记录] {kind} · {preview}（暂无已连接设备，仅本地保存）");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::StoreDocument;
    use crate::store::Store;
    use std::time::Duration;
    use tokio::net::TcpStream;
    use tokio::time::{sleep, timeout};

    async fn handshake(
        port: u16,
        token: &str,
        device_id: &str,
        name: &str,
    ) -> (TcpStream, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let hello = json!({
            "type": "hello",
            "token": token,
            "device": {
                "id": device_id,
                "name": name,
                "platform": "android",
                "role": "mobile",
            },
        });
        protocol::write_frame(&mut stream, &hello).await.unwrap();

        let mut buffer = Vec::new();
        let welcome = timeout(
            Duration::from_secs(3),
            protocol::read_frame(&mut stream, &mut buffer),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();

        assert_eq!(welcome.get("type").and_then(Value::as_str), Some("welcome"));
        (stream, buffer)
    }

    async fn wait_for_peers(service: &Arc<Service>, expected: usize) {
        for _ in 0..100 {
            if service.peers_snapshot().await.len() >= expected {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("设备未在预期时间内完成握手");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_and_relay_between_two_peers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::from_dir(dir.path());
        let mut document = StoreDocument::default();
        document.settings.sync_enabled = false;
        store.save(&document).unwrap();

        let service = Arc::new(Service::new(store).unwrap());
        let address = service.bind(0).await.unwrap();
        let token = service.pairing_payload().await.token;

        let (mut phone, _phone_buffer) =
            handshake(address.port(), &token, "phone-1", "Pixel").await;
        let (mut tablet, mut tablet_buffer) =
            handshake(address.port(), &token, "phone-2", "Pad").await;
        wait_for_peers(&service, 2).await;

        let update = json!({
            "type": "clipboard_update",
            "routeId": "route-1",
            "entry": {
                "mimeType": "text/plain",
                "preview": "hello",
                "text": "hello",
                "createdAt": 1,
                "sourceDeviceId": "phone-1",
                "sourceDeviceName": "Pixel",
                "sha256": "deadbeef",
                "direction": "outbound",
            },
        });
        protocol::write_frame(&mut phone, &update).await.unwrap();

        let relayed = timeout(
            Duration::from_secs(3),
            protocol::read_frame(&mut tablet, &mut tablet_buffer),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();

        assert_eq!(
            relayed.get("type").and_then(Value::as_str),
            Some("clipboard_update")
        );
        assert_eq!(
            relayed.get("routeId").and_then(Value::as_str),
            Some("route-1")
        );
        assert_eq!(relayed["entry"]["preview"].as_str(), Some("hello"));
        assert_eq!(relayed["entry"]["direction"].as_str(), Some("outbound"));

        let history = service.history_snapshot(Some(10)).await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].sha256, "deadbeef");
        assert_eq!(history[0].direction, "inbound");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_wrong_token() {
        let dir = tempfile::tempdir().unwrap();
        let service = Arc::new(Service::new(Store::from_dir(dir.path())).unwrap());
        let address = service.bind(0).await.unwrap();

        let mut stream = TcpStream::connect(("127.0.0.1", address.port()))
            .await
            .unwrap();
        protocol::write_frame(
            &mut stream,
            &json!({
                "type": "hello",
                "token": "wrong",
                "device": {
                    "id": "phone-1",
                    "name": "Pixel",
                    "platform": "android",
                    "role": "mobile",
                },
            }),
        )
        .await
        .unwrap();

        let mut buffer = Vec::new();
        if let Ok(Ok(Some(frame))) = timeout(
            Duration::from_secs(2),
            protocol::read_frame(&mut stream, &mut buffer),
        )
        .await
        {
            panic!("未预期的握手响应：{frame}");
        }

        assert!(service.peers_snapshot().await.is_empty());
    }
}
