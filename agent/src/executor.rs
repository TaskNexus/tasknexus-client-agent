//! 命令执行模块
//!
//! 在本地环境中执行服务器分发的命令。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, warn};

/// 命令执行结果
#[derive(Debug)]
pub struct ExecutionResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub cancelled: bool,
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
        cancel_rx: Option<watch::Receiver<bool>>,
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

        // 从环境变量读取 SHELL，未设置则按平台使用默认值
        let shell_from_env = environment
            .and_then(|env| env.get("SHELL").cloned());

        let default_shell = {
            #[cfg(target_os = "macos")]
            { "/bin/zsh" }
            #[cfg(target_os = "linux")]
            { "/bin/bash" }
            #[cfg(windows)]
            { "cmd" }
        };

        let shell_path = shell_from_env
            .as_deref()
            .unwrap_or(default_shell);

        // 提取 shell 基本名称用于参数映射
        let shell_name = shell_path
            .rsplit('/')
            .next()
            .unwrap_or(shell_path)
            .rsplit('\\')
            .next()
            .unwrap_or(shell_path);

        info!("Using shell: {} ({})", shell_path, shell_name);

        // 根据 shell 名称选择参数
        let shell_args: Vec<&str> = match shell_name {
            "zsh" | "bash" => vec!["-l", "-i", "-c"],
            "sh" => vec!["-c"],
            "cmd" | "cmd.exe" => vec!["/C"],
            "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => vec!["-Command"],
            _ => vec!["-c"],
        };

        let mut cmd = Command::new(shell_path);
        cmd.args(&shell_args).arg(command);

        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }

        // 设置环境变量（过滤掉 SHELL，仅供内部使用）
        if let Some(env) = environment {
            for (key, value) in env {
                if key != "SHELL" {
                    cmd.env(key, value);
                }
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
                    cancelled: false,
                };
            }
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let mut stdout_reader = BufReader::new(stdout);
        let mut stderr_reader = BufReader::new(stderr);

        let stdout_chunks: Vec<String> = Vec::new();
        let _stderr_chunks: Vec<String> = Vec::new();

        // 创建输出通道
        let (tx, mut rx) = mpsc::channel::<(String, bool)>(100);
        let tx_stdout = tx.clone();
        let tx_stderr = tx;

        // 读取 stdout
        let stdout_handle = tokio::spawn(async move {
            let mut lines = Vec::new();
            let mut buffer = Vec::new();
            while let Ok(n) = stdout_reader.read_until(b'\n', &mut buffer).await {
                if n == 0 {
                    break;
                }
                let line_str = String::from_utf8_lossy(&buffer).trim_end().to_string();
                let params = (line_str.clone(), false);
                let _ = tx_stdout.send(params).await;
                lines.push(line_str);
                buffer.clear();
            }
            lines
        });

        // 读取 stderr
        let stderr_handle = tokio::spawn(async move {
            let mut lines = Vec::new();
            let mut buffer = Vec::new();
            while let Ok(n) = stderr_reader.read_until(b'\n', &mut buffer).await {
                if n == 0 {
                    break;
                }
                let line_str = String::from_utf8_lossy(&buffer).trim_end().to_string();
                let params = (line_str.clone(), true);
                let _ = tx_stderr.send(params).await;
                lines.push(line_str);
                buffer.clear();
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

        // Wait for completion with timeout and cancellation
        let timed_future = timeout(Duration::from_secs(timeout_secs), async {
            let stdout_lines = stdout_handle.await.unwrap_or_default();
            let stderr_lines = stderr_handle.await.unwrap_or_default();
            let status = child.wait().await;
            (stdout_lines, stderr_lines, status)
        });

        // If we have a cancel receiver, `select!` between timeout and cancellation
        if let Some(mut cancel_rx) = cancel_rx {
            tokio::select! {
                result = timed_future => {
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
                                cancelled: false,
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
                                cancelled: false,
                            }
                        }
                    }
                }
                _ = cancel_rx.changed() => {
                    warn!("Command cancelled");
                    let _ = child.kill().await;
                    drop(callback_handle);
                    ExecutionResult {
                        exit_code: -1,
                        stdout: stdout_chunks.join("\n"),
                        stderr: "Task was cancelled".to_string(),
                        timed_out: false,
                        cancelled: true,
                    }
                }
            }
        } else {
            // No cancellation - original behavior
            let result = timed_future.await;
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
                        cancelled: false,
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
                        cancelled: false,
                    }
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
        client_repo_token: Option<&str>,
        timeout_secs: u64,
        on_output: Option<F>,
        environment: Option<HashMap<String, String>>,
        cancel_rx: Option<watch::Receiver<bool>>,
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
                cancelled: false,
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
                    let result = self.clone_repo(repo_url, &repo_path, client_repo_ref, client_repo_token, on_output.clone()).await;
                    if result.exit_code != 0 {
                        return result;
                    }
                } else {
                    // 尝试 pull 最新代码
                    info!("Updating repository: {:?}", repo_path);
                    let update_result = self.update_repo(repo_url, &repo_path, client_repo_ref, client_repo_token, on_output.clone()).await;
                    if update_result.exit_code != 0 {
                        warn!("Failed to update repository (will continue anyway)");
                    }
                }

                exec_dir = repo_path;
            }
        }

        // If no repo_url was provided, auto-detect existing git repo in workspace
        if exec_dir == workspace_dir {
            if let Ok(entries) = std::fs::read_dir(&workspace_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() && path.join(".git").exists() {
                        info!("Auto-detected repo directory: {:?}", path);
                        exec_dir = path;
                        break;
                    }
                }
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
                cancel_rx,
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

    /// Inject token into an HTTPS URL for authenticated git operations
    fn inject_token_into_url(repo_url: &str, token: Option<&str>) -> String {
        match token {
            Some(t) if !t.is_empty() => {
                if let Some(rest) = repo_url.strip_prefix("https://") {
                    format!("https://oauth2:{}@{}", t, rest)
                } else if let Some(rest) = repo_url.strip_prefix("http://") {
                    format!("http://oauth2:{}@{}", t, rest)
                } else {
                    // For non-HTTP URLs (e.g. SSH), return as-is
                    repo_url.to_string()
                }
            }
            _ => repo_url.to_string(),
        }
    }

    /// Clone a git repository
    async fn clone_repo<F, Fut>(
        &self,
        repo_url: &str,
        target_path: &Path,
        ref_name: &str,
        token: Option<&str>,
        on_output: Option<F>,
    ) -> ExecutionResult
    where
        F: Fn(String, bool) -> Fut + Send + Clone + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let repo_name = target_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");

        let auth_url = Self::inject_token_into_url(repo_url, token);

        let clone_cmd = format!(
            "git clone --depth 1 --branch {} {} {}",
            ref_name, auth_url, repo_name
        );

        info!("Cloning: {} (in {:?})", repo_url, target_path.parent());

        let mut env = HashMap::new();
        env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());

        self.executor
            .execute(
                &clone_cmd,
                target_path.parent(),
                Some(&env),
                Some(300), // 5 minutes for clone
                on_output,
                None,
            )
            .await
    }

    /// Update a git repository
    async fn update_repo<F, Fut>(
        &self,
        repo_url: &str,
        repo_path: &Path,
        ref_name: &str,
        token: Option<&str>,
        on_output: Option<F>,
    ) -> ExecutionResult
    where
        F: Fn(String, bool) -> Fut + Send + Clone + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        // Use the same token injection as clone for authentication
        let auth_url = Self::inject_token_into_url(repo_url, token);

        let update_cmd = format!(
            "git fetch {} {} && git reset --hard FETCH_HEAD",
            auth_url, ref_name
        );

        let mut env = HashMap::new();
        env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());

        self.executor
            .execute(
                &update_cmd,
                Some(repo_path),
                Some(&env),
                Some(120), // 2 minutes for update
                on_output,
                None,
            )
            .await
    }
}
