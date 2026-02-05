//! 命令执行模块
//!
//! 在本地环境中执行服务器分发的命令。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, warn};

/// 命令执行结果
#[derive(Debug)]
pub struct ExecutionResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// 命令执行器
pub struct CommandExecutor {
    default_timeout: u64,
}

impl CommandExecutor {
    pub fn new(default_timeout: u64) -> Self {
        Self { default_timeout }
    }

    /// 异步执行命令
    pub async fn execute<F, Fut>(
        &self,
        command: &str,
        working_dir: Option<&Path>,
        environment: Option<&HashMap<String, String>>,
        timeout_secs: Option<u64>,
        on_output: Option<F>,
    ) -> ExecutionResult
    where
        F: Fn(String, bool) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let timeout_secs = timeout_secs.unwrap_or(self.default_timeout);

        info!("Executing command: {}", command);
        if let Some(dir) = working_dir {
            info!("Working directory: {:?}", dir);
        }

        // 根据平台选择 shell
        #[cfg(windows)]
        let (shell, shell_arg) = ("cmd", "/C");
        #[cfg(not(windows))]
        let (shell, shell_arg) = ("sh", "-c");

        let mut cmd = Command::new(shell);
        cmd.arg(shell_arg).arg(command);

        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }

        // 设置环境变量
        if let Some(env) = environment {
            for (key, value) in env {
                cmd.env(key, value);
            }
        }

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // 在 Windows 上使用 CREATE_NO_WINDOW 标志
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                error!("Failed to spawn command: {}", e);
                return ExecutionResult {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: e.to_string(),
                    timed_out: false,
                };
            }
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        let stdout_chunks: Vec<String> = Vec::new();
        let _stderr_chunks: Vec<String> = Vec::new();

        // 创建输出通道
        let (tx, mut rx) = mpsc::channel::<(String, bool)>(100);
        let tx_stdout = tx.clone();
        let tx_stderr = tx;

        // 读取 stdout
        let stdout_handle = tokio::spawn(async move {
            let mut lines = Vec::new();
            while let Ok(Some(line)) = stdout_reader.next_line().await {
                let _ = tx_stdout.send((line.clone(), false)).await;
                lines.push(line);
            }
            lines
        });

        // 读取 stderr
        let stderr_handle = tokio::spawn(async move {
            let mut lines = Vec::new();
            while let Ok(Some(line)) = stderr_reader.next_line().await {
                let _ = tx_stderr.send((line.clone(), true)).await;
                lines.push(line);
            }
            lines
        });

        // 处理输出回调
        let callback_handle = tokio::spawn(async move {
            if let Some(callback) = on_output {
                while let Some((line, is_stderr)) = rx.recv().await {
                    callback(line, is_stderr).await;
                }
            } else {
                // 如果没有回调，也要消费通道
                while rx.recv().await.is_some() {}
            }
        });

        // 等待命令完成，带超时
        let result = timeout(Duration::from_secs(timeout_secs), async {
            let stdout_lines = stdout_handle.await.unwrap_or_default();
            let stderr_lines = stderr_handle.await.unwrap_or_default();
            let status = child.wait().await;
            (stdout_lines, stderr_lines, status)
        })
        .await;

        // 确保回调处理完成
        drop(callback_handle);

        match result {
            Ok((stdout_lines, stderr_lines, status)) => {
                let exit_code = status
                    .map(|s| s.code().unwrap_or(-1))
                    .unwrap_or(-1);

                ExecutionResult {
                    exit_code,
                    stdout: stdout_lines.join("\n"),
                    stderr: stderr_lines.join("\n"),
                    timed_out: false,
                }
            }
            Err(_) => {
                warn!("Command timed out after {} seconds", timeout_secs);
                let _ = child.kill().await;

                ExecutionResult {
                    exit_code: -1,
                    stdout: stdout_chunks.join("\n"),
                    stderr: format!("Command timed out after {} seconds", timeout_secs),
                    timed_out: true,
                }
            }
        }
    }
}

/// 任务运行器
pub struct TaskRunner {
    workspaces_path: PathBuf,
    executor: CommandExecutor,
}

impl TaskRunner {
    pub fn new(workspaces_path: PathBuf) -> Self {
        // 确保工作目录存在
        let _ = std::fs::create_dir_all(&workspaces_path);

        Self {
            workspaces_path,
            executor: CommandExecutor::new(3600),
        }
    }

    /// 运行任务
    pub async fn run_task<F, Fut>(
        &self,
        task_id: i64,
        command: &str,
        workspace_name: &str,
        client_repo_url: Option<&str>,
        client_repo_ref: &str,
        timeout_secs: u64,
        on_output: Option<F>,
        environment: Option<HashMap<String, String>>,
    ) -> ExecutionResult
    where
        F: Fn(String, bool) -> Fut + Send + Clone + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        info!(
            "Running task {} in workspace '{}': {}",
            task_id, workspace_name, command
        );
        debug!(
            "client_repo_url={:?}, client_repo_ref={}",
            client_repo_url, client_repo_ref
        );

        // 获取/创建工作空间目录
        let workspace_dir = self.workspaces_path.join(workspace_name);
        if let Err(e) = std::fs::create_dir_all(&workspace_dir) {
            error!("Failed to create workspace directory: {}", e);
            return ExecutionResult {
                exit_code: -1,
                stdout: String::new(),
                stderr: format!("Failed to create workspace directory: {}", e),
                timed_out: false,
            };
        }

        // 确定执行目录
        let mut exec_dir = workspace_dir.clone();

        if let Some(repo_url) = client_repo_url {
            if !repo_url.is_empty() {
                // 从 URL 提取仓库名称
                let repo_name = repo_url
                    .trim_end_matches('/')
                    .split('/')
                    .last()
                    .unwrap_or("repo")
                    .trim_end_matches(".git");

                let repo_path = workspace_dir.join(repo_name);

                if !repo_path.exists() {
                    // 自动 clone 仓库
                    info!("Cloning repository {} to {:?}", repo_url, repo_path);
                    let result = self.clone_repo(repo_url, &repo_path, client_repo_ref).await;
                    if result.exit_code != 0 {
                        return result;
                    }
                } else {
                    // 尝试 pull 最新代码
                    info!("Updating repository: {:?}", repo_path);
                    let update_result = self.update_repo(&repo_path, client_repo_ref).await;
                    if update_result.exit_code != 0 {
                        warn!("Failed to update repository (will continue anyway)");
                    }
                }

                exec_dir = repo_path;
            }
        }

        // 设置环境变量
        let mut task_env = HashMap::new();
        task_env.insert("TASKNEXUS_TASK_ID".to_string(), task_id.to_string());
        task_env.insert("TASKNEXUS_WORKSPACE".to_string(), workspace_name.to_string());

        // 合并用户自定义环境变量
        if let Some(env) = environment {
            task_env.extend(env);
        }

        // 执行命令
        let result = self
            .executor
            .execute(
                command,
                Some(&exec_dir),
                Some(&task_env),
                Some(timeout_secs),
                on_output,
            )
            .await;

        if result.exit_code != 0 {
            error!("Command failed with exit code {}", result.exit_code);
            if !result.stderr.is_empty() {
                error!("stderr: {}", &result.stderr[..result.stderr.len().min(500)]);
            }
        }

        result
    }

    /// Clone a git repository
    async fn clone_repo(
        &self,
        repo_url: &str,
        target_path: &Path,
        ref_name: &str,
    ) -> ExecutionResult {
        let repo_name = target_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");

        let clone_cmd = format!(
            "git clone --depth 1 --branch {} {} {}",
            ref_name, repo_url, repo_name
        );

        info!("Cloning: {} (in {:?})", clone_cmd, target_path.parent());

        self.executor
            .execute::<fn(String, bool) -> std::future::Ready<()>, _>(
                &clone_cmd,
                target_path.parent(),
                None,
                Some(300), // 5 minutes for clone
                None,
            )
            .await
    }

    /// Update a git repository
    async fn update_repo(&self, repo_path: &Path, ref_name: &str) -> ExecutionResult {
        let update_cmd = format!(
            "git fetch origin {} && git reset --hard origin/{}",
            ref_name, ref_name
        );

        self.executor
            .execute::<fn(String, bool) -> std::future::Ready<()>, _>(
                &update_cmd,
                Some(repo_path),
                None,
                Some(120), // 2 minutes for update
                None,
            )
            .await
    }
}
