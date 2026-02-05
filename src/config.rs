//! 配置管理模块
//!
//! 处理 Agent 的配置，支持命令行参数、配置文件和环境变量。

use crate::error::Result;
use serde::Deserialize;
use std::net::UdpSocket;
use std::path::PathBuf;
use sysinfo::System;

/// Agent 配置
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// WebSocket 服务器地址
    pub server: String,

    /// Agent 名称
    pub name: String,

    /// 工作空间根目录
    pub workspaces_path: PathBuf,

    /// 日志级别
    pub log_level: String,

    /// 日志文件路径
    pub log_file: Option<PathBuf>,

    /// 心跳间隔(秒)
    pub heartbeat_interval: u64,

    /// 重连间隔(秒)
    pub reconnect_interval: u64,

    /// 最大重连次数 (-1 表示无限)
    pub max_reconnect_attempts: i32,

    /// 默认任务超时(秒)
    pub task_timeout: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            server: String::new(),
            name: hostname::get()
                .map(|h| h.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".to_string()),
            workspaces_path: PathBuf::from("./workspaces"),
            log_level: "INFO".to_string(),
            log_file: None,
            heartbeat_interval: 30,
            reconnect_interval: 5,
            max_reconnect_attempts: -1,
            task_timeout: 3600,
        }
    }
}

impl AgentConfig {
    /// 从 YAML 文件加载配置
    pub fn from_file(path: &PathBuf) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: AgentConfig = serde_yaml::from_str(&content)?;
        Ok(config)
    }

    /// 从环境变量加载配置
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(server) = std::env::var("TASKNEXUS_SERVER") {
            config.server = server;
        }
        if let Ok(name) = std::env::var("TASKNEXUS_AGENT_NAME") {
            config.name = name;
        }
        if let Ok(path) = std::env::var("TASKNEXUS_WORKSPACES_PATH") {
            config.workspaces_path = PathBuf::from(path);
        }
        if let Ok(level) = std::env::var("TASKNEXUS_LOG_LEVEL") {
            config.log_level = level;
        }
        if let Ok(interval) = std::env::var("TASKNEXUS_HEARTBEAT_INTERVAL") {
            if let Ok(val) = interval.parse() {
                config.heartbeat_interval = val;
            }
        }

        config
    }

    /// 验证配置
    pub fn validate(&self) -> std::result::Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if self.server.is_empty() {
            errors.push("Server URL is required".to_string());
        }
        if self.name.is_empty() {
            errors.push("Agent name is required".to_string());
        }
        if !self.server.is_empty()
            && !self.server.starts_with("ws://")
            && !self.server.starts_with("wss://")
        {
            errors.push("Server URL must start with ws:// or wss://".to_string());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// 获取系统信息用于心跳上报
    pub fn get_system_info(&self) -> SystemInfo {
        let mut sys = System::new_all();
        sys.refresh_all();

        SystemInfo {
            hostname: hostname::get()
                .map(|h| h.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".to_string()),
            platform: std::env::consts::OS.to_string(),
            platform_version: System::os_version().unwrap_or_else(|| "unknown".to_string()),
            platform_release: System::kernel_version().unwrap_or_else(|| "unknown".to_string()),
            architecture: std::env::consts::ARCH.to_string(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            ip_address: get_local_ip(),
        }
    }

    /// 获取工作空间路径
    pub fn get_workspace_path(&self, workspace_name: &str) -> PathBuf {
        self.workspaces_path.join(workspace_name)
    }

    /// 合并命令行参数
    pub fn merge_cli(
        &mut self,
        server: Option<String>,
        name: Option<String>,
        workspaces_path: Option<PathBuf>,
        log_level: Option<String>,
        heartbeat_interval: Option<u64>,
    ) {
        if let Some(s) = server {
            self.server = s;
        }
        if let Some(n) = name {
            self.name = n;
        }
        if let Some(p) = workspaces_path {
            self.workspaces_path = p;
        }
        if let Some(l) = log_level {
            self.log_level = l;
        }
        if let Some(h) = heartbeat_interval {
            self.heartbeat_interval = h;
        }
    }
}

/// 系统信息
#[derive(Debug, Clone, serde::Serialize)]
pub struct SystemInfo {
    pub hostname: String,
    pub platform: String,
    pub platform_version: String,
    pub platform_release: String,
    pub architecture: String,
    pub agent_version: String,
    pub ip_address: String,
}

/// 获取本机 IP 地址
fn get_local_ip() -> String {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("8.8.8.8:80")?;
            socket.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// 加载配置，优先级：命令行参数 > 配置文件 > 环境变量 > 默认值
pub fn load_config(
    config_file: Option<PathBuf>,
    server: Option<String>,
    name: Option<String>,
    workspaces_path: Option<PathBuf>,
    log_level: Option<String>,
    heartbeat_interval: Option<u64>,
) -> Result<AgentConfig> {
    // 从环境变量开始
    let mut config = AgentConfig::from_env();

    // 如果有配置文件，加载并覆盖
    if let Some(ref path) = config_file {
        if path.exists() {
            let file_config = AgentConfig::from_file(path)?;
            // 只覆盖非默认值
            if !file_config.server.is_empty() {
                config.server = file_config.server;
            }
            if file_config.name != AgentConfig::default().name {
                config.name = file_config.name;
            }
            if file_config.workspaces_path != PathBuf::from("./workspaces") {
                config.workspaces_path = file_config.workspaces_path;
            }
            if file_config.log_level != "INFO" {
                config.log_level = file_config.log_level;
            }
            if file_config.log_file.is_some() {
                config.log_file = file_config.log_file;
            }
            if file_config.heartbeat_interval != 30 {
                config.heartbeat_interval = file_config.heartbeat_interval;
            }
            if file_config.reconnect_interval != 5 {
                config.reconnect_interval = file_config.reconnect_interval;
            }
            if file_config.max_reconnect_attempts != -1 {
                config.max_reconnect_attempts = file_config.max_reconnect_attempts;
            }
            if file_config.task_timeout != 3600 {
                config.task_timeout = file_config.task_timeout;
            }
        }
    }

    // 命令行参数覆盖
    config.merge_cli(server, name, workspaces_path, log_level, heartbeat_interval);

    Ok(config)
}
