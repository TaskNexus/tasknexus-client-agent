//! TaskNexus Agent 主入口
//!
//! 提供命令行接口和 Agent 运行逻辑。

use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{watch, RwLock};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn, Level};
use tracing_subscriber::{fmt, EnvFilter};

use tasknexus_agent::{
    config::{load_config, AgentConfig},
    client::{AgentClient, TaskDispatchData},
    executor::TaskRunner,
    autostart::AutoStartManager,
};

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
            log_path.file_name().unwrap_or(std::ffi::OsStr::new("agent.log")),
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
    task_runner: TaskRunner,
    client: AgentClient,
    /// Maps workspace_name -> (task_id, cancel_sender)
    running_tasks: Arc<RwLock<HashMap<String, (i64, watch::Sender<bool>)>>>,
}

impl Agent {
    fn new(config: AgentConfig) -> Self {
        let task_runner = TaskRunner::new(config.workspaces_path.clone());
        let client = AgentClient::new(config.clone());

        Self {
            config,
            task_runner,
            client,
            running_tasks: Arc::new(RwLock::new(HashMap::new())),
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

        // 运行客户端
        agent.client.run(
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
            || info!("Connected to TaskNexus server"),
            || warn!("Disconnected from TaskNexus server"),
        ).await?;

        Ok(())
    }

    async fn handle_task_dispatch(&self, data: TaskDispatchData) {
        let task_id = data.task_id;
        let workspace_name = data.workspace_name.clone();
        let command = data.command.clone();

        info!(
            "Received task {} for workspace '{}': {}",
            task_id, workspace_name, command
        );

        // 检查该 workspace 是否已有运行中的任务
        {
            let running = self.running_tasks.read().await;
            if let Some((existing_task_id, _)) = running.get(&workspace_name) {
                warn!(
                    "Workspace '{}' already running task {}, rejecting new task",
                    workspace_name, existing_task_id
                );
                let _ = self.client.send_task_failed(
                    task_id,
                    format!(
                        "Workspace '{}' is busy running task {}",
                        workspace_name, existing_task_id
                    ),
                ).await;
                return;
            }
        }

        // 创建取消信号通道
        let (cancel_tx, cancel_rx) = watch::channel(false);

        // 标记任务正在运行
        self.running_tasks.write().await.insert(workspace_name.clone(), (task_id, cancel_tx));

        // 通知任务开始
        if let Err(e) = self.client.send_task_started(task_id).await {
            error!("Failed to send task started: {}", e);
        }

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

        // 创建输出回调
        let client = self.client.clone();
        let output_callback = move |line: String, _is_stderr: bool| {
            let client = client.clone();
            async move {
                let _ = client.send_task_progress(task_id, line).await;
            }
        };

        // 执行任务，传入取消信号
        let result = self.task_runner.run_task(
            task_id,
            &data.command,
            &data.workspace_name,
            data.client_repo_url.as_deref(),
            &data.client_repo_ref,
            data.client_repo_token.as_deref(),
            data.timeout,
            Some(output_callback),
            if data.environment.is_empty() { None } else { Some(data.environment) },
            Some(cancel_rx),
        ).await;

        // 停止心跳发送器
        heartbeat_task.abort();

        // 发送结果
        if result.cancelled {
            info!("Task {} was cancelled", task_id);
            // Task already marked as cancelled on the server side
        } else if result.exit_code == 0 {
            if let Err(e) = self.client.send_task_completed(
                task_id,
                result.exit_code,
                result.stdout,
                result.stderr,
            ).await {
                error!("Failed to send task completed: {}", e);
            }
            info!("Task {} completed successfully", task_id);
        } else {
            if result.timed_out {
                if let Err(e) = self.client.send_task_failed(
                    task_id,
                    format!("Task timed out after {} seconds", data.timeout),
                ).await {
                    error!("Failed to send task failed: {}", e);
                }
            } else {
                if let Err(e) = self.client.send_task_completed(
                    task_id,
                    result.exit_code,
                    result.stdout,
                    result.stderr,
                ).await {
                    error!("Failed to send task completed: {}", e);
                }
            }
            warn!("Task {} failed with exit code {}", task_id, result.exit_code);
        }

        // 移除运行中的任务
        self.running_tasks.write().await.remove(&workspace_name);
    }

    async fn handle_task_cancel(&self, task_id: i64) {
        info!("Processing cancel for task {}", task_id);
        let running = self.running_tasks.read().await;
        for (workspace_name, (tid, cancel_tx)) in running.iter() {
            if *tid == task_id {
                info!("Sending cancel signal to task {} in workspace '{}'", task_id, workspace_name);
                let _ = cancel_tx.send(true);
                return;
            }
        }
        warn!("Task {} not found in running tasks, cannot cancel", task_id);
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
    let agent = Agent::new(config);

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
