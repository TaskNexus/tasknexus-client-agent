//! TaskNexus Agent 主入口
//!
//! 提供命令行接口和 Agent 运行逻辑。

use chrono::Local;
use clap::Parser;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{watch, Mutex, RwLock};
use tokio::time::{interval, Duration, Instant};
use tracing::{error, info, warn, Level};
use tracing_subscriber::{fmt, EnvFilter};

use tasknexus_agent::{
    autostart::AutoStartManager,
    client::{AgentClient, AgentUpdateData, TaskDispatchData},
    config::{load_config, AgentConfig},
    executor::TaskRunner,
    self_update,
};

const MAX_LOG_CHUNK_BYTES: usize = 64 * 1024;
const LOG_APPEND_INTERVAL_MS: u64 = 500;
const LOG_ACTIVE_DEBOUNCE_MS: u64 = 125;
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

struct TaskLogSyncState {
    task_id: i64,
    local_log_path: PathBuf,
    committed_offset: u64,
    synced_offset: u64,
    line_timestamp: Option<String>,
    line_buffer: String,
    line_is_stderr: bool,
    active_display: Option<VisibleLine>,
    pending_active_event: Option<PendingActiveEvent>,
    next_active_seq: u64,
    last_append_flush: Instant,
    last_active_change: Option<Instant>,
}

impl TaskLogSyncState {
    fn new(workspaces_path: &PathBuf, task_id: i64) -> std::io::Result<Self> {
        let log_dir = workspaces_path.join(TASK_LOG_DIR_NAME);
        fs::create_dir_all(&log_dir)?;
        let local_log_path = log_dir.join(format!("task_{}.log", task_id));
        File::create(&local_log_path)?;

        Ok(Self {
            task_id,
            local_log_path,
            committed_offset: 0,
            synced_offset: 0,
            line_timestamp: None,
            line_buffer: String::new(),
            line_is_stderr: false,
            active_display: None,
            pending_active_event: None,
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
        let backlog = self.committed_offset.saturating_sub(self.synced_offset);
        let should_flush_append = backlog > 0
            && (force_append
                || backlog >= MAX_LOG_CHUNK_BYTES as u64
                || self.last_append_flush.elapsed()
                    >= Duration::from_millis(LOG_APPEND_INTERVAL_MS));

        if should_flush_append {
            loop {
                if self.synced_offset >= self.committed_offset {
                    break;
                }

                let (content, byte_len) = read_utf8_chunk(
                    &self.local_log_path,
                    self.synced_offset,
                    MAX_LOG_CHUNK_BYTES,
                )?;
                if byte_len == 0 || content.is_empty() {
                    break;
                }

                client
                    .send_task_log_append(self.task_id, self.synced_offset, content)
                    .await?;
                self.synced_offset += byte_len as u64;
                self.last_append_flush = Instant::now();

                if !force_append && self.synced_offset < self.committed_offset {
                    continue;
                }
            }
        }

        if self.synced_offset != self.committed_offset {
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
                        self.synced_offset,
                        visible.text.clone(),
                        visible.is_stderr,
                    )
                    .await?;
            }
            PendingActiveEventKind::Clear => {
                client
                    .send_task_log_active_clear(self.task_id, pending.seq, self.synced_offset)
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
        let mut file = OpenOptions::new().append(true).open(&self.local_log_path)?;
        file.write_all(bytes)?;
        file.flush()?;
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
struct Cli {
    /// 配置文件路径 (必须)
    #[arg(short, long)]
    config: PathBuf,
}

/// 配置日志
fn setup_logging(log_level: &str, log_file: Option<&PathBuf>) {
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
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
        subscriber.with_writer(non_blocking).init();
    } else {
        subscriber.init();
    }
}

/// Agent 主结构
struct Agent {
    config: AgentConfig,
    config_path: PathBuf,
    task_runner: TaskRunner,
    client: AgentClient,
    /// Maps task_id -> (workspace_name, cancel_sender)
    running_tasks: Arc<RwLock<HashMap<i64, (String, watch::Sender<bool>)>>>,
    update_in_progress: Arc<RwLock<bool>>,
}

impl Agent {
    fn new(config: AgentConfig, config_path: PathBuf) -> Self {
        let task_runner = TaskRunner::new(config.workspaces_path.clone(), config.proxy_env());
        let client = AgentClient::new(config.clone());

        Self {
            config,
            config_path,
            task_runner,
            client,
            running_tasks: Arc::new(RwLock::new(HashMap::new())),
            update_in_progress: Arc::new(RwLock::new(false)),
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
                || info!("Connected to TaskNexus server"),
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
            .insert(task_id, (workspace_name.clone(), cancel_tx));

        // 通知任务开始
        if let Err(e) = self.client.send_task_started(task_id).await {
            error!("Failed to send task started: {}", e);
        }

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
            let mut tick = interval(Duration::from_millis(LOG_ACTIVE_DEBOUNCE_MS));
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
            let sync_ok = if let Err(e) = guard.sync_with_server(&self.client, true, true).await {
                warn!("Failed to flush task logs for {}: {}", task_id, e);
                false
            } else {
                true
            };
            (finalize_ok && sync_ok, guard.local_log_path.clone())
        };

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
                    result.stdout,
                    result.stderr,
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
                        result.stdout,
                        result.stderr,
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

        if local_log_flush_succeeded {
            if let Err(e) = fs::remove_file(&local_log_path) {
                warn!(
                    "Failed to cleanup local task log for {} at {:?}: {}",
                    task_id, local_log_path, e
                );
            } else {
                info!(
                    "Cleaned up local task log for {} at {:?}",
                    task_id, local_log_path
                );
            }
        } else {
            warn!(
                "Keeping local task log for {} at {:?} because final log flush did not complete successfully",
                task_id, local_log_path
            );
        }

        // 移除运行中的任务
        self.running_tasks.write().await.remove(&task_id);
    }

    async fn handle_task_cancel(&self, task_id: i64) {
        info!("Processing cancel for task {}", task_id);
        let running = self.running_tasks.read().await;
        if let Some((workspace_name, cancel_tx)) = running.get(&task_id) {
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

        let _ = self
            .client
            .send_task_progress(
                task_id,
                "Starting self-update from latest release".to_string(),
                false,
            )
            .await;

        match self_update::perform_self_update(&self.config_path).await {
            Ok(update_result) => {
                let _ = self
                    .client
                    .send_task_progress(
                        task_id,
                        format!(
                            "Updater launched successfully for version {}. Agent will restart now.",
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

                tokio::time::sleep(Duration::from_millis(700)).await;
                std::process::exit(0);
            }
            Err(e) => {
                error!("Self-update task {} failed: {}", task_id, e);
                let _ = self
                    .client
                    .send_task_failed(task_id, format!("Self-update failed: {}", e))
                    .await;
            }
        }

        *self.update_in_progress.write().await = false;
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // 加载配置
    let config_path = cli.config.clone();
    let config = match load_config(cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置加载失败: {}", e);
            std::process::exit(1);
        }
    };

    // 配置日志
    setup_logging(&config.log_level, config.log_file.as_ref());

    // 验证配置
    if let Err(errors) = config.validate() {
        for error in errors {
            eprintln!("配置错误: {}", error);
        }
        std::process::exit(1);
    }

    // 根据配置自动应用开机自启动设置
    apply_autostart_from_config(&config, &config_path);

    // 创建并运行 Agent
    let agent = Agent::new(config, config_path);

    if let Err(e) = agent.start().await {
        error!("Agent 运行失败: {}", e);
        std::process::exit(1);
    }
}

/// 根据配置自动应用开机自启动设置
fn apply_autostart_from_config(config: &AgentConfig, config_path: &PathBuf) {
    let extra_args = if config.autostart.args.is_empty() {
        None
    } else {
        Some(config.autostart.args.as_slice())
    };

    let manager = match AutoStartManager::new("tasknexus-agent", Some(config_path), extra_args) {
        Ok(m) => m,
        Err(e) => {
            warn!("创建自启动管理器失败: {}", e);
            return;
        }
    };

    // 检查当前状态是否与配置一致
    let current_enabled = manager.is_enabled().unwrap_or(false);

    if config.autostart.enabled && !current_enabled {
        // 配置要求启用，但当前未启用
        match manager.enable() {
            Ok(_) => info!("根据配置启用了开机自启动"),
            Err(e) => warn!("启用开机自启动失败: {}", e),
        }
    } else if !config.autostart.enabled && current_enabled {
        // 配置要求禁用，但当前已启用
        match manager.disable() {
            Ok(_) => info!("根据配置禁用了开机自启动"),
            Err(e) => warn!("禁用开机自启动失败: {}", e),
        }
    }
}
