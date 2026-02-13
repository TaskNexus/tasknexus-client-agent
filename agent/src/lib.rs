//! TaskNexus Agent 库
//!
//! 提供客户端代理的核心功能模块。

pub mod client;
pub mod config;
pub mod error;
pub mod executor;
pub mod autostart;

pub use client::AgentClient;
pub use config::AgentConfig;
pub use error::{AgentError, Result};
pub use executor::{CommandExecutor, ExecutionResult, TaskRunner};
