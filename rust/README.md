# ClipVault CLI（Rust）

Windows 端的单进程前台 CLI 实现，用来替代 `electron/` 下的桌面端。协议与 Android/iOS 端保持完全一致，手机端无需任何改动。

## 快速开始

```bash
cd rust

# 前台运行：监听 49372/TCP 与 49373/UDP，打印二维码和 6 位配对码
cargo run --release -- run

# 不渲染终端二维码（适合小终端或日志采集）
cargo run --release -- run --no-qr

# 启动后直接连接另一台设备：6 位配对码或二维码 JSON
cargo run --release -- run --pair 086314
cargo run --release -- run --pair '{"version":1,"host":"192.168.1.12",...}'
```

打包产物是单个 `target/release/clipvault.exe`，约 2–3 MB，无需 Node.js 或 Electron。

## 运行中的交互命令

启动后在同一个终端输入：

| 命令 | 说明 |
| --- | --- |
| `status` | 查看监听地址、配对码、已连接设备 |
| `qr` | 重新打印二维码与配对码 |
| `refresh` | 刷新令牌与配对码（已连接设备不受影响） |
| `pair <码\|JSON>` | 主动连接另一台设备 |
| `peers` | 列出已连接设备（含设备 ID 前缀） |
| `history [n]` | 查看最近 n 条记录（默认 20） |
| `copy <序号\|ID>` | 把历史记录写回剪贴板并继续广播 |
| `clear` | 清空历史记录 |
| `disconnect [ID\|all]` | 断开指定设备或全部设备 |
| `sync on\|off` | 暂停/恢复剪贴板同步（暂停后仍保持连接） |
| `limit <n>` | 设置历史上限，范围 20–1000 |
| `quit` / `Ctrl+C` | 退出 |

## 离线命令

服务未运行时可用（会直接读写数据文件）：

```bash
clipvault pairing [--qr] [--json]   # 打印当前配对码，--json 便于脚本读取
clipvault history -n 20 [--json]
clipvault clear
clipvault config [get|set] [sync on|off] [limit 200] [--json]
clipvault info
```

`clipvault` 不带子命令等同于 `clipvault run`。
`clipvault pairing` 不会启动服务，适合把配对码接入脚本；首次使用前请先运行一次
`clipvault run`，设备身份才会落盘并保持稳定。

## 与 Electron 版的差异

| 项目 | Electron 版 | CLI 版 |
| --- | --- | --- |
| 界面 | React 窗口 + 托盘 + 全局快捷键 | 无窗口、无托盘，终端交互 |
| 剪贴板监听 | 每 700ms 轮询 | Windows `AddClipboardFormatListener` 事件驱动 |
| 开机自启 | 写入注册表 | 未实现（可自行放到启动文件夹） |
| 安装包 | NSIS / portable | 单个 exe |
| 后台常驻 | 关闭窗口最小化到托盘 | 前台进程，Ctrl+C 才退出 |
| 协议 | TCP 49372 / UDP 49373 | 完全一致 |

> 剪贴板是会话级资源，Windows 服务（Session 0）拿不到用户剪贴板，因此没有做服务模式。
> 需要在登录后自动运行，可以给 `clipvault.exe run --no-qr` 建一个快捷方式放进
> `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup`。

## 数据兼容

沿用 Electron 版的存储位置和字段名：

```text
%APPDATA%\ClipVault\clipvault\clipvault-store.json
```

日志直接输出到终端（前台单进程，不再写 `clipvault-sync.log`）。
首次运行会读取旧文件并继续使用其中的设备 ID、令牌、历史与已记忆配对；旧版本可以直接切回来。
写入使用「临时文件 + rename」的原子替换，避免同步过程中崩溃写坏历史。

> 离线命令与服务同时运行会互相覆盖，请先退出运行中的实例再执行 `history`/`clear`/`config`。
> 第二个实例如果端口被占用会直接退出，并且不会覆写数据文件。

## Windows 注意事项

- 首次监听 49372 时 Windows 防火墙会弹窗。只在本机热点/家庭网络使用，选择「专用网络」即可；
  若误点取消，可手动放行 `clipvault.exe` 的入站 TCP 49372 与 UDP 49373。
- 配对时若终端二维码扫不出来，直接用 6 位配对码：前 3 位是电脑 IP 最后一段，后 3 位是会话校验码。
- 未签名的 exe 在别的电脑上可能触发 SmartScreen 提示，正式分发前建议做代码签名。

## 测试

```bash
cargo test
```

覆盖帧编解码（含分包/粘包）、配对码生成与解析、存储读写与字段兼容，以及一个端到端用例：
两个模拟手机同时握手，验证消息转发与历史记录。

手动验证真实剪贴板（会读取当前剪贴板）：

```bash
cargo test manual_read_current_clipboard -- --ignored --nocapture
```
