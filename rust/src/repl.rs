use std::io::Write;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Notify;

use crate::console;
use crate::service::Service;

pub async fn run(service: Arc<Service>, shutdown: Arc<Notify>) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();

    loop {
        print!("> ");
        let _ = std::io::stdout().flush();

        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                // Launched without an interactive terminal (or piped input): keep
                // the sync service running until Ctrl+C instead of exiting.
                eprintln!("[提示] 标准输入已关闭，服务继续运行；按 Ctrl+C 退出。");
                std::future::pending::<()>().await;
                continue;
            }
            Err(error) => {
                eprintln!("[输入] 读取失败：{error}");
                break;
            }
        };

        let line = line.trim();
        let (command, rest) = match line.find(char::is_whitespace) {
            Some(index) => (&line[..index], line[index..].trim()),
            None => (line, ""),
        };

        match command {
            "" => {}
            "help" | "?" => console::print_help(),
            "status" => println!("{}", service.status_text().await),
            "qr" => {
                let payload = service.pairing_payload().await;
                console::print_pairing(&payload, true, false);
            }
            "refresh" => {
                let payload = service.refresh_pairing().await;
                println!("[配对] 已刷新令牌与配对码。");
                console::print_pairing(&payload, true, false);
            }
            "pair" => {
                if rest.is_empty() {
                    println!("用法: pair <6 位配对码 | 二维码 JSON>");
                } else {
                    match service.connect_input(rest, false).await {
                        Ok(()) => {}
                        Err(error) => eprintln!("[连接] 失败：{error}"),
                    }
                }
            }
            "peers" => {
                let peers = service.peers_snapshot().await;
                console::print_peers(&peers);
            }
            "history" => {
                let limit = rest.parse::<usize>().ok().unwrap_or(20);
                let entries = service.history_snapshot(Some(limit)).await;
                console::print_history(&entries, false);
            }
            "copy" => {
                if rest.is_empty() {
                    println!("用法: copy <history 中的序号 | 记录 ID>");
                } else {
                    match service.copy_entry(rest).await {
                        Ok(preview) => println!("[复制] 已写入剪贴板并同步：{preview}"),
                        Err(error) => eprintln!("[复制] 失败：{error}"),
                    }
                }
            }
            "clear" => {
                let removed = service.clear_history().await;
                println!("[历史] 已清空 {removed} 条记录。");
            }
            "disconnect" | "disconnect-all" => {
                let selector = if rest.is_empty() { "all" } else { rest };
                let removed = service.disconnect(selector).await;
                if removed == 0 {
                    println!("（没有匹配的设备）");
                } else {
                    println!("[连接] 已断开 {removed} 台设备。");
                }
            }
            "sync" => match rest {
                "on" | "true" | "1" => {
                    service.set_sync_enabled(true).await;
                    println!("[设置] 剪贴板同步已开启。");
                }
                "off" | "false" | "0" => {
                    service.set_sync_enabled(false).await;
                    println!("[设置] 剪贴板同步已暂停。");
                }
                _ => println!("用法: sync on|off"),
            },
            "limit" => match rest.parse::<usize>() {
                Ok(value) => {
                    let applied = service.set_history_limit(value).await;
                    println!("[设置] 历史上限已设为 {applied}。");
                }
                Err(_) => println!("用法: limit <20-1000>"),
            },
            "quit" | "exit" | "q" => {
                shutdown.notify_one();
                break;
            }
            other => println!("未知命令 `{other}`，输入 help 查看所有命令。"),
        }
    }
}
