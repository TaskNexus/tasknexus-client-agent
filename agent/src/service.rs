//! 跨平台系统服务管理模块
//!
//! 提供 install / uninstall / start / stop / restart / status 统一接口，
//! 内部根据编译目标平台分发到各自实现。

#[cfg(target_os = "windows")]
#[path = "service_windows.rs"]
mod platform;

#[cfg(target_os = "linux")]
#[path = "service_linux.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "service_macos.rs"]
mod platform;

use clap::Subcommand;
use std::path::PathBuf;

/// 服务名称常量
pub const SERVICE_NAME: &str = "tasknexus";
pub const SERVICE_DISPLAY_NAME: &str = "TaskNexus Agent";
pub const SERVICE_DESCRIPTION: &str =
    "TaskNexus client agent - connects to TaskNexus server and executes remote tasks";

/// 服务管理子命令
#[derive(Subcommand, Debug)]
pub enum ServiceAction {
    /// 安装为系统服务
    Install {
        /// 配置文件路径（必须为绝对路径）
        #[arg(short, long)]
        config: PathBuf,
    },
    /// 卸载系统服务
    Uninstall,
    /// 启动服务
    Start,
    /// 停止服务
    Stop,
    /// 重启服务
    Restart,
    /// 查看服务状态
    Status,
}

/// 处理服务管理子命令（由 main.rs 调用）
///
/// 此函数不返回 —— 执行成功后 exit(0)，失败后 exit(1)。
pub fn handle_service_command(action: ServiceAction) {
    let result = match action {
        ServiceAction::Install { config } => {
            let config_path = match std::fs::canonicalize(&config) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("无法解析配置文件路径 {:?}: {}", config, e);
                    std::process::exit(1);
                }
            };
            if !config_path.exists() {
                eprintln!("配置文件不存在: {:?}", config_path);
                std::process::exit(1);
            }
            platform::install_service(&config_path)
        }
        ServiceAction::Uninstall => platform::uninstall_service(),
        ServiceAction::Start => platform::start_service(),
        ServiceAction::Stop => platform::stop_service(),
        ServiceAction::Restart => platform::stop_service().and_then(|_| platform::start_service()),
        ServiceAction::Status => platform::query_service_status(),
    };

    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("操作失败: {}", e);
            std::process::exit(1);
        }
    }
}

/// 通过服务管理器重启当前服务（用于自更新后）。
///
/// 不同平台实现不同：
/// - Windows / Linux / macOS 下一般使用非零退出码触发 restart-on-failure
pub fn restart_via_service_manager() {
    platform::restart_via_service_manager();
}
