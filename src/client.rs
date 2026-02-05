//! WebSocket 客户端模块
//!
//! 处理与 TaskNexus 服务器的 WebSocket 连接。

use crate::config::{AgentConfig, SystemInfo};
use crate::error::{AgentError, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};
use url::Url;

/// 服务器发送的消息类型
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Connected {
        message: String,
    },
    HeartbeatAck {
        server_time: String,
    },
    TaskDispatch {
        task_id: i64,
        #[serde(default)]
        workspace_name: String,
        #[serde(default)]
        command: String,
        #[serde(default)]
        client_repo_url: Option<String>,
        #[serde(default = "default_ref")]
        client_repo_ref: String,
        #[serde(default = "default_timeout")]
        timeout: u64,
        #[serde(default)]
        environment: HashMap<String, String>,
    },
}

fn default_ref() -> String {
    "main".to_string()
}

fn default_timeout() -> u64 {
    3600
}

/// 客户端发送的消息类型
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Heartbeat { system_info: SystemInfo },
    TaskStarted { task_id: i64 },
    TaskProgress { task_id: i64, output: String },
    TaskCompleted { task_id: i64, exit_code: i32, stdout: String, stderr: String },
    TaskFailed { task_id: i64, error: String },
}

/// 任务分发的数据
#[derive(Debug, Clone)]
pub struct TaskDispatchData {
    pub task_id: i64,
    pub workspace_name: String,
    pub command: String,
    pub client_repo_url: Option<String>,
    pub client_repo_ref: String,
    pub timeout: u64,
    pub environment: HashMap<String, String>,
}

/// WebSocket 客户端
#[derive(Clone)]
pub struct AgentClient {
    pub config: AgentConfig,
    connected: Arc<RwLock<bool>>,
    running: Arc<RwLock<bool>>,
    reconnect_attempts: Arc<RwLock<u32>>,
    sender: Arc<RwLock<Option<mpsc::Sender<ClientMessage>>>>,
}

impl AgentClient {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            connected: Arc::new(RwLock::new(false)),
            running: Arc::new(RwLock::new(false)),
            reconnect_attempts: Arc::new(RwLock::new(0)),
            sender: Arc::new(RwLock::new(None)),
        }
    }

    /// 构建带 name 的 WebSocket URL
    fn ws_url(&self) -> Result<Url> {
        let separator = if self.config.server.contains('?') { "&" } else { "?" };
        let url_str = format!("{}{}name={}", self.config.server, separator, self.config.name);
        Url::parse(&url_str).map_err(AgentError::from)
    }

    /// 发送消息到服务器
    pub async fn send_message(&self, message: ClientMessage) -> Result<()> {
        let sender = self.sender.read().await;
        if let Some(tx) = sender.as_ref() {
            tx.send(message).await.map_err(|e| {
                AgentError::Connection(format!("Failed to send message: {}", e))
            })?;
        } else {
            warn!("Cannot send message: not connected");
        }
        Ok(())
    }

    /// 发送心跳
    pub async fn send_heartbeat(&self) -> Result<()> {
        let system_info = self.config.get_system_info();
        self.send_message(ClientMessage::Heartbeat { system_info }).await
    }

    /// 发送任务开始通知
    pub async fn send_task_started(&self, task_id: i64) -> Result<()> {
        self.send_message(ClientMessage::TaskStarted { task_id }).await
    }

    /// 发送任务进度更新
    pub async fn send_task_progress(&self, task_id: i64, output: String) -> Result<()> {
        self.send_message(ClientMessage::TaskProgress { task_id, output }).await
    }

    /// 发送任务完成通知
    pub async fn send_task_completed(
        &self,
        task_id: i64,
        exit_code: i32,
        stdout: String,
        stderr: String,
    ) -> Result<()> {
        self.send_message(ClientMessage::TaskCompleted {
            task_id,
            exit_code,
            stdout,
            stderr,
        }).await
    }

    /// 发送任务失败通知
    pub async fn send_task_failed(&self, task_id: i64, error: String) -> Result<()> {
        self.send_message(ClientMessage::TaskFailed { task_id, error }).await
    }

    /// 运行客户端主循环
    pub async fn run<F, Fut>(
        &self,
        on_task_dispatch: F,
        on_connected: impl Fn() + Send + Sync + 'static,
        on_disconnected: impl Fn() + Send + Sync + 'static,
    ) -> Result<()>
    where
        F: Fn(TaskDispatchData) -> Fut + Send + Sync + Clone + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        *self.running.write().await = true;

        while *self.running.read().await {
            // 尝试连接
            if !*self.connected.read().await {
                match self.connect().await {
                    Ok(_) => {
                        *self.reconnect_attempts.write().await = 0;
                        on_connected();
                    }
                    Err(e) => {
                        error!("Failed to connect: {}", e);
                        *self.reconnect_attempts.write().await += 1;

                        let attempts = *self.reconnect_attempts.read().await;
                        if self.config.max_reconnect_attempts > 0
                            && attempts >= self.config.max_reconnect_attempts as u32
                        {
                            error!("Max reconnect attempts reached, giving up");
                            break;
                        }

                        let wait_time = std::cmp::min(
                            self.config.reconnect_interval * 2u64.pow(std::cmp::min(attempts, 5)),
                            60,
                        );
                        info!("Reconnecting in {} seconds... (attempt {})", wait_time, attempts);
                        tokio::time::sleep(Duration::from_secs(wait_time)).await;
                        continue;
                    }
                }
            }

            // 运行消息循环
            if let Err(e) = self.message_loop(on_task_dispatch.clone()).await {
                error!("Message loop error: {}", e);
            }

            *self.connected.write().await = false;
            on_disconnected();

            if *self.running.read().await {
                warn!("Connection lost, will reconnect...");
                tokio::time::sleep(Duration::from_secs(self.config.reconnect_interval)).await;
            }
        }

        Ok(())
    }

    /// 连接到服务器
    async fn connect(&self) -> Result<()> {
        let url = self.ws_url()?;
        info!("Connecting to {}...", self.config.server);

        let (ws_stream, _) = connect_async(url.as_str()).await?;
        info!("Connected to server successfully");

        let (write, read) = ws_stream.split();

        // 创建消息发送通道
        let (tx, mut rx) = mpsc::channel::<ClientMessage>(100);
        *self.sender.write().await = Some(tx);
        *self.connected.write().await = true;

        // 消息发送任务
        let write = Arc::new(tokio::sync::Mutex::new(write));
        let write_clone = write.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Ok(json) = serde_json::to_string(&msg) {
                    let mut w = write_clone.lock().await;
                    if let Err(e) = w.send(Message::Text(json)).await {
                        error!("Failed to send message: {}", e);
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    /// 消息接收循环
    async fn message_loop<F, Fut>(&self, on_task_dispatch: F) -> Result<()>
    where
        F: Fn(TaskDispatchData) -> Fut + Send + Sync + Clone + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let url = self.ws_url()?;
        let (ws_stream, _) = connect_async(url.as_str()).await?;
        let (write, mut read) = ws_stream.split();

        // 创建消息发送通道
        let (tx, mut rx) = mpsc::channel::<ClientMessage>(100);
        *self.sender.write().await = Some(tx.clone());

        // 消息发送任务
        let write = Arc::new(tokio::sync::Mutex::new(write));
        let write_clone = write.clone();
        let send_task = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Ok(json) = serde_json::to_string(&msg) {
                    let mut w = write_clone.lock().await;
                    if let Err(e) = w.send(Message::Text(json)).await {
                        error!("Failed to send message: {}", e);
                        break;
                    }
                }
            }
        });

        // 心跳任务
        let heartbeat_interval = self.config.heartbeat_interval;
        let tx_heartbeat = tx.clone();
        let config_clone = self.config.clone();
        let heartbeat_task = tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(heartbeat_interval));
            loop {
                interval.tick().await;
                let system_info = config_clone.get_system_info();
                if tx_heartbeat.send(ClientMessage::Heartbeat { system_info }).await.is_err() {
                    break;
                }
            }
        });

        // 消息接收循环
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    match serde_json::from_str::<ServerMessage>(&text) {
                        Ok(server_msg) => {
                            self.handle_message(server_msg, on_task_dispatch.clone()).await;
                        }
                        Err(e) => {
                            warn!("Failed to parse message: {} - {}", e, text);
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    info!("Server closed connection");
                    break;
                }
                Ok(Message::Ping(data)) => {
                    let mut w = write.lock().await;
                    let _ = w.send(Message::Pong(data)).await;
                }
                Err(e) => {
                    error!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }

        // 清理
        heartbeat_task.abort();
        send_task.abort();
        *self.sender.write().await = None;

        Ok(())
    }

    /// 处理接收到的消息
    async fn handle_message<F, Fut>(&self, message: ServerMessage, on_task_dispatch: F)
    where
        F: Fn(TaskDispatchData) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {
        match message {
            ServerMessage::Connected { message } => {
                info!("Server acknowledged connection: {}", message);
            }
            ServerMessage::HeartbeatAck { server_time } => {
                debug!("Heartbeat acknowledged at {}", server_time);
            }
            ServerMessage::TaskDispatch {
                task_id,
                workspace_name,
                command,
                client_repo_url,
                client_repo_ref,
                timeout,
                environment,
            } => {
                info!("Received task dispatch: {}", task_id);
                let data = TaskDispatchData {
                    task_id,
                    workspace_name: if workspace_name.is_empty() {
                        "default".to_string()
                    } else {
                        workspace_name
                    },
                    command,
                    client_repo_url,
                    client_repo_ref,
                    timeout,
                    environment,
                };
                on_task_dispatch(data).await;
            }
        }
    }

    /// 停止客户端
    pub async fn stop(&self) {
        info!("Stopping agent client...");
        *self.running.write().await = false;
    }
}
