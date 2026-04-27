//! WebSocket 客户端模块
//!
//! 处理与 TaskNexus 服务器的 WebSocket 连接。

use crate::config::{AgentConfig, SystemInfo};
use crate::error::{AgentError, Result};
use crate::persisted_state::PersistedTaskState;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineCode {
    #[serde(default = "default_code_language")]
    pub language: String,
    #[serde(default)]
    pub content: String,
}

fn default_code_language() -> String {
    "shell".to_string()
}

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
    TaskLogAck {
        task_id: i64,
        next_offset: u64,
    },
    StateSyncResult {
        #[serde(default)]
        actions: Vec<StateSyncAction>,
    },
    TaskStateAck {
        task_id: i64,
        status: String,
        accepted: bool,
    },
    TaskDispatch {
        task_id: i64,
        #[serde(default)]
        workspace_name: String,
        #[serde(default = "default_execution_mode")]
        execution_mode: String,
        #[serde(default)]
        command: String,
        #[serde(default)]
        code: Option<InlineCode>,
        #[serde(default)]
        client_repo_url: Option<String>,
        #[serde(default = "default_ref")]
        client_repo_ref: String,
        #[serde(default)]
        client_repo_token: Option<String>,
        #[serde(default = "default_timeout")]
        timeout: u64,
        #[serde(default)]
        environment: HashMap<String, String>,
        #[serde(default)]
        prepare_repo_before_execute: bool,
        #[serde(default)]
        cleanup_workspace_on_success: bool,
    },
    TaskCancel {
        task_id: i64,
    },
    AgentUpdate {
        task_id: i64,
        #[serde(default)]
        download_url: Option<String>,
    },
    FetchAgentLog {
        request_id: String,
        #[serde(default = "default_tail_bytes")]
        tail_bytes: u64,
    },
}

fn default_ref() -> String {
    "main".to_string()
}

fn default_timeout() -> u64 {
    3600
}

fn default_execution_mode() -> String {
    "command".to_string()
}

fn default_tail_bytes() -> u64 {
    256 * 1024
}

/// 客户端发送的消息类型
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Heartbeat {
        system_info: SystemInfo,
    },
    StateSync {
        tasks: Vec<StateSyncTask>,
    },
    TaskStarted {
        task_id: i64,
    },
    TaskLogAppend {
        task_id: i64,
        start_offset: u64,
        content: String,
    },
    TaskLogActive {
        task_id: i64,
        seq: u64,
        base_offset: u64,
        line: String,
        is_stderr: bool,
    },
    TaskLogActiveClear {
        task_id: i64,
        seq: u64,
        base_offset: u64,
    },
    TaskCompleted {
        task_id: i64,
        exit_code: i32,
        stdout: String,
        stderr: String,
        result: HashMap<String, serde_json::Value>,
    },
    TaskFailed {
        task_id: i64,
        error: String,
    },
    TaskHeartbeat {
        task_id: i64,
    },
    AgentLogContent {
        request_id: String,
        content: String,
        file_path: String,
        total_size: u64,
        truncated: bool,
        error: String,
    },
}

/// 任务分发的数据
#[derive(Debug, Clone)]
pub struct TaskDispatchData {
    pub task_id: i64,
    pub workspace_name: String,
    pub execution_mode: String,
    pub command: String,
    pub code: Option<InlineCode>,
    pub client_repo_url: Option<String>,
    pub client_repo_ref: String,
    pub client_repo_token: Option<String>,
    pub timeout: u64,
    pub environment: HashMap<String, String>,
    pub prepare_repo_before_execute: bool,
    pub cleanup_workspace_on_success: bool,
}

#[derive(Debug, Clone)]
pub struct AgentUpdateData {
    pub task_id: i64,
    pub download_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FetchAgentLogData {
    pub request_id: String,
    pub tail_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StateSyncAction {
    pub task_id: i64,
    pub action: String,
    #[serde(default)]
    pub backend_status: String,
    #[serde(default)]
    pub payload: Option<StateSyncPayload>,
    #[serde(default)]
    pub server_log_offset: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StateSyncPayload {
    TaskDispatch {
        task_id: i64,
        #[serde(default)]
        workspace_name: String,
        #[serde(default = "default_execution_mode")]
        execution_mode: String,
        #[serde(default)]
        command: String,
        #[serde(default)]
        code: Option<InlineCode>,
        #[serde(default)]
        client_repo_url: Option<String>,
        #[serde(default = "default_ref")]
        client_repo_ref: String,
        #[serde(default)]
        client_repo_token: Option<String>,
        #[serde(default = "default_timeout")]
        timeout: u64,
        #[serde(default)]
        environment: HashMap<String, String>,
        #[serde(default)]
        prepare_repo_before_execute: bool,
        #[serde(default)]
        cleanup_workspace_on_success: bool,
    },
    AgentUpdate {
        task_id: i64,
        #[serde(default)]
        download_url: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct TaskStateAckData {
    pub task_id: i64,
    pub status: String,
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateSyncTask {
    pub task_id: i64,
    pub local_state: String,
    pub workspace_name: String,
    pub final_payload: serde_json::Value,
    pub has_local_log: bool,
}

impl StateSyncTask {
    pub fn from_persisted(task: &PersistedTaskState) -> Self {
        let local_state = match task.local_state {
            crate::persisted_state::PersistedTaskStateKind::Running => "RUNNING",
            crate::persisted_state::PersistedTaskStateKind::CompletedPendingSync => {
                "COMPLETED_PENDING_SYNC"
            }
            crate::persisted_state::PersistedTaskStateKind::FailedPendingSync => {
                "FAILED_PENDING_SYNC"
            }
            crate::persisted_state::PersistedTaskStateKind::LostOnRestart => "LOST_ON_RESTART",
        }
        .to_string();

        Self {
            task_id: task.task_id,
            local_state,
            workspace_name: task.workspace_name.clone(),
            final_payload: task.final_payload.to_state_sync_payload(),
            has_local_log: !task.local_log_path.is_empty(),
        }
    }
}

#[derive(Debug, Clone)]
struct QueuedLogMessage {
    task_id: i64,
    message: ClientMessage,
}

#[derive(Default)]
struct FairLogQueue {
    queues: HashMap<i64, VecDeque<ClientMessage>>,
    ready_tasks: VecDeque<i64>,
}

impl FairLogQueue {
    fn push(&mut self, task_id: i64, message: ClientMessage) {
        let queue = self.queues.entry(task_id).or_default();
        let was_empty = queue.is_empty();
        queue.push_back(message);
        if was_empty {
            self.ready_tasks.push_back(task_id);
        }
    }

    fn pop(&mut self) -> Option<ClientMessage> {
        while let Some(task_id) = self.ready_tasks.pop_front() {
            let mut remove_task = false;
            let maybe_message = if let Some(queue) = self.queues.get_mut(&task_id) {
                let message = queue.pop_front();
                if queue.is_empty() {
                    remove_task = true;
                } else {
                    self.ready_tasks.push_back(task_id);
                }
                message
            } else {
                None
            };

            if remove_task {
                self.queues.remove(&task_id);
            }

            if maybe_message.is_some() {
                return maybe_message;
            }
        }

        None
    }
}

fn log_task_id(message: &ClientMessage) -> Option<i64> {
    match message {
        ClientMessage::TaskLogAppend { task_id, .. }
        | ClientMessage::TaskLogActive { task_id, .. }
        | ClientMessage::TaskLogActiveClear { task_id, .. }
        | ClientMessage::TaskCompleted { task_id, .. }
        | ClientMessage::TaskFailed { task_id, .. } => Some(*task_id),
        _ => None,
    }
}

/// WebSocket 客户端
#[derive(Clone)]
pub struct AgentClient {
    pub config: AgentConfig,
    connected: Arc<RwLock<bool>>,
    running: Arc<RwLock<bool>>,
    reconnect_attempts: Arc<RwLock<u32>>,
    control_sender: Arc<RwLock<Option<mpsc::Sender<ClientMessage>>>>,
    log_sender: Arc<RwLock<Option<mpsc::Sender<QueuedLogMessage>>>>,
    log_ack_offsets: Arc<RwLock<HashMap<i64, u64>>>,
    connection_generation: Arc<AtomicU64>,
}

impl AgentClient {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            connected: Arc::new(RwLock::new(false)),
            running: Arc::new(RwLock::new(false)),
            reconnect_attempts: Arc::new(RwLock::new(0)),
            control_sender: Arc::new(RwLock::new(None)),
            log_sender: Arc::new(RwLock::new(None)),
            log_ack_offsets: Arc::new(RwLock::new(HashMap::new())),
            connection_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 构建带 name 的 WebSocket URL
    fn ws_url(&self) -> Result<Url> {
        let separator = if self.config.server.contains('?') {
            "&"
        } else {
            "?"
        };
        let url_str = format!(
            "{}{}name={}",
            self.config.server, separator, self.config.name
        );
        Url::parse(&url_str).map_err(AgentError::from)
    }

    async fn send_control_message(&self, message: ClientMessage) -> Result<()> {
        let sender = self.control_sender.read().await;
        if let Some(tx) = sender.as_ref() {
            tx.send(message).await.map_err(|e| {
                AgentError::Connection(format!("Failed to send control message: {}", e))
            })?;
            return Ok(());
        }
        Err(AgentError::Connection(
            "Cannot send control message: not connected".to_string(),
        ))
    }

    async fn send_log_message(&self, message: ClientMessage) -> Result<()> {
        let task_id = log_task_id(&message).ok_or_else(|| {
            AgentError::Connection("Cannot send log message without task context".to_string())
        })?;
        let sender = self.log_sender.read().await;
        if let Some(tx) = sender.as_ref() {
            tx.send(QueuedLogMessage { task_id, message })
                .await
                .map_err(|e| {
                    AgentError::Connection(format!("Failed to send log message: {}", e))
                })?;
            return Ok(());
        }
        Err(AgentError::Connection(
            "Cannot send log message: not connected".to_string(),
        ))
    }

    pub async fn is_connected(&self) -> bool {
        *self.connected.read().await
    }

    pub fn connection_generation(&self) -> u64 {
        self.connection_generation.load(Ordering::SeqCst)
    }

    pub async fn record_task_log_ack(&self, task_id: i64, next_offset: u64) {
        let mut ack_offsets = self.log_ack_offsets.write().await;
        ack_offsets
            .entry(task_id)
            .and_modify(|current| *current = (*current).max(next_offset))
            .or_insert(next_offset);
    }

    pub async fn get_task_log_ack(&self, task_id: i64) -> u64 {
        self.log_ack_offsets
            .read()
            .await
            .get(&task_id)
            .copied()
            .unwrap_or(0)
    }

    pub async fn clear_task_log_ack(&self, task_id: i64) {
        self.log_ack_offsets.write().await.remove(&task_id);
    }

    /// 发送消息到服务器
    pub async fn send_message(&self, message: ClientMessage) -> Result<()> {
        // Keep task output and terminal events in the same queue so their ordering is stable.
        if matches!(
            message,
            ClientMessage::TaskLogAppend { .. }
                | ClientMessage::TaskLogActive { .. }
                | ClientMessage::TaskLogActiveClear { .. }
                | ClientMessage::TaskCompleted { .. }
                | ClientMessage::TaskFailed { .. }
        ) {
            self.send_log_message(message).await
        } else {
            self.send_control_message(message).await
        }
    }

    /// 发送心跳
    pub async fn send_heartbeat(&self) -> Result<()> {
        let system_info = self.config.get_system_info();
        self.send_control_message(ClientMessage::Heartbeat { system_info })
            .await
    }

    pub async fn send_state_sync(&self, tasks: Vec<StateSyncTask>) -> Result<()> {
        self.send_control_message(ClientMessage::StateSync { tasks })
            .await
    }

    /// 发送任务开始通知
    pub async fn send_task_started(&self, task_id: i64) -> Result<()> {
        self.send_message(ClientMessage::TaskStarted { task_id })
            .await
    }

    pub async fn send_task_log_append(
        &self,
        task_id: i64,
        start_offset: u64,
        content: String,
    ) -> Result<()> {
        self.send_log_message(ClientMessage::TaskLogAppend {
            task_id,
            start_offset,
            content,
        })
        .await
    }

    pub async fn send_task_log_active(
        &self,
        task_id: i64,
        seq: u64,
        base_offset: u64,
        line: String,
        is_stderr: bool,
    ) -> Result<()> {
        self.send_log_message(ClientMessage::TaskLogActive {
            task_id,
            seq,
            base_offset,
            line,
            is_stderr,
        })
        .await
    }

    pub async fn send_task_log_active_clear(
        &self,
        task_id: i64,
        seq: u64,
        base_offset: u64,
    ) -> Result<()> {
        self.send_log_message(ClientMessage::TaskLogActiveClear {
            task_id,
            seq,
            base_offset,
        })
        .await
    }

    /// 发送任务完成通知
    pub async fn send_task_completed(
        &self,
        task_id: i64,
        exit_code: i32,
        stdout: String,
        stderr: String,
        result: HashMap<String, serde_json::Value>,
    ) -> Result<()> {
        self.send_message(ClientMessage::TaskCompleted {
            task_id,
            exit_code,
            stdout,
            stderr,
            result,
        })
        .await
    }

    /// 发送任务失败通知
    pub async fn send_task_failed(&self, task_id: i64, error: String) -> Result<()> {
        self.send_message(ClientMessage::TaskFailed { task_id, error })
            .await
    }

    /// 发送任务心跳
    pub async fn send_task_heartbeat(&self, task_id: i64) -> Result<()> {
        self.send_control_message(ClientMessage::TaskHeartbeat { task_id })
            .await
    }

    /// 发送 Agent 系统日志内容
    pub async fn send_agent_log_content(
        &self,
        request_id: String,
        content: String,
        file_path: String,
        total_size: u64,
        truncated: bool,
        error: String,
    ) -> Result<()> {
        self.send_control_message(ClientMessage::AgentLogContent {
            request_id,
            content,
            file_path,
            total_size,
            truncated,
            error,
        })
        .await
    }

    /// 运行客户端主循环
    pub async fn run<F, G, H, I, J, Fut1, Fut2, Fut3, Fut4, Fut5>(
        &self,
        on_task_dispatch: F,
        on_task_cancel: G,
        on_agent_update: H,
        on_state_sync_result: I,
        on_task_state_ack: J,
        on_connected: impl Fn() + Send + Sync + Clone + 'static,
        on_disconnected: impl Fn() + Send + Sync + 'static,
    ) -> Result<()>
    where
        F: Fn(TaskDispatchData) -> Fut1 + Send + Sync + Clone + 'static,
        Fut1: std::future::Future<Output = ()> + Send + 'static,
        G: Fn(i64) -> Fut2 + Send + Sync + Clone + 'static,
        Fut2: std::future::Future<Output = ()> + Send,
        H: Fn(AgentUpdateData) -> Fut3 + Send + Sync + Clone + 'static,
        Fut3: std::future::Future<Output = ()> + Send + 'static,
        I: Fn(Vec<StateSyncAction>) -> Fut4 + Send + Sync + Clone + 'static,
        Fut4: std::future::Future<Output = ()> + Send + 'static,
        J: Fn(TaskStateAckData) -> Fut5 + Send + Sync + Clone + 'static,
        Fut5: std::future::Future<Output = ()> + Send + 'static,
    {
        *self.running.write().await = true;

        while *self.running.read().await {
            match self
                .message_loop(
                    on_task_dispatch.clone(),
                    on_task_cancel.clone(),
                    on_agent_update.clone(),
                    on_state_sync_result.clone(),
                    on_task_state_ack.clone(),
                    on_connected.clone(),
                )
                .await
            {
                Ok(_) => {
                    *self.reconnect_attempts.write().await = 0;
                    let was_connected = *self.connected.read().await;
                    *self.connected.write().await = false;
                    *self.control_sender.write().await = None;
                    *self.log_sender.write().await = None;

                    if was_connected {
                        on_disconnected();
                    }

                    if *self.running.read().await {
                        warn!("Connection lost, will reconnect...");
                        tokio::time::sleep(Duration::from_secs(self.config.reconnect_interval))
                            .await;
                    }
                }
                Err(e) => {
                    error!("Connection/message loop error: {}", e);
                    let was_connected = *self.connected.read().await;
                    *self.connected.write().await = false;
                    *self.control_sender.write().await = None;
                    *self.log_sender.write().await = None;
                    if was_connected {
                        on_disconnected();
                    }

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
                    info!(
                        "Reconnecting in {} seconds... (attempt {})",
                        wait_time, attempts
                    );
                    tokio::time::sleep(Duration::from_secs(wait_time)).await;
                }
            }
        }

        Ok(())
    }

    /// 消息接收循环
    async fn message_loop<F, G, H, I, J, Fut1, Fut2, Fut3, Fut4, Fut5>(
        &self,
        on_task_dispatch: F,
        on_task_cancel: G,
        on_agent_update: H,
        on_state_sync_result: I,
        on_task_state_ack: J,
        on_connected: impl Fn() + Send + Sync + Clone + 'static,
    ) -> Result<()>
    where
        F: Fn(TaskDispatchData) -> Fut1 + Send + Sync + Clone + 'static,
        Fut1: std::future::Future<Output = ()> + Send + 'static,
        G: Fn(i64) -> Fut2 + Send + Sync + Clone + 'static,
        Fut2: std::future::Future<Output = ()> + Send,
        H: Fn(AgentUpdateData) -> Fut3 + Send + Sync + Clone + 'static,
        Fut3: std::future::Future<Output = ()> + Send + 'static,
        I: Fn(Vec<StateSyncAction>) -> Fut4 + Send + Sync + Clone + 'static,
        Fut4: std::future::Future<Output = ()> + Send + 'static,
        J: Fn(TaskStateAckData) -> Fut5 + Send + Sync + Clone + 'static,
        Fut5: std::future::Future<Output = ()> + Send + 'static,
    {
        let url = self.ws_url()?;
        info!("Connecting to {}...", self.config.server);
        let (ws_stream, _) = connect_async(url.as_str()).await?;
        info!("Connected to server successfully");
        *self.connected.write().await = true;
        let connection_generation = self.connection_generation.fetch_add(1, Ordering::SeqCst) + 1;
        debug!(
            "Established websocket connection generation {}",
            connection_generation
        );

        let (write, mut read) = ws_stream.split();

        // 控制/日志分队列，控制消息优先发送
        let (control_tx, mut control_rx) = mpsc::channel::<ClientMessage>(100);
        let (log_tx, mut log_rx) = mpsc::channel::<QueuedLogMessage>(1024);
        *self.control_sender.write().await = Some(control_tx.clone());
        *self.log_sender.write().await = Some(log_tx);

        // 消息发送任务
        let write = Arc::new(tokio::sync::Mutex::new(write));
        let write_clone = write.clone();
        let send_task = tokio::spawn(async move {
            let mut control_closed = false;
            let mut log_closed = false;
            let mut fair_log_queue = FairLogQueue::default();

            loop {
                loop {
                    match control_rx.try_recv() {
                        Ok(msg) => match serde_json::to_string(&msg) {
                            Ok(json) => {
                                let mut w = write_clone.lock().await;
                                if let Err(e) = w.send(Message::Text(json)).await {
                                    error!("Failed to send control message: {}", e);
                                    return;
                                }
                            }
                            Err(e) => {
                                error!("Failed to serialize control message: {}", e);
                            }
                        },
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            control_closed = true;
                            break;
                        }
                    }
                }

                if let Some(msg) = fair_log_queue.pop() {
                    match serde_json::to_string(&msg) {
                        Ok(json) => {
                            let mut w = write_clone.lock().await;
                            if let Err(e) = w.send(Message::Text(json)).await {
                                error!("Failed to send log message: {}", e);
                                return;
                            }
                        }
                        Err(e) => {
                            error!("Failed to serialize log message: {}", e);
                        }
                    }
                    continue;
                }

                if control_closed && log_closed {
                    break;
                }

                tokio::select! {
                    biased;
                    msg = control_rx.recv(), if !control_closed => {
                        match msg {
                            Some(msg) => match serde_json::to_string(&msg) {
                                Ok(json) => {
                                    let mut w = write_clone.lock().await;
                                    if let Err(e) = w.send(Message::Text(json)).await {
                                        error!("Failed to send control message: {}", e);
                                        break;
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to serialize control message: {}", e);
                                }
                            },
                            None => {
                                control_closed = true;
                            }
                        }
                    }
                    msg = log_rx.recv(), if !log_closed => {
                        match msg {
                            Some(msg) => fair_log_queue.push(msg.task_id, msg.message),
                            None => {
                                log_closed = true;
                            }
                        }
                    }
                }
            }
        });

        // agent 心跳任务（控制队列）
        let heartbeat_interval = self.config.heartbeat_interval;
        let control_tx_heartbeat = control_tx.clone();
        let config_clone = self.config.clone();
        let heartbeat_task = tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(heartbeat_interval));
            loop {
                ticker.tick().await;
                let system_info = config_clone.get_system_info();
                if control_tx_heartbeat
                    .send(ClientMessage::Heartbeat { system_info })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        // 消息接收循环
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(server_msg) => {
                        self.handle_message(
                            server_msg,
                            on_task_dispatch.clone(),
                            on_task_cancel.clone(),
                            on_agent_update.clone(),
                            on_state_sync_result.clone(),
                            on_task_state_ack.clone(),
                            on_connected.clone(),
                        )
                        .await;
                    }
                    Err(e) => {
                        warn!("Failed to parse message: {} - {}", e, text);
                    }
                },
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
        *self.control_sender.write().await = None;
        *self.log_sender.write().await = None;

        Ok(())
    }

    /// 处理接收到的消息
    async fn handle_message<F, G, H, I, J, Fut1, Fut2, Fut3, Fut4, Fut5>(
        &self,
        message: ServerMessage,
        on_task_dispatch: F,
        on_task_cancel: G,
        on_agent_update: H,
        on_state_sync_result: I,
        on_task_state_ack: J,
        on_connected: impl Fn() + Send + Sync + 'static,
    ) where
        F: Fn(TaskDispatchData) -> Fut1 + Send + Sync + 'static,
        Fut1: std::future::Future<Output = ()> + Send + 'static,
        G: Fn(i64) -> Fut2 + Send + Sync,
        Fut2: std::future::Future<Output = ()> + Send,
        H: Fn(AgentUpdateData) -> Fut3 + Send + Sync + 'static,
        Fut3: std::future::Future<Output = ()> + Send + 'static,
        I: Fn(Vec<StateSyncAction>) -> Fut4 + Send + Sync + 'static,
        Fut4: std::future::Future<Output = ()> + Send + 'static,
        J: Fn(TaskStateAckData) -> Fut5 + Send + Sync + 'static,
        Fut5: std::future::Future<Output = ()> + Send + 'static,
    {
        match message {
            ServerMessage::Connected { message } => {
                info!("Server acknowledged connection: {}", message);
                on_connected();
            }
            ServerMessage::HeartbeatAck { server_time } => {
                debug!("Heartbeat acknowledged at {}", server_time);
            }
            ServerMessage::TaskLogAck {
                task_id,
                next_offset,
            } => {
                self.record_task_log_ack(task_id, next_offset).await;
            }
            ServerMessage::StateSyncResult { actions } => {
                on_state_sync_result(actions).await;
            }
            ServerMessage::TaskStateAck {
                task_id,
                status,
                accepted,
            } => {
                on_task_state_ack(TaskStateAckData {
                    task_id,
                    status,
                    accepted,
                })
                .await;
            }
            ServerMessage::TaskDispatch {
                task_id,
                workspace_name,
                execution_mode,
                command,
                code,
                client_repo_url,
                client_repo_ref,
                client_repo_token,
                timeout,
                environment,
                prepare_repo_before_execute,
                cleanup_workspace_on_success,
            } => {
                info!("Received task dispatch: {}", task_id);
                let data = TaskDispatchData {
                    task_id,
                    workspace_name: if workspace_name.is_empty() {
                        "default".to_string()
                    } else {
                        workspace_name
                    },
                    execution_mode,
                    command,
                    code,
                    client_repo_url,
                    client_repo_ref,
                    client_repo_token,
                    timeout,
                    environment,
                    prepare_repo_before_execute,
                    cleanup_workspace_on_success,
                };
                // 在后台任务中执行，不阻塞消息接收循环，以便能接收 TaskCancel 消息
                tokio::spawn(async move {
                    on_task_dispatch(data).await;
                });
            }
            ServerMessage::TaskCancel { task_id } => {
                info!("Received task cancel: {}", task_id);
                on_task_cancel(task_id).await;
            }
            ServerMessage::AgentUpdate { task_id, download_url } => {
                info!("Received self-update task {}", task_id);
                let data = AgentUpdateData { task_id, download_url };
                tokio::spawn(async move {
                    on_agent_update(data).await;
                });
            }
            ServerMessage::FetchAgentLog {
                request_id,
                tail_bytes,
            } => {
                debug!(
                    "Received fetch_agent_log request (request_id={}, tail_bytes={})",
                    request_id, tail_bytes
                );
                let client = self.clone();
                tokio::spawn(async move {
                    client.handle_fetch_agent_log(request_id, tail_bytes).await;
                });
            }
        }
    }

    /// 处理系统日志拉取请求
    async fn handle_fetch_agent_log(&self, request_id: String, tail_bytes: u64) {
        let log_file = match &self.config.log_file {
            Some(path) => path.clone(),
            None => {
                warn!("fetch_agent_log: log_file not configured");
                let _ = self
                    .send_agent_log_content(
                        request_id,
                        String::new(),
                        String::new(),
                        0,
                        false,
                        "Agent 未配置 log_file，日志仅输出到 stdout".to_string(),
                    )
                    .await;
                return;
            }
        };

        // tracing_appender::rolling::daily 生成的文件名格式为
        // {prefix}.{YYYY-MM-DD}，定位当天的日志文件
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let parent = log_file
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let file_stem = log_file
            .file_name()
            .unwrap_or(std::ffi::OsStr::new("agent.log"));
        let daily_file = parent.join(format!(
            "{}.{}",
            file_stem.to_string_lossy(),
            today
        ));

        // 尝试当天的日志文件，如果不存在则尝试原始路径
        let actual_path = if tokio::fs::try_exists(&daily_file).await.unwrap_or(false) {
            daily_file
        } else if tokio::fs::try_exists(&log_file).await.unwrap_or(false) {
            log_file.clone()
        } else {
            warn!(
                "fetch_agent_log: log file not found at {:?} or {:?}",
                daily_file, log_file
            );
            let _ = self
                .send_agent_log_content(
                    request_id,
                    String::new(),
                    log_file.to_string_lossy().to_string(),
                    0,
                    false,
                    format!("日志文件不存在: {}", log_file.display()),
                )
                .await;
            return;
        };

        let file_path_str = actual_path.to_string_lossy().to_string();
        debug!("fetch_agent_log: reading log file {:?}", actual_path);

        match tokio::fs::read(&actual_path).await {
            Ok(data) => {
                let total_size = data.len() as u64;
                let tail = (tail_bytes as usize).min(data.len());
                let offset = data.len().saturating_sub(tail);
                let truncated = offset > 0;
                let slice = &data[offset..];

                // 确保 UTF-8 安全截断：跳过不完整的首行
                let content = String::from_utf8_lossy(slice);
                let content = if truncated {
                    // 跳过第一个不完整的行
                    if let Some(pos) = content.find('\n') {
                        content[pos + 1..].to_string()
                    } else {
                        content.to_string()
                    }
                } else {
                    content.to_string()
                };

                debug!(
                    "fetch_agent_log: sending {} bytes of log content (total_size={}, truncated={})",
                    content.len(),
                    total_size,
                    truncated
                );

                if let Err(e) = self
                    .send_agent_log_content(
                        request_id,
                        content,
                        file_path_str,
                        total_size,
                        truncated,
                        String::new(),
                    )
                    .await
                {
                    error!("Failed to send agent log content: {}", e);
                }
            }
            Err(e) => {
                error!("Failed to read log file {:?}: {}", actual_path, e);
                let _ = self
                    .send_agent_log_content(
                        request_id,
                        String::new(),
                        file_path_str,
                        0,
                        false,
                        format!("读取日志文件失败: {}", e),
                    )
                    .await;
            }
        }
    }

    /// 停止客户端
    pub async fn stop(&self) {
        info!("Stopping agent client...");
        *self.running.write().await = false;
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientMessage, FairLogQueue};
    use std::collections::HashMap;

    fn append(task_id: i64, start_offset: u64) -> ClientMessage {
        ClientMessage::TaskLogAppend {
            task_id,
            start_offset,
            content: format!("chunk-{}", start_offset),
        }
    }

    #[test]
    fn fair_log_queue_round_robins_between_tasks() {
        let mut queue = FairLogQueue::default();
        queue.push(1, append(1, 0));
        queue.push(1, append(1, 10));
        queue.push(2, append(2, 0));
        queue.push(3, append(3, 0));
        queue.push(2, append(2, 10));

        let mut order = Vec::new();
        while let Some(message) = queue.pop() {
            match message {
                ClientMessage::TaskLogAppend {
                    task_id,
                    start_offset,
                    ..
                } => order.push((task_id, start_offset)),
                _ => unreachable!("unexpected message type"),
            }
        }

        assert_eq!(order, vec![(1, 0), (2, 0), (3, 0), (1, 10), (2, 10)]);
    }

    #[test]
    fn fair_log_queue_preserves_order_within_each_task() {
        let mut queue = FairLogQueue::default();
        let mut expected = HashMap::new();
        expected.insert(7, vec![0, 5, 10]);
        expected.insert(8, vec![0, 9]);

        queue.push(7, append(7, 0));
        queue.push(8, append(8, 0));
        queue.push(7, append(7, 5));
        queue.push(7, append(7, 10));
        queue.push(8, append(8, 9));

        let mut actual: HashMap<i64, Vec<u64>> = HashMap::new();
        while let Some(message) = queue.pop() {
            match message {
                ClientMessage::TaskLogAppend {
                    task_id,
                    start_offset,
                    ..
                } => actual.entry(task_id).or_default().push(start_offset),
                _ => unreachable!("unexpected message type"),
            }
        }

        assert_eq!(actual, expected);
    }
}
