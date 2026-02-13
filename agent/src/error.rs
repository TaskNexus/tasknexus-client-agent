//! TaskNexus Agent 错误类型
//!
//! 统一的错误处理机制。

use std::io;
use thiserror::Error;

/// Agent 错误类型
#[derive(Error, Debug)]
pub enum AgentError {
    #[error("配置错误: {0}")]
    Config(String),

    #[error("连接错误: {0}")]
    Connection(String),

    #[error("WebSocket 错误: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("JSON 解析错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("YAML 解析错误: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("IO 错误: {0}")]
    Io(#[from] io::Error),

    #[error("URL 解析错误: {0}")]
    Url(#[from] url::ParseError),

    #[error("任务执行错误: {0}")]
    Execution(String),

    #[error("超时")]
    Timeout,
}

pub type Result<T> = std::result::Result<T, AgentError>;
