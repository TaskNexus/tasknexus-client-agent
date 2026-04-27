//! 跨平台系统服务管理模块
//!
//! 提供 install / uninstall / start / stop / restart / status 统一接口，
//! 内部根据编译目标平台分发到各自实现。
//!
//! 支持多实例：通过 `--name` 参数可以安装多个独立的服务实例，
//! 每个实例指向不同的配置文件（例如不同磁盘的工作空间）。

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

/// 默认服务名称常量
pub const DEFAULT_SERVICE_NAME: &str = "tasknexus";
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
        /// 服务名称（多实例时需不同，默认: tasknexus）
        #[arg(short, long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
    },
    /// 卸载系统服务
    Uninstall {
        /// 服务名称
        #[arg(short, long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
    },
    /// 启动服务
    Start {
        /// 服务名称
        #[arg(short, long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
    },
    /// 停止服务
    Stop {
        /// 服务名称
        #[arg(short, long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
    },
    /// 重启服务
    Restart {
        /// 服务名称
        #[arg(short, long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
    },
    /// 查看服务状态
    Status {
        /// 服务名称
        #[arg(short, long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
    },
}

/// 根据服务名称生成显示名称。
///
/// 默认实例: "TaskNexus Agent"
/// 自定义实例: "TaskNexus Agent (custom-name)"
pub fn display_name_for(service_name: &str) -> String {
    if service_name == DEFAULT_SERVICE_NAME {
        SERVICE_DISPLAY_NAME.to_string()
    } else {
        format!("{} ({})", SERVICE_DISPLAY_NAME, service_name)
    }
}

/// 处理服务管理子命令（由 main.rs 调用）
///
/// 此函数不返回 —— 执行成功后 exit(0)，失败后 exit(1)。
pub fn handle_service_command(action: ServiceAction) {
    let result = match action {
        ServiceAction::Install { config, name } => {
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
            platform::install_service(&name, &config_path)
        }
        ServiceAction::Uninstall { name } => platform::uninstall_service(&name),
        ServiceAction::Start { name } => platform::start_service(&name),
        ServiceAction::Stop { name } => platform::stop_service(&name),
        ServiceAction::Restart { name } => {
            platform::stop_service(&name).and_then(|_| platform::start_service(&name))
        }
        ServiceAction::Status { name } => platform::query_service_status(&name),
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
