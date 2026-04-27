//! TaskNexus Agent 主入口
//!
//! 提供命令行接口和 Agent 运行逻辑。

use chrono::Local;
use clap::Parser;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{watch, Mutex, RwLock};
use tokio::time::{interval, Duration, Instant};
use tracing::{error, info, warn, Level};
use tracing_subscriber::{fmt, EnvFilter};

use tasknexus_agent::{
    client::{
        AgentClient, AgentRestartData, AgentUpdateData, StateSyncAction, StateSyncPayload,
        StateSyncTask, TaskDispatchData, TaskStateAckData,
    },
    config::{load_config, AgentConfig},
    executor::TaskRunner,
    persisted_state::PersistedStateStore,
    self_update,
    service,
};

const MAX_LOG_CHUNK_BYTES: usize = 64 * 1024;
const LOG_APPEND_INTERVAL_MS: u64 = 200;
const LOG_SYNC_INTERVAL_MS: u64 = 25;
const LOG_ACTIVE_DEBOUNCE_MS: u64 = 125;
const LOG_APPEND_ACK_TIMEOUT_MS: u64 = 1000;
const LOG_FINAL_SYNC_TIMEOUT_MS: u64 = 5000;
const MAX_STORED_OUTPUT_CHARS: usize = 16 * 1024;
const TASK_LOG_DIR_NAME: &str = ".tasknexus_task_logs";

#[derive(Clone, Debug, PartialEq, Eq)]
struct VisibleLine {
    text: String,
    is_stderr: bool,
}

#[derive(Clone, Debug)]
enum PendingActiveEventKind {
    Set(VisibleLine),
    Clear,
}

#[derive(Clone, Debug)]
struct PendingActiveEvent {
    seq: u64,
    kind: PendingActiveEventKind,
}

struct InflightAppend {
    start_offset: u64,
    end_offset: u64,
    sent_at: Instant,
    connection_generation: u64,
}

struct TaskLogSyncState {
    task_id: i64,
    local_log_path: PathBuf,
    writer: BufWriter<File>,
    committed_offset: u64,
    acked_offset: u64,
    line_timestamp: Option<String>,
    line_buffer: String,
    line_is_stderr: bool,
    active_display: Option<VisibleLine>,
    pending_active_event: Option<PendingActiveEvent>,
    inflight_append: Option<InflightAppend>,
    next_active_seq: u64,
    last_append_flush: Instant,
    last_active_change: Option<Instant>,
}

impl TaskLogSyncState {
    fn new(workspaces_path: &PathBuf, task_id: i64) -> std::io::Result<Self> {
        let log_dir = workspaces_path.join(TASK_LOG_DIR_NAME);
        fs::create_dir_all(&log_dir)?;
        let local_log_path = log_dir.join(format!("task_{}.log", task_id));
        let file = File::create(&local_log_path)?;

        Ok(Self {
            task_id,
            local_log_path,
            writer: BufWriter::new(file),
            committed_offset: 0,
            acked_offset: 0,
            line_timestamp: None,
            line_buffer: String::new(),
            line_is_stderr: false,
            active_display: None,
            pending_active_event: None,
            inflight_append: None,
            next_active_seq: 0,
            last_append_flush: Instant::now(),
            last_active_change: None,
        })
    }

    fn ingest_chunk(&mut self, chunk: &str, is_stderr: bool) -> std::io::Result<()> {
        for ch in chunk.chars() {
            match ch {
                '\r' => {
                    if !self.line_buffer.is_empty() {
                        self.refresh_active_from_buffer();
                        self.line_buffer.clear();
                    }
                }
                '\n' => {
                    if !self.line_buffer.is_empty() {
                        let visible = self
                            .build_visible_from_buffer()
                            .unwrap_or_else(|| self.build_blank_visible(is_stderr));
                        self.commit_visible_line(visible)?;
                    } else if let Some(visible) = self.active_display.clone() {
                        self.commit_visible_line(visible)?;
                    } else {
                        let visible = self.build_blank_visible(is_stderr);
                        self.commit_visible_line(visible)?;
                    }
                    self.reset_line_state();
                }
                _ => {
                    self.ensure_line_timestamp();
                    if self.line_buffer.is_empty() {
                        self.line_is_stderr = is_stderr;
                    }
                    self.line_buffer.push(ch);
                    self.refresh_active_from_buffer();
                }
            }
        }

        Ok(())
    }

    fn finalize_pending(&mut self) -> std::io::Result<()> {
        if !self.line_buffer.is_empty() {
            if let Some(visible) = self.build_visible_from_buffer() {
                self.commit_visible_line(visible)?;
            }
            self.reset_line_state();
            return Ok(());
        }

        if let Some(visible) = self.active_display.clone() {
            self.commit_visible_line(visible)?;
            self.reset_line_state();
        }

        Ok(())
    }

    async fn sync_with_server(
        &mut self,
        client: &AgentClient,
        force_append: bool,
        force_active: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.reconcile_ack(client).await;
        self.retry_inflight_append_if_needed(client).await?;

        let backlog = self.committed_offset.saturating_sub(self.acked_offset);
        let should_flush_append = backlog > 0
            && (force_append
                || backlog >= MAX_LOG_CHUNK_BYTES as u64
                || self.last_append_flush.elapsed()
                    >= Duration::from_millis(LOG_APPEND_INTERVAL_MS));

        if self.inflight_append.is_none() && should_flush_append {
            self.flush_local_log_writer()?;
            let (content, byte_len) =
                read_utf8_chunk(&self.local_log_path, self.acked_offset, MAX_LOG_CHUNK_BYTES)?;
            if byte_len > 0 && !content.is_empty() {
                let connection_generation = client.connection_generation();
                client
                    .send_task_log_append(self.task_id, self.acked_offset, content)
                    .await?;
                self.inflight_append = Some(InflightAppend {
                    start_offset: self.acked_offset,
                    end_offset: self.acked_offset + byte_len as u64,
                    sent_at: Instant::now(),
                    connection_generation,
                });
                self.last_append_flush = Instant::now();
            }
        }

        self.reconcile_ack(client).await;
        if self.inflight_append.is_some() || self.acked_offset != self.committed_offset {
            return Ok(());
        }

        let should_flush_active = if let Some(last_change) = self.last_active_change {
            force_active || last_change.elapsed() >= Duration::from_millis(LOG_ACTIVE_DEBOUNCE_MS)
        } else {
            false
        };

        if !should_flush_active {
            return Ok(());
        }

        let pending = match self.pending_active_event.clone() {
            Some(value) => value,
            None => return Ok(()),
        };

        match pending.kind {
            PendingActiveEventKind::Set(visible) => {
                client
                    .send_task_log_active(
                        self.task_id,
                        pending.seq,
                        self.acked_offset,
                        visible.text.clone(),
                        visible.is_stderr,
                    )
                    .await?;
            }
            PendingActiveEventKind::Clear => {
                client
                    .send_task_log_active_clear(self.task_id, pending.seq, self.acked_offset)
                    .await?;
            }
        }

        self.pending_active_event = None;
        self.last_active_change = None;
        Ok(())
    }

    fn ensure_line_timestamp(&mut self) {
        if self.line_timestamp.is_none() {
            self.line_timestamp = Some(Local::now().format("%Y-%m-%dT%H:%M:%S%.3f").to_string());
        }
    }

    fn refresh_active_from_buffer(&mut self) {
        let visible = self.build_visible_from_buffer();
        self.update_active_display(visible);
    }

    fn build_visible_from_buffer(&self) -> Option<VisibleLine> {
        let timestamp = self.line_timestamp.as_ref()?;
        Some(VisibleLine {
            text: format!("[{}] {}", timestamp, self.line_buffer),
            is_stderr: self.line_is_stderr,
        })
    }

    fn build_blank_visible(&self, is_stderr: bool) -> VisibleLine {
        let timestamp = self
            .line_timestamp
            .clone()
            .unwrap_or_else(|| Local::now().format("%Y-%m-%dT%H:%M:%S%.3f").to_string());
        VisibleLine {
            text: format!("[{}] ", timestamp),
            is_stderr,
        }
    }

    fn commit_visible_line(&mut self, visible: VisibleLine) -> std::io::Result<()> {
        let record = format!("{}\n", visible.text);
        let bytes = record.as_bytes();
        self.writer.write_all(bytes)?;
        self.committed_offset += bytes.len() as u64;
        self.update_active_display(None);
        Ok(())
    }

    fn reset_line_state(&mut self) {
        self.line_timestamp = None;
        self.line_buffer.clear();
        self.line_is_stderr = false;
    }

    fn update_active_display(&mut self, next: Option<VisibleLine>) {
        if self.active_display == next {
            return;
        }

        self.active_display = next.clone();
        self.next_active_seq += 1;
        self.pending_active_event = Some(PendingActiveEvent {
            seq: self.next_active_seq,
            kind: match next {
                Some(visible) => PendingActiveEventKind::Set(visible),
                None => PendingActiveEventKind::Clear,
            },
        });
        self.last_active_change = Some(Instant::now());
    }

    fn flush_local_log_writer(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }

    async fn reconcile_ack(&mut self, client: &AgentClient) {
        let acked_offset = client.get_task_log_ack(self.task_id).await;
        if acked_offset > self.acked_offset {
            self.acked_offset = acked_offset;
        }

        if self
            .inflight_append
            .as_ref()
            .map(|inflight| self.acked_offset >= inflight.end_offset)
            .unwrap_or(false)
        {
            self.inflight_append = None;
        }
    }

    async fn retry_inflight_append_if_needed(
        &mut self,
        client: &AgentClient,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Some((start_offset, end_offset, previous_generation, previous_sent_at)) =
            self.inflight_append.as_ref().map(|inflight| {
                (
                    inflight.start_offset,
                    inflight.end_offset,
                    inflight.connection_generation,
                    inflight.sent_at,
                )
            })
        else {
            return Ok(());
        };

        if !client.is_connected().await {
            return Ok(());
        }

        let connection_generation = client.connection_generation();
        let should_resend = connection_generation != previous_generation
            || previous_sent_at.elapsed() >= Duration::from_millis(LOG_APPEND_ACK_TIMEOUT_MS);
        if !should_resend {
            return Ok(());
        }

        self.flush_local_log_writer()?;
        let max_bytes = end_offset.saturating_sub(start_offset) as usize;
        let (content, byte_len) = read_utf8_chunk(&self.local_log_path, start_offset, max_bytes)?;
        if byte_len == 0 || content.is_empty() {
            return Ok(());
        }

        client
            .send_task_log_append(self.task_id, start_offset, content)
            .await?;
        if let Some(inflight) = self.inflight_append.as_mut() {
            inflight.sent_at = Instant::now();
            inflight.connection_generation = connection_generation;
        }
        Ok(())
    }

    fn is_fully_synced(&self) -> bool {
        self.acked_offset == self.committed_offset
            && self.inflight_append.is_none()
            && self.pending_active_event.is_none()
    }
}

fn trim_output_for_storage(content: String) -> String {
    let char_count = content.chars().count();
    if char_count <= MAX_STORED_OUTPUT_CHARS {
        return content;
    }

    let tail: String = content
        .chars()
        .skip(char_count - MAX_STORED_OUTPUT_CHARS)
        .collect();
    format!(
        "[task output truncated, showing last {} chars]\n{}",
        MAX_STORED_OUTPUT_CHARS, tail
    )
}

fn read_utf8_chunk(
    path: &PathBuf,
    start_offset: u64,
    max_bytes: usize,
) -> std::io::Result<(String, usize)> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start_offset))?;

    let mut raw = vec![0u8; max_bytes];
    let bytes_read = file.read(&mut raw)?;
    raw.truncate(bytes_read);
    if raw.is_empty() {
        return Ok((String::new(), 0));
    }

    let mut valid_len = raw.len();
    while valid_len > 0 && std::str::from_utf8(&raw[..valid_len]).is_err() {
        valid_len -= 1;
    }

    if valid_len == 0 {
        return Ok((String::new(), 0));
    }

    let content = String::from_utf8(raw[..valid_len].to_vec())
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?;

    Ok((content, valid_len))
}

/// TaskNexus Agent - 客户端代理
#[derive(Parser, Debug)]
#[command(name = "tasknexus-agent")]
#[command(about = "TaskNexus Agent - 连接到 TaskNexus 服务器执行远程任务")]
#[command(version)]
enum Cli {
    /// 运行 Agent
    Run {
        /// 配置文件路径
        #[arg(short, long)]
        config: PathBuf,
        /// 服务名称（由 service install 自动设置，请勿手动指定）
        #[arg(long, hide = true, default_value = service::DEFAULT_SERVICE_NAME)]
        service_name: String,
    },
    /// 系统服务管理
    Service {
        #[command(subcommand)]
        action: service::ServiceAction,
    },
}

/// 配置日志
///
/// 返回的 `WorkerGuard` 必须在程序生命周期内保持存活，
/// 否则 non-blocking writer 会被 drop，文件日志停止写入。
fn setup_logging(log_level: &str, log_file: Option<&PathBuf>) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let level = match log_level.to_uppercase().as_str() {
        "TRACE" => Level::TRACE,
        "DEBUG" => Level::DEBUG,
        "INFO" => Level::INFO,
        "WARN" | "WARNING" => Level::WARN,
        "ERROR" => Level::ERROR,
        _ => Level::INFO,
    };

    let filter = EnvFilter::from_default_env()
        .add_directive(level.into())
        .add_directive("tokio_tungstenite=warn".parse().unwrap())
        .add_directive("tungstenite=warn".parse().unwrap());

    let subscriber = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false);

    if let Some(log_path) = log_file {
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file_appender = tracing_appender::rolling::daily(
            log_path.parent().unwrap_or(std::path::Path::new(".")),
            log_path
                .file_name()
                .unwrap_or(std::ffi::OsStr::new("agent.log")),
        );
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        subscriber.with_writer(non_blocking).init();
        Some(guard)
    } else {
        subscriber.init();
        None
    }
}

/// Agent 主结构
struct Agent {
    config: AgentConfig,
    task_runner: TaskRunner,
    client: AgentClient,
    /// Maps task_id -> (workspace_name, cancel_sender, local_log_path)
    running_tasks: Arc<RwLock<HashMap<i64, (String, watch::Sender<bool>, PathBuf)>>>,
    persisted_state: Arc<Mutex<PersistedStateStore>>,
    update_in_progress: Arc<RwLock<bool>>,
}

impl Agent {
    fn new(
        config: AgentConfig,
        persisted_state: PersistedStateStore,
    ) -> Self {
        let task_runner = TaskRunner::new(config.workspaces_path.clone(), config.proxy_env());
        let client = AgentClient::new(config.clone());

        Self {
            config,
            task_runner,
            client,
            running_tasks: Arc::new(RwLock::new(HashMap::new())),
            persisted_state: Arc::new(Mutex::new(persisted_state)),
            update_in_progress: Arc::new(RwLock::new(false)),
        }
    }

    async fn flush_task_logs_until_synced(
        &self,
        log_sync_state: &Arc<Mutex<TaskLogSyncState>>,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;

        loop {
            let is_synced = {
                let mut guard = log_sync_state.lock().await;
                if let Err(e) = guard.sync_with_server(&self.client, true, true).await {
                    warn!("Failed to sync task log state for {}: {}", guard.task_id, e);
                }
                guard.is_fully_synced()
            };

            if is_synced {
                return true;
            }

            if Instant::now() >= deadline {
                return false;
            }

            tokio::time::sleep(Duration::from_millis(LOG_SYNC_INTERVAL_MS)).await;
        }
    }

    async fn append_manual_task_log_line(
        &self,
        log_sync_state: &Arc<Mutex<TaskLogSyncState>>,
        line: &str,
        is_stderr: bool,
    ) {
        {
            let mut guard = log_sync_state.lock().await;
            if let Err(e) = guard.ingest_chunk(&format!("{}\n", line), is_stderr) {
                error!("Failed to write manual task log line: {}", e);
                return;
            }
        }

        let synced = self
            .flush_task_logs_until_synced(
                log_sync_state,
                Duration::from_millis(LOG_FINAL_SYNC_TIMEOUT_MS),
            )
            .await;
        if !synced {
            warn!("Timed out while syncing manual task log line for {}", line);
        }
    }

    async fn save_persisted_state(&self) {
        let store = self.persisted_state.lock().await;
        if let Err(e) = store.save() {
            error!("Failed to persist agent task state: {}", e);
        }
    }

    async fn send_state_sync_snapshot(&self) {
        let mut tasks = {
            let store = self.persisted_state.lock().await;
            store
                .snapshot()
                .iter()
                .map(StateSyncTask::from_persisted)
                .collect::<Vec<_>>()
        };
        let running = self.running_tasks.read().await;
        for (task_id, (workspace_name, _, log_path)) in running.iter() {
            if tasks.iter().any(|task| task.task_id == *task_id) {
                continue;
            }
            tasks.push(StateSyncTask {
                task_id: *task_id,
                local_state: "RUNNING".to_string(),
                workspace_name: workspace_name.clone(),
                final_payload: serde_json::Value::Object(serde_json::Map::new()),
                has_local_log: !log_path.as_os_str().is_empty(),
            });
        }
        tasks.sort_by_key(|task| task.task_id);

        if let Err(e) = self.client.send_state_sync(tasks).await {
            warn!("Failed to send state sync snapshot: {}", e);
        }
    }

    async fn clear_persisted_task_state(&self, task_id: i64) {
        let removed = {
            let mut store = self.persisted_state.lock().await;
            store.remove(task_id)
        };
        self.save_persisted_state().await;

        let local_log_path = removed
            .map(|state| state.local_log_path)
            .filter(|path| !path.is_empty());
        let is_running = self.running_tasks.read().await.contains_key(&task_id);
        if !is_running {
            if let Some(path) = local_log_path {
                self.cleanup_persisted_task_files(&path);
            }
        }
    }

    async fn handle_state_sync_result(&self, actions: Vec<StateSyncAction>) {
        for action in actions {
            match action.action.as_str() {
                "start" => {
                    if let Some(payload) = action.payload {
                        match payload {
                            StateSyncPayload::TaskDispatch {
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
                                self.client.clear_task_log_ack(task_id).await;
                                self.clear_persisted_task_state(task_id).await;
                                self.handle_task_dispatch(TaskDispatchData {
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
                                })
                                .await;
                            }
                            StateSyncPayload::AgentUpdate { task_id, download_url } => {
                                self.handle_agent_update(AgentUpdateData { task_id, download_url }).await;
                            }
                            StateSyncPayload::AgentRestart { task_id } => {
                                self.handle_agent_restart(AgentRestartData { task_id }).await;
                            }
                        }
                    }
                }
                "keep_running" => {
                    self.client
                        .record_task_log_ack(action.task_id, action.server_log_offset)
                        .await;
                }
                "cancel" => {
                    self.handle_task_cancel(action.task_id).await;
                    self.clear_persisted_task_state(action.task_id).await;
                }
                "clear" => {
                    self.clear_persisted_task_state(action.task_id).await;
                }
                other => {
                    warn!(
                        "Ignoring unknown state sync action '{}' for task {}",
                        other, action.task_id
                    );
                }
            }
        }
    }

    async fn handle_task_state_ack(&self, ack: TaskStateAckData) {
        if ack.accepted {
            match ack.status.as_str() {
                "RUNNING" => {
                    return;
                }
                "COMPLETED" | "FAILED" | "CANCELLED" | "TIMEOUT" => {
                    self.clear_persisted_task_state(ack.task_id).await;
                }
                _ => {}
            }
            return;
        }

        if matches!(
            ack.status.as_str(),
            "COMPLETED" | "FAILED" | "CANCELLED" | "TIMEOUT"
        ) {
            if self.running_tasks.read().await.contains_key(&ack.task_id) {
                self.handle_task_cancel(ack.task_id).await;
            }
            self.clear_persisted_task_state(ack.task_id).await;
        }
    }

    fn cleanup_persisted_task_files(&self, local_log_path: &str) {
        if local_log_path.is_empty() {
            return;
        }
        if let Err(e) = fs::remove_file(local_log_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    "Failed to cleanup persisted local task log '{}': {}",
                    local_log_path, e
                );
            }
        }
    }

    async fn start(self) -> Result<(), Box<dyn std::error::Error>> {
        info!("Starting TaskNexus Agent: {}", self.config.name);
        info!("Server: {}", self.config.server);
        info!("Workspaces path: {:?}", self.config.workspaces_path);

        // 确保工作目录存在
        std::fs::create_dir_all(&self.config.workspaces_path)?;

        let agent = Arc::new(self);
        let agent_clone = agent.clone();
        let agent_cancel = agent.clone();
        let agent_update = agent.clone();
        let agent_restart = agent.clone();
        let agent_sync = agent.clone();
        let agent_ack = agent.clone();

        // 运行客户端
        agent
            .client
            .run(
                move |data| {
                    let agent = agent_clone.clone();
                    async move {
                        agent.handle_task_dispatch(data).await;
                    }
                },
                move |task_id| {
                    let agent = agent_cancel.clone();
                    async move {
                        agent.handle_task_cancel(task_id).await;
                    }
                },
                move |data| {
                    let agent = agent_update.clone();
                    async move {
                        agent.handle_agent_update(data).await;
                    }
                },
                move |data| {
                    let agent = agent_restart.clone();
                    async move {
                        agent.handle_agent_restart(data).await;
                    }
                },
                move |actions| {
                    let agent = agent_sync.clone();
                    async move {
                        agent.handle_state_sync_result(actions).await;
                    }
                },
                move |ack| {
                    let agent = agent_ack.clone();
                    async move {
                        agent.handle_task_state_ack(ack).await;
                    }
                },
                {
                    let agent = agent.clone();
                    move || {
                        let agent = agent.clone();
                        tokio::spawn(async move {
                            info!("Connected to TaskNexus server");
                            agent.send_state_sync_snapshot().await;
                        });
                    }
                },
                || warn!("Disconnected from TaskNexus server"),
            )
            .await?;

        Ok(())
    }

    async fn handle_task_dispatch(&self, data: TaskDispatchData) {
        let task_id = data.task_id;
        let workspace_name = data.workspace_name.clone();
        let execution_mode = data.execution_mode.clone();
        let command = data.command.clone();

        if *self.update_in_progress.read().await {
            warn!(
                "Reject task {} because self-update is currently in progress",
                task_id
            );
            let _ = self
                .client
                .send_task_failed(task_id, "Agent is updating; task rejected".to_string())
                .await;
            return;
        }

        if execution_mode == "code" {
            let language = data
                .code
                .as_ref()
                .map(|item| item.language.as_str())
                .unwrap_or("unknown");
            info!(
                "Received task {} for workspace '{}' in code mode (language={})",
                task_id, workspace_name, language
            );
        } else {
            info!(
                "Received task {} for workspace '{}': {}",
                task_id, workspace_name, command
            );
        }

        // 防止重复分发同一个 task_id（例如重试场景）
        {
            let running = self.running_tasks.read().await;
            if running.contains_key(&task_id) {
                warn!(
                    "Task {} is already running, ignore duplicate dispatch",
                    task_id
                );
                return;
            }
        }

        // 创建取消信号通道
        let (cancel_tx, cancel_rx) = watch::channel(false);

        // 标记任务正在运行
        self.running_tasks
            .write()
            .await
            .insert(task_id, (workspace_name.clone(), cancel_tx, PathBuf::new()));

        // 通知任务开始
        if let Err(e) = self.client.send_task_started(task_id).await {
            error!("Failed to send task started: {}", e);
        }

        self.client.clear_task_log_ack(task_id).await;
        let log_sync_state = match TaskLogSyncState::new(&self.config.workspaces_path, task_id) {
            Ok(state) => Arc::new(Mutex::new(state)),
            Err(e) => {
                error!(
                    "Failed to initialize task log sync state for {}: {}",
                    task_id, e
                );
                let _ = self
                    .client
                    .send_task_failed(task_id, format!("Failed to initialize task logs: {}", e))
                    .await;
                self.running_tasks.write().await.remove(&task_id);
                return;
            }
        };
        {
            let log_path = log_sync_state.lock().await.local_log_path.clone();
            if let Some((_, _, existing_log_path)) =
                self.running_tasks.write().await.get_mut(&task_id)
            {
                *existing_log_path = log_path.clone();
            }
            let mut store = self.persisted_state.lock().await;
            store.upsert_running(task_id, workspace_name.clone(), log_path);
        }
        self.save_persisted_state().await;

        // 启动任务心跳发送器
        let heartbeat_client = self.client.clone();
        let mut heartbeat_cancel_rx = cancel_rx.clone();
        let heartbeat_task = tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(e) = heartbeat_client.send_task_heartbeat(task_id).await {
                            warn!("Failed to send task heartbeat: {}", e);
                        }
                    }
                    _ = heartbeat_cancel_rx.changed() => {
                        break;
                    }
                }
            }
        });

        let state_for_callback = log_sync_state.clone();
        let output_callback = move |line: String, is_stderr: bool| {
            let state = state_for_callback.clone();
            async move {
                let mut guard = state.lock().await;
                if let Err(e) = guard.ingest_chunk(&line, is_stderr) {
                    error!("Failed to buffer task log chunk: {}", e);
                }
            }
        };

        let flush_client = self.client.clone();
        let flush_state = log_sync_state.clone();
        let log_flush_task = tokio::spawn(async move {
            let mut tick = interval(Duration::from_millis(LOG_SYNC_INTERVAL_MS));
            loop {
                tick.tick().await;
                let mut guard = flush_state.lock().await;
                let _ = guard.sync_with_server(&flush_client, false, false).await;
            }
        });

        // 执行任务，传入取消信号
        let result = self
            .task_runner
            .run_task(
                task_id,
                &data.execution_mode,
                &data.command,
                data.code.as_ref(),
                &data.workspace_name,
                data.client_repo_url.as_deref(),
                &data.client_repo_ref,
                data.client_repo_token.as_deref(),
                data.prepare_repo_before_execute,
                data.cleanup_workspace_on_success,
                data.timeout,
                Some(output_callback),
                if data.environment.is_empty() {
                    None
                } else {
                    Some(data.environment)
                },
                Some(cancel_rx),
            )
            .await;

        // 停止日志批量发送和心跳
        log_flush_task.abort();
        heartbeat_task.abort();

        let (local_log_flush_succeeded, local_log_path) = {
            let mut guard = log_sync_state.lock().await;
            let finalize_ok = if let Err(e) = guard.finalize_pending() {
                error!("Failed to finalize task log state for {}: {}", task_id, e);
                false
            } else {
                true
            };
            let local_log_path = guard.local_log_path.clone();
            (finalize_ok, local_log_path)
        };
        let fully_synced = self
            .flush_task_logs_until_synced(
                &log_sync_state,
                Duration::from_millis(LOG_FINAL_SYNC_TIMEOUT_MS),
            )
            .await;
        let local_log_flush_succeeded = local_log_flush_succeeded && fully_synced;

        {
            let mut store = self.persisted_state.lock().await;
            if result.cancelled {
                store.remove(task_id);
            } else if result.exit_code == 0 {
                store.mark_completed(
                    task_id,
                    workspace_name.clone(),
                    local_log_path.clone(),
                    result.exit_code,
                    trim_output_for_storage(result.stdout.clone()),
                    trim_output_for_storage(result.stderr.clone()),
                    result.result.clone(),
                );
            } else if result.timed_out {
                store.mark_failed(
                    task_id,
                    workspace_name.clone(),
                    local_log_path.clone(),
                    format!("Task timed out after {} seconds", data.timeout),
                );
            } else {
                store.mark_completed(
                    task_id,
                    workspace_name.clone(),
                    local_log_path.clone(),
                    result.exit_code,
                    trim_output_for_storage(result.stdout.clone()),
                    trim_output_for_storage(result.stderr.clone()),
                    result.result.clone(),
                );
            }
        }
        self.save_persisted_state().await;

        // 发送结果
        if result.cancelled {
            info!(
                "Task {} was cancelled, not sending completion (server already handled)",
                task_id
            );
        } else if result.exit_code == 0 {
            if let Err(e) = self
                .client
                .send_task_completed(
                    task_id,
                    result.exit_code,
                    trim_output_for_storage(result.stdout),
                    trim_output_for_storage(result.stderr),
                    result.result,
                )
                .await
            {
                error!("Failed to send task completed: {}", e);
            }
            info!("Task {} completed successfully", task_id);
        } else {
            if result.timed_out {
                if let Err(e) = self
                    .client
                    .send_task_failed(
                        task_id,
                        format!("Task timed out after {} seconds", data.timeout),
                    )
                    .await
                {
                    error!("Failed to send task failed: {}", e);
                }
            } else {
                if let Err(e) = self
                    .client
                    .send_task_completed(
                        task_id,
                        result.exit_code,
                        trim_output_for_storage(result.stdout),
                        trim_output_for_storage(result.stderr),
                        result.result,
                    )
                    .await
                {
                    error!("Failed to send task completed: {}", e);
                }
            }
            warn!(
                "Task {} failed with exit code {}",
                task_id, result.exit_code
            );
        }

        let should_cleanup_local_log = {
            let store = self.persisted_state.lock().await;
            store.get(task_id).is_none()
        };

        if local_log_flush_succeeded && should_cleanup_local_log {
            self.cleanup_persisted_task_files(local_log_path.to_string_lossy().as_ref());
            info!(
                "Cleaned up local task log for {} at {:?}",
                task_id, local_log_path
            );
        } else if !local_log_flush_succeeded {
            warn!(
                "Keeping local task log for {} at {:?} because final log flush did not complete successfully",
                task_id, local_log_path
            );
        } else {
            info!(
                "Keeping local task log for {} at {:?} until server ack clears persisted state",
                task_id, local_log_path
            );
        }

        // 移除运行中的任务
        self.client.clear_task_log_ack(task_id).await;
        self.running_tasks.write().await.remove(&task_id);
    }

    async fn handle_task_cancel(&self, task_id: i64) {
        info!("Processing cancel for task {}", task_id);
        let running = self.running_tasks.read().await;
        if let Some((workspace_name, cancel_tx, _)) = running.get(&task_id) {
            info!(
                "Sending cancel signal to task {} in workspace '{}'",
                task_id, workspace_name
            );
            let _ = cancel_tx.send(true);
            return;
        }
        warn!("Task {} not found in running tasks, cannot cancel", task_id);
    }

    async fn handle_agent_update(&self, data: AgentUpdateData) {
        let task_id = data.task_id;

        {
            let mut flag = self.update_in_progress.write().await;
            if *flag {
                warn!(
                    "Ignore duplicate self-update task {} because update is already running",
                    task_id
                );
                let _ = self
                    .client
                    .send_task_failed(task_id, "Self-update already running".to_string())
                    .await;
                return;
            }
            *flag = true;
        }

        let running_count = self.running_tasks.read().await.len();
        if running_count > 0 {
            let _ = self
                .client
                .send_task_failed(
                    task_id,
                    format!(
                        "Cannot update while {} task(s) are still running",
                        running_count
                    ),
                )
                .await;
            *self.update_in_progress.write().await = false;
            return;
        }

        if let Err(e) = self.client.send_task_started(task_id).await {
            error!(
                "Failed to report self-update start for task {}: {}",
                task_id, e
            );
        }

        self.client.clear_task_log_ack(task_id).await;
        let log_sync_state = match TaskLogSyncState::new(&self.config.workspaces_path, task_id) {
            Ok(state) => Arc::new(Mutex::new(state)),
            Err(e) => {
                error!(
                    "Failed to initialize self-update log state for {}: {}",
                    task_id, e
                );
                let _ = self
                    .client
                    .send_task_failed(task_id, format!("Failed to initialize task logs: {}", e))
                    .await;
                *self.update_in_progress.write().await = false;
                return;
            }
        };
        {
            let log_path = log_sync_state.lock().await.local_log_path.clone();
            let mut store = self.persisted_state.lock().await;
            store.upsert_running(task_id, "[self_update]".to_string(), log_path);
        }
        self.save_persisted_state().await;

        self.append_manual_task_log_line(
            &log_sync_state,
            "Starting self-update from latest release",
            false,
        )
        .await;

        match self_update::perform_self_update(data.download_url.as_deref()).await {
            Ok(update_result) => {
                self.append_manual_task_log_line(
                    &log_sync_state,
                    &format!(
                        "Binary replaced to version {}. Agent will restart via service manager.",
                        update_result.target_version
                    ),
                    false,
                )
                .await;

                let mut result = HashMap::new();
                result.insert(
                    "target_version".to_string(),
                    serde_json::Value::String(update_result.target_version),
                );

                let fully_synced = self
                    .flush_task_logs_until_synced(
                        &log_sync_state,
                        Duration::from_millis(LOG_FINAL_SYNC_TIMEOUT_MS),
                    )
                    .await;
                if !fully_synced {
                    warn!(
                        "Self-update task {} log sync did not fully complete before completion",
                        task_id
                    );
                }

                {
                    let local_log_path = log_sync_state.lock().await.local_log_path.clone();
                    let mut store = self.persisted_state.lock().await;
                    store.mark_completed(
                        task_id,
                        "[self_update]".to_string(),
                        local_log_path,
                        0,
                        String::new(),
                        String::new(),
                        result.clone(),
                    );
                }
                self.save_persisted_state().await;

                if let Err(e) = self
                    .client
                    .send_task_completed(task_id, 0, String::new(), String::new(), result)
                    .await
                {
                    error!(
                        "Failed to report self-update completion for task {}: {}",
                        task_id, e
                    );
                }

                info!("Binary replaced, exiting for service manager restart...");
                tokio::time::sleep(Duration::from_millis(700)).await;
                // 使用非零退出码触发服务管理器的 on-failure 自动重启
                std::process::exit(42);
            }
            Err(e) => {
                error!("Self-update task {} failed: {}", task_id, e);
                self.append_manual_task_log_line(
                    &log_sync_state,
                    &format!("Self-update failed: {}", e),
                    true,
                )
                .await;
                {
                    let local_log_path = log_sync_state.lock().await.local_log_path.clone();
                    let mut store = self.persisted_state.lock().await;
                    store.mark_failed(
                        task_id,
                        "[self_update]".to_string(),
                        local_log_path,
                        format!("Self-update failed: {}", e),
                    );
                }
                self.save_persisted_state().await;
                let _ = self
                    .client
                    .send_task_failed(task_id, format!("Self-update failed: {}", e))
                    .await;
            }
        }

        self.client.clear_task_log_ack(task_id).await;
        *self.update_in_progress.write().await = false;
    }

    async fn handle_agent_restart(&self, data: AgentRestartData) {
        let task_id = data.task_id;

        let running_count = self.running_tasks.read().await.len();
        if running_count > 0 {
            warn!(
                "Reject restart task {} because {} task(s) are still running",
                task_id, running_count
            );
            let _ = self
                .client
                .send_task_failed(
                    task_id,
                    format!(
                        "Cannot restart while {} task(s) are still running",
                        running_count
                    ),
                )
                .await;
            return;
        }

        if let Err(e) = self.client.send_task_started(task_id).await {
            error!("Failed to report restart start for task {}: {}", task_id, e);
        }

        self.client.clear_task_log_ack(task_id).await;
        let log_sync_state = match TaskLogSyncState::new(&self.config.workspaces_path, task_id) {
            Ok(state) => Arc::new(Mutex::new(state)),
            Err(e) => {
                error!(
                    "Failed to initialize restart log state for {}: {}",
                    task_id, e
                );
                let _ = self
                    .client
                    .send_task_failed(task_id, format!("Failed to initialize task logs: {}", e))
                    .await;
                return;
            }
        };

        self.append_manual_task_log_line(
            &log_sync_state,
            "Agent restart requested, exiting for service manager restart...",
            false,
        )
        .await;

        let fully_synced = self
            .flush_task_logs_until_synced(
                &log_sync_state,
                Duration::from_millis(LOG_FINAL_SYNC_TIMEOUT_MS),
            )
            .await;
        if !fully_synced {
            warn!(
                "Restart task {} log sync did not fully complete before exit",
                task_id
            );
        }

        if let Err(e) = self
            .client
            .send_task_completed(
                task_id,
                0,
                String::new(),
                String::new(),
                HashMap::new(),
            )
            .await
        {
            error!(
                "Failed to report restart completion for task {}: {}",
                task_id, e
            );
        }

        info!("Agent restart requested, exiting for service manager restart...");
        tokio::time::sleep(Duration::from_millis(700)).await;
        // 使用非零退出码触发服务管理器的 on-failure 自动重启
        std::process::exit(42);
    }
}

fn main() {
    let cli = Cli::parse();

    match cli {
        Cli::Run { config, service_name } => {
            // Windows: 尝试以 SCM 服务模式运行
            // 如果进程由 SCM 启动，service_dispatcher::start 会阻塞直到服务停止
            // 如果由用户直接启动，会失败并回退到前台模式
            #[cfg(windows)]
            {
                if try_run_as_windows_service(&service_name) {
                    return;
                }
            }

            // 前台模式运行（用户直接启动或 Linux/macOS 服务管理器启动）
            let _ = service_name; // Linux/macOS 不需要此参数
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            rt.block_on(run_agent(config));
        }
        Cli::Service { action } => {
            service::handle_service_command(action);
        }
    }
}

pub async fn run_agent(config_path: PathBuf) {
    let config = match load_config(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置加载失败: {}", e);
            std::process::exit(1);
        }
    };

    // 配置日志（_log_guard 必须保持存活，否则文件日志停止写入）
    let _log_guard = setup_logging(&config.log_level, config.log_file.as_ref());

    // 验证配置
    if let Err(errors) = config.validate() {
        for error in errors {
            eprintln!("配置错误: {}", error);
        }
        std::process::exit(1);
    }

    // 清理上次更新遗留的 .bak 文件
    self_update::cleanup_previous_update();

    let mut persisted_state = match PersistedStateStore::load(&config.workspaces_path) {
        Ok(store) => store,
        Err(e) => {
            error!("Failed to load persisted agent state: {}", e);
            std::process::exit(1);
        }
    };
    if persisted_state.recover_after_restart() {
        if let Err(e) = persisted_state.save() {
            error!("Failed to save recovered persisted agent state: {}", e);
            std::process::exit(1);
        }
    }

    // 创建并运行 Agent
    let agent = Agent::new(config, persisted_state);

    if let Err(e) = agent.start().await {
        error!("Agent 运行失败: {}", e);
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Windows SCM 服务入口（仅 Windows 编译）
// ---------------------------------------------------------------------------

/// 尝试注册到 Windows SCM 并以服务模式运行。
///
/// 如果由 SCM 启动，`service_dispatcher::start` 阻塞直到服务停止，返回 `true`。
/// 如果由用户直接启动，返回 `false`，调用者应回退到前台模式。
#[cfg(windows)]
fn try_run_as_windows_service(service_name: &str) -> bool {
    use windows_service::service_dispatcher;
    match service_dispatcher::start(service_name, ffi_service_main) {
        Ok(_) => true,
        Err(_) => false,
    }
}

#[cfg(windows)]
windows_service::define_windows_service!(ffi_service_main, windows_service_main);

/// Windows 服务主入口（由 SCM 在后台线程调用）
#[cfg(windows)]
fn windows_service_main(_arguments: Vec<std::ffi::OsString>) {
    use std::sync::mpsc;
    use std::time::Duration;
    use windows_service::{
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
    };

    // 创建 channel：控制处理器通过它发送 Stop 信号
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    // 从进程命令行参数中获取服务名称（多实例支持）
    let service_name = extract_service_name_from_process_args()
        .unwrap_or_else(|| service::DEFAULT_SERVICE_NAME.to_string());

    // 注册服务控制处理器
    let status_handle =
        match service_control_handler::register(&service_name, move |control_event| {
            match control_event {
                ServiceControl::Stop => {
                    let _ = shutdown_tx.send(());
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        }) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("Failed to register service control handler: {}", e);
                return;
            }
        };

    // 上报 Running 状态给 SCM
    if let Err(e) = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }) {
        eprintln!("Failed to set service status to Running: {}", e);
        return;
    }

    // 从进程命令行获取 config 路径
    // SCM 启动命令为: tasknexus-agent.exe run --config <path>
    let config_path = match extract_config_from_process_args() {
        Some(p) => p,
        None => {
            eprintln!(
                "Service: failed to extract --config from process arguments. \
                 Please reinstall the service with: tasknexus-agent service install --config <path>"
            );
            let _ = status_handle.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::Stopped,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::Win32(1),
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            });
            return;
        }
    };

    // 创建 tokio runtime 运行 Agent
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Failed to create tokio runtime: {}", e);
            return;
        }
    };

    rt.block_on(async {
        // 启动 Agent
        let agent_handle = tokio::spawn(run_agent(config_path));
        tokio::pin!(agent_handle);

        // 在后台等待 SCM Stop 信号
        let stop_waiter = tokio::task::spawn_blocking(move || {
            let _ = shutdown_rx.recv();
        });
        tokio::pin!(stop_waiter);

        // 等待 Stop 信号或 Agent 自行退出（以先发生者为准）
        tokio::select! {
            _ = &mut stop_waiter => {
                // 收到 Stop 信号，中止 Agent
                agent_handle.abort();
            }
            _ = &mut agent_handle => {
                // Agent 自行退出（例如自更新场景的 exit(42)）
            }
        }
    });

    // 上报 Stopped 状态
    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });
}

/// 从当前进程命令行参数中提取 --config 值
#[cfg(windows)]
fn extract_config_from_process_args() -> Option<PathBuf> {
    extract_arg_value("--config", Some("-c"))
        .map(PathBuf::from)
}

/// 从当前进程命令行参数中提取 --service-name 值
#[cfg(windows)]
fn extract_service_name_from_process_args() -> Option<String> {
    extract_arg_value("--service-name", None)
}

/// 通用参数提取器：支持 `--key value` 和 `--key=value` 格式
#[cfg(windows)]
fn extract_arg_value(long_name: &str, short_name: Option<&str>) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == long_name || short_name.map_or(false, |s| arg == s) {
            if let Some(value) = iter.next() {
                return Some(value.clone());
            }
        }
        let prefix = format!("{}=", long_name);
        if let Some(value) = arg.strip_prefix(&prefix) {
            return Some(value.to_string());
        }
    }
    None
}
