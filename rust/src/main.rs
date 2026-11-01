mod cli;
mod clipboard;
mod console;
mod models;
mod pairing;
mod protocol;
mod repl;
mod service;
mod store;

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;
use tokio::sync::Notify;

use crate::cli::{Cli, Command, ConfigArgs, HistoryArgs, PairingArgs, RunArgs};
use crate::models::{clamp_history_limit, PairingPayload, DEFAULT_SERVER_PORT};
use crate::service::Service;
use crate::store::Store;

#[tokio::main]
async fn main() {
    let args = Cli::parse();
    init_tracing(args.verbose);

    if let Err(error) = dispatch(args).await {
        eprintln!("错误: {error:#}");
        std::process::exit(1);
    }
}

fn init_tracing(verbose: bool) {
    let default_filter = if verbose {
        "clipvault=debug"
    } else {
        "clipvault=info"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

async fn dispatch(args: Cli) -> Result<()> {
    match args.command.unwrap_or(Command::Run(RunArgs::default())) {
        Command::Run(run) => run_command(run).await,
        Command::Pairing(pairing) => pairing_command(pairing),
        Command::History(history) => history_command(history),
        Command::Clear => clear_command(),
        Command::Config(config) => config_command(config),
        Command::Info => info_command(),
    }
}

async fn run_command(args: RunArgs) -> Result<()> {
    let store = Store::discover()?;
    let service = Arc::new(Service::new(store)?);

    println!(
        "ClipVault 局域网剪贴板同步 (CLI v{})",
        env!("CARGO_PKG_VERSION")
    );
    println!("数据目录: {}", service.store_dir().display());
    println!("设备名称: {}", service.device_name().await);
    println!();

    service.start_network().await?;
    service.spawn_discovery();
    service.spawn_clipboard();
    service.spawn_reconnect();

    let payload = service.pairing_payload().await;
    console::print_pairing(&payload, !args.no_qr, false);
    if payload.host == "127.0.0.1" {
        eprintln!("[警告] 未检测到局域网 IPv4 地址，手机暂时无法连接；请检查网络后重新运行。");
    }

    if let Some(input) = args.pair.as_deref() {
        match service.connect_input(input, false).await {
            Ok(()) => {}
            Err(error) => eprintln!("[连接] 失败：{error}"),
        }
    }

    console::print_startup_hint(service.store_dir());

    let shutdown = Arc::new(Notify::new());
    let repl_service = service.clone();
    let repl_shutdown = shutdown.clone();
    let mut repl_task = tokio::spawn(async move {
        repl::run(repl_service, repl_shutdown).await;
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = shutdown.notified() => {}
        _ = &mut repl_task => {}
    }

    repl_task.abort();
    println!("\n[退出] 已停止 ClipVault。");
    Ok(())
}

fn pairing_command(args: PairingArgs) -> Result<()> {
    let store = Store::discover()?;
    if !store.exists() {
        println!("[提示] 尚未初始化，先运行一次 `clipvault run` 会生成并保存设备身份。");
    }
    let document = store.load()?;
    let host = pairing::local_address();
    let secret = pairing::last3(&document.server_identity.pairing_code);
    let payload = PairingPayload {
        version: 1,
        host: host.clone(),
        port: DEFAULT_SERVER_PORT,
        server_id: document.server_identity.server_id,
        server_name: document.device.name,
        platform: document.device.platform,
        token: document.server_identity.token,
        pairing_code: pairing::build_pairing_code(&host, &secret),
        pairing_secret: secret,
        issued_at: pairing::now_ms(),
    };
    console::print_pairing(&payload, args.qr, args.json);
    Ok(())
}

fn history_command(args: HistoryArgs) -> Result<()> {
    let store = Store::discover()?;
    let document = store.load()?;
    let entries: Vec<_> = document.history.into_iter().take(args.limit).collect();
    console::print_history(&entries, args.json);
    Ok(())
}

fn clear_command() -> Result<()> {
    let store = Store::discover()?;
    let mut document = store.load()?;
    let removed = document.history.len();
    document.history.clear();
    store.save(&document)?;
    println!("已清空 {removed} 条历史记录。");
    Ok(())
}

fn config_command(args: ConfigArgs) -> Result<()> {
    let store = Store::discover()?;
    let mut document = store.load()?;

    match (args.key.as_deref(), args.value.as_deref()) {
        (None, _) => {
            console::print_settings(&document.settings, store.dir(), args.json);
        }
        (Some(key), Some(value)) => {
            match key {
                "sync" => document.settings.sync_enabled = parse_bool(value)?,
                "limit" => {
                    let limit: usize = value.parse().context("历史记录上限必须是数字")?;
                    document.settings.history_limit = clamp_history_limit(limit);
                }
                "port" => bail!("监听端口固定为 {DEFAULT_SERVER_PORT}，不支持修改。"),
                other => bail!("未知设置项：{other}（可用：sync、limit）"),
            }
            store.save(&document)?;
            println!("已保存。");
            console::print_settings(&document.settings, store.dir(), args.json);
        }
        (Some(key), None) => {
            let value = match key {
                "sync" => document.settings.sync_enabled.to_string(),
                "limit" => document.settings.history_limit.to_string(),
                "port" => document.settings.server_port.to_string(),
                other => bail!("未知设置项：{other}（可用：sync、limit）"),
            };
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ key: value }))?
                );
            } else {
                println!("{key} = {value}");
            }
        }
    }

    Ok(())
}

fn info_command() -> Result<()> {
    let store = Store::discover()?;
    if !store.exists() {
        println!("[提示] 尚未初始化，先运行一次 `clipvault run` 会生成并保存设备身份。");
    }
    let document = store.load()?;
    let local_address = pairing::local_address();
    let pairing_code = pairing::build_pairing_code(
        &local_address,
        &pairing::last3(&document.server_identity.pairing_code),
    );

    let mut remembered = document.remembered_pairings.clone();
    remembered.sort_by_key(|item| std::cmp::Reverse(item.last_connected_at));

    console::print_info(
        &document.device.id,
        &document.device.name,
        &document.device.platform,
        store.dir(),
        &pairing_code,
        &remembered,
        document.history.len(),
    );
    Ok(())
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        _ => bail!("请输入 on 或 off。"),
    }
}
