use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "clipvault",
    version,
    about = "ClipVault 局域网剪贴板同步（前台单进程 CLI）",
    long_about = "启动后本机作为 TCP 服务端（49372）并监听 UDP 发现端口（49373），\n与 Android/iOS 端保持现有协议兼容。所有控制命令都在运行中的终端里输入。"
)]
pub struct Cli {
    /// 输出调试日志
    #[arg(long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 启动同步服务并在前台运行（默认）
    Run(RunArgs),
    /// 打印当前配对码与二维码（不启动服务）
    Pairing(PairingArgs),
    /// 查看本地历史记录（需先退出运行中的实例）
    History(HistoryArgs),
    /// 清空本地历史记录
    Clear,
    /// 查看或修改本地设置
    Config(ConfigArgs),
    /// 显示数据目录、设备信息与已记忆的配对
    Info,
}

#[derive(Debug, Default, Args)]
pub struct RunArgs {
    /// 启动后立即连接：6 位配对码或二维码 JSON
    #[arg(long)]
    pub pair: Option<String>,
    /// 不在终端渲染二维码
    #[arg(long)]
    pub no_qr: bool,
}

#[derive(Debug, Args)]
pub struct PairingArgs {
    /// 同时渲染终端二维码
    #[arg(long)]
    pub qr: bool,
    /// 以 JSON 输出，方便脚本读取
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// 显示最近多少条
    #[arg(short, long, default_value_t = 20)]
    pub limit: usize,
    /// 以 JSON 输出
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// 设置项：sync | limit
    pub key: Option<String>,
    /// 新的值：sync 用 on/off，limit 用数字
    pub value: Option<String>,
    /// 以 JSON 输出
    #[arg(long)]
    pub json: bool,
}
