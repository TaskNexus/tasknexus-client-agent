//! 命令执行模块
//!
//! 在本地环境中执行服务器分发的命令。

use crate::client::InlineCode;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, warn};

/// Magic markers for structured result extraction from stdout
const RESULT_BEGIN_MARKER: &str = "##TASKNEXUS_RESULT_BEGIN##";
const RESULT_END_MARKER: &str = "##TASKNEXUS_RESULT_END##";
const MAX_CAPTURED_OUTPUT_CHARS: usize = 16 * 1024;

fn default_shell_path() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "/bin/zsh"
    }
    #[cfg(target_os = "linux")]
    {
        "/bin/bash"
    }
    #[cfg(windows)]
    {
        "cmd"
    }
}

fn shell_name_from_path(shell_path: &str) -> &str {
    let trimmed = shell_path.trim();
    trimmed
        .rsplit('/')
        .next()
        .unwrap_or(trimmed)
        .rsplit('\\')
        .next()
        .unwrap_or(trimmed)
}

fn resolve_shell_path(environment: Option<&HashMap<String, String>>) -> String {
    environment
        .and_then(|env| env.get("SHELL"))
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or(default_shell_path())
        .to_string()
}

#[cfg(windows)]
fn is_cmd_shell(shell_name: &str) -> bool {
    matches!(shell_name, "cmd" | "cmd.exe")
}

/// 杀死进程及其所有子进程
async fn kill_process_tree(child: &mut Child) {
    let pid = match child.id() {
        Some(pid) => pid,
        None => {
            warn!("Process already exited, no pid to kill");
            return;
        }
    };

    #[cfg(unix)]
    {
        // 使用 killpg 杀死整个进程组
        let pgid = pid as libc::pid_t;
        info!("Killing process group {}", pgid);
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }

    #[cfg(windows)]
    {
        // 使用 taskkill /F /T 递归杀死进程树
        info!("Killing process tree for PID {}", pid);
        let _ = std::process::Command::new("taskkill")
            .args(&["/F", "/T", "/PID", &pid.to_string()])
            .output();
    }

    // 等待子进程退出，防止僵尸进程
    let _ = child.wait().await;
}

/// 命令执行结果
#[derive(Debug)]
pub struct ExecutionResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub cancelled: bool,
    /// Structured result extracted from stdout via magic markers.
    pub result: HashMap<String, serde_json::Value>,
}

#[derive(Default)]
struct OutputTail {
    text: String,
    truncated: bool,
}

impl OutputTail {
    fn append(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }

        self.text.push_str(chunk);
        let total_chars = self.text.chars().count();
        if total_chars <= MAX_CAPTURED_OUTPUT_CHARS {
            return;
        }

        let overflow = total_chars - MAX_CAPTURED_OUTPUT_CHARS;
        self.text = self.text.chars().skip(overflow).collect();
        self.truncated = true;
    }

    fn finish(self) -> String {
        if !self.truncated {
            return self.text;
        }

        format!(
            "[output truncated, showing last {} chars]\n{}",
            MAX_CAPTURED_OUTPUT_CHARS, self.text
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StdoutCaptureMode {
    Visible,
    StructuredResult,
}

struct StdoutCapture {
    visible_output: OutputTail,
    hidden_buffer: String,
    mode: StdoutCaptureMode,
    result: HashMap<String, serde_json::Value>,
    last_visible_char: Option<char>,
    suppress_next_leading_newline: bool,
}

impl StdoutCapture {
    fn new() -> Self {
        Self {
            visible_output: OutputTail::default(),
            hidden_buffer: String::new(),
            mode: StdoutCaptureMode::Visible,
            result: HashMap::new(),
            last_visible_char: None,
            suppress_next_leading_newline: false,
        }
    }

    fn process_chunk(&mut self, chunk: &str) -> String {
        let mut remaining = self.maybe_trim_leading_newline(chunk);
        let mut emitted = String::new();

        while !remaining.is_empty() {
            match self.mode {
                StdoutCaptureMode::Visible => {
                    if let Some(idx) = remaining.find(RESULT_BEGIN_MARKER) {
                        let before = &remaining[..idx];
                        self.push_visible(&mut emitted, before);
                        self.mode = StdoutCaptureMode::StructuredResult;
                        self.hidden_buffer.clear();
                        self.hidden_buffer.push_str(RESULT_BEGIN_MARKER);
                        remaining = &remaining[idx + RESULT_BEGIN_MARKER.len()..];
                    } else {
                        self.push_visible(&mut emitted, remaining);
                        break;
                    }
                }
                StdoutCaptureMode::StructuredResult => {
                    if let Some(idx) = remaining.find(RESULT_END_MARKER) {
                        self.hidden_buffer.push_str(&remaining[..idx]);
                        self.capture_result_from_hidden_buffer();
                        self.hidden_buffer.clear();
                        self.mode = StdoutCaptureMode::Visible;
                        self.suppress_next_leading_newline =
                            matches!(self.last_visible_char, Some('\n' | '\r'));
                        remaining = self.maybe_trim_leading_newline(
                            &remaining[idx + RESULT_END_MARKER.len()..],
                        );
                    } else {
                        self.hidden_buffer.push_str(remaining);
                        break;
                    }
                }
            }
        }

        emitted
    }

    fn finish(mut self) -> (String, HashMap<String, serde_json::Value>) {
        if self.mode == StdoutCaptureMode::StructuredResult && !self.hidden_buffer.is_empty() {
            let hidden = self.hidden_buffer.clone();
            self.push_visible(&mut String::new(), &hidden);
            self.hidden_buffer.clear();
            self.mode = StdoutCaptureMode::Visible;
        }

        (self.visible_output.finish(), self.result)
    }

    fn push_visible(&mut self, emitted: &mut String, chunk: &str) {
        if chunk.is_empty() {
            return;
        }

        emitted.push_str(chunk);
        self.visible_output.append(chunk);
        self.last_visible_char = chunk.chars().last();
    }

    fn capture_result_from_hidden_buffer(&mut self) {
        if !self.result.is_empty() {
            return;
        }

        let json_str = self
            .hidden_buffer
            .strip_prefix(RESULT_BEGIN_MARKER)
            .unwrap_or(self.hidden_buffer.as_str())
            .trim();
        self.result = match serde_json::from_str(json_str) {
            Ok(map) => map,
            Err(err) => {
                warn!("Failed to parse result JSON: {}", err);
                HashMap::new()
            }
        };
    }

    fn maybe_trim_leading_newline<'a>(&mut self, text: &'a str) -> &'a str {
        if !self.suppress_next_leading_newline {
            return text;
        }

        if let Some(rest) = text.strip_prefix("\r\n") {
            self.suppress_next_leading_newline = false;
            return rest;
        }
        if let Some(rest) = text.strip_prefix('\n') {
            self.suppress_next_leading_newline = false;
            return rest;
        }
        if let Some(rest) = text.strip_prefix('\r') {
            self.suppress_next_leading_newline = false;
            return rest;
        }
        if !text.is_empty() {
            self.suppress_next_leading_newline = false;
        }
        text
    }
}

fn split_stream_chunks(pending: &mut Vec<u8>, incoming: &[u8]) -> Vec<String> {
    let mut result = Vec::new();

    for byte in incoming {
        pending.push(*byte);
        if *byte == b'\n' || *byte == b'\r' {
            result.push(String::from_utf8_lossy(pending).to_string());
            pending.clear();
        }
    }

    result
}

fn flush_stream_buffer(pending: &mut Vec<u8>) -> Option<String> {
    if pending.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(pending).to_string();
    pending.clear();
    Some(text)
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

        let shell_path = resolve_shell_path(environment);
        let shell_name = shell_name_from_path(&shell_path).to_string();

        info!("Using shell: {} ({})", shell_path, shell_name);

        // 根据 shell 名称选择参数
        let shell_args: Vec<&str> = match shell_name.as_str() {
            "zsh" | "bash" => vec!["-l", "-c"],
            "sh" => vec!["-c"],
            "cmd" | "cmd.exe" => vec!["/S", "/C"],
            "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => vec!["-Command"],
            _ => vec!["-c"],
        };

        let mut cmd = Command::new(&shell_path);

        // Windows cmd.exe 默认使用 GBK 编码，切换代码页为 UTF-8 (65001)
        #[cfg(windows)]
        let actual_cmd = {
            if is_cmd_shell(&shell_name) {
                format!("chcp 65001 >nul && {}", command)
            } else {
                command.to_string()
            }
        };
        #[cfg(not(windows))]
        let actual_cmd = command;

        #[cfg(windows)]
        {
            if shell_name == "cmd" || shell_name == "cmd.exe" {
                cmd.args(&shell_args).raw_arg(&actual_cmd);
            } else {
                cmd.args(&shell_args).arg(&actual_cmd);
            }
        }
        #[cfg(not(windows))]
        {
            cmd.args(&shell_args).arg(&actual_cmd);
        }

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

        // 抑制 macOS 终端会话恢复
        #[cfg(target_os = "macos")]
        cmd.env("SHELL_SESSION_DID_INIT", "1");

        // Windows 上强制 Python 子进程使用 UTF-8 输出
        #[cfg(windows)]
        cmd.env("PYTHONIOENCODING", "utf-8");

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Unix: 使用 setsid 创建新进程组，以便后续可以 killpg 杀死整个进程树
        #[cfg(unix)]
        {
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        // 在 Windows 上使用 CREATE_NO_WINDOW 标志
        #[cfg(windows)]
        {
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
                    result: HashMap::new(),
                };
            }
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let mut stdout_reader = BufReader::new(stdout);
        let mut stderr_reader = BufReader::new(stderr);

        // 创建输出通道
        let (tx, mut rx) = mpsc::channel::<(String, bool)>(100);
        let tx_stdout = tx.clone();
        let tx_stderr = tx;

        // 读取 stdout
        let stdout_handle = tokio::spawn(async move {
            let mut stdout_capture = StdoutCapture::new();
            let mut read_buffer = [0u8; 4096];
            let mut pending = Vec::new();

            loop {
                let n = match stdout_reader.read(&mut read_buffer).await {
                    Ok(n) => n,
                    Err(err) => {
                        warn!("Failed to read stdout: {}", err);
                        break;
                    }
                };
                if n == 0 {
                    break;
                }

                for chunk in split_stream_chunks(&mut pending, &read_buffer[..n]) {
                    let visible_chunk = stdout_capture.process_chunk(&chunk);
                    if !visible_chunk.is_empty() {
                        let _ = tx_stdout.send((visible_chunk, false)).await;
                    }
                }
            }

            if let Some(tail) = flush_stream_buffer(&mut pending) {
                let visible_tail = stdout_capture.process_chunk(&tail);
                if !visible_tail.is_empty() {
                    let _ = tx_stdout.send((visible_tail, false)).await;
                }
            }

            stdout_capture.finish()
        });

        // 读取 stderr
        let stderr_handle = tokio::spawn(async move {
            let mut stderr_output = OutputTail::default();
            let mut read_buffer = [0u8; 4096];
            let mut pending = Vec::new();

            loop {
                let n = match stderr_reader.read(&mut read_buffer).await {
                    Ok(n) => n,
                    Err(err) => {
                        warn!("Failed to read stderr: {}", err);
                        break;
                    }
                };
                if n == 0 {
                    break;
                }

                for chunk in split_stream_chunks(&mut pending, &read_buffer[..n]) {
                    stderr_output.append(&chunk);
                    let _ = tx_stderr.send((chunk.clone(), true)).await;
                }
            }

            if let Some(tail) = flush_stream_buffer(&mut pending) {
                stderr_output.append(&tail);
                let _ = tx_stderr.send((tail.clone(), true)).await;
            }

            stderr_output.finish()
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
        // 注意：callback_handle 必须在 stdout/stderr handle 之后等待，
        // 确保所有日志发送回调完成后才返回，避免任务完成了但日志还没发完
        let timed_future = timeout(Duration::from_secs(timeout_secs), async {
            let (stdout, result) = stdout_handle
                .await
                .unwrap_or_else(|_| (String::new(), HashMap::new()));
            let stderr = stderr_handle.await.unwrap_or_default();
            // 等待所有日志发送完毕（senders 已 drop，rx 会自然结束）
            let _ = callback_handle.await;
            let status = child.wait().await;
            (stdout, stderr, result, status)
        });

        // If we have a cancel receiver, `select!` between timeout and cancellation
        if let Some(mut cancel_rx) = cancel_rx {
            tokio::select! {
                result = timed_future => {
                    match result {
                        Ok((stdout, stderr, result, status)) => {
                            let exit_code = status
                                .map(|s| s.code().unwrap_or(-1))
                                .unwrap_or(-1);
                            ExecutionResult {
                                exit_code,
                                stdout,
                                stderr,
                                timed_out: false,
                                cancelled: false,
                                result,
                            }
                        }
                        Err(_) => {
                            warn!("Command timed out after {} seconds", timeout_secs);
                            kill_process_tree(&mut child).await;
                            ExecutionResult {
                                exit_code: -1,
                                stdout: String::new(),
                                stderr: format!("Command timed out after {} seconds", timeout_secs),
                                timed_out: true,
                                cancelled: false,
                                result: HashMap::new(),
                            }
                        }
                    }
                }
                _ = cancel_rx.changed() => {
                    warn!("Command cancelled");
                    kill_process_tree(&mut child).await;
                    ExecutionResult {
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: "Task was cancelled".to_string(),
                        timed_out: false,
                        cancelled: true,
                        result: HashMap::new(),
                    }
                }
            }
        } else {
            // No cancellation - original behavior
            let result = timed_future.await;
            match result {
                Ok((stdout, stderr, result, status)) => {
                    let exit_code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                    ExecutionResult {
                        exit_code,
                        stdout,
                        stderr,
                        timed_out: false,
                        cancelled: false,
                        result,
                    }
                }
                Err(_) => {
                    warn!("Command timed out after {} seconds", timeout_secs);
                    kill_process_tree(&mut child).await;
                    ExecutionResult {
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: format!("Command timed out after {} seconds", timeout_secs),
                        timed_out: true,
                        cancelled: false,
                        result: HashMap::new(),
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
    base_env: HashMap<String, String>,
}

impl TaskRunner {
    pub fn new(workspaces_path: PathBuf, base_env: HashMap<String, String>) -> Self {
        // 确保工作目录存在
        let _ = std::fs::create_dir_all(&workspaces_path);

        Self {
            workspaces_path,
            executor: CommandExecutor::new(3600),
            base_env,
        }
    }

    async fn ensure_repo_ready<F, Fut>(
        &self,
        workspace_dir: &Path,
        repo_name: &str,
        repo_url: &str,
        client_repo_ref: &str,
        client_repo_token: Option<&str>,
        on_output: Option<F>,
        continue_on_update_failure: bool,
        log_context: &str,
    ) -> Option<ExecutionResult>
    where
        F: Fn(String, bool) -> Fut + Send + Clone + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let repo_path = workspace_dir.join(repo_name);

        if !repo_path.exists() {
            info!("{} (clone): {} -> {:?}", log_context, repo_url, repo_path);
            let clone_result = self
                .clone_repo(
                    repo_url,
                    &repo_path,
                    client_repo_ref,
                    client_repo_token,
                    on_output,
                )
                .await;
            if clone_result.exit_code != 0 {
                return Some(clone_result);
            }
            return None;
        }

        info!("{} (update): {:?}", log_context, repo_path);
        let update_result = self
            .update_repo(
                repo_url,
                &repo_path,
                client_repo_ref,
                client_repo_token,
                on_output,
            )
            .await;

        if update_result.exit_code != 0 {
            if continue_on_update_failure {
                warn!("Failed to update repository (will continue anyway)");
                return None;
            }
            return Some(update_result);
        }

        None
    }

    /// 运行任务
    pub async fn run_task<F, Fut>(
        &self,
        task_id: i64,
        execution_mode: &str,
        command: &str,
        code: Option<&InlineCode>,
        workspace_name: &str,
        client_repo_url: Option<&str>,
        client_repo_ref: &str,
        client_repo_token: Option<&str>,
        prepare_repo_before_execute: bool,
        cleanup_workspace_on_success: bool,
        timeout_secs: u64,
        on_output: Option<F>,
        environment: Option<HashMap<String, String>>,
        cancel_rx: Option<watch::Receiver<bool>>,
    ) -> ExecutionResult
    where
        F: Fn(String, bool) -> Fut + Send + Clone + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let normalized_mode = if execution_mode.eq_ignore_ascii_case("code") {
            "code"
        } else {
            "command"
        };

        info!(
            "Running task {} in workspace '{}' with execution mode '{}'",
            task_id, workspace_name, normalized_mode
        );
        if normalized_mode == "command" {
            info!("Command input: {}", command);
        }
        debug!(
            "client_repo_url={:?}, client_repo_ref={}",
            client_repo_url, client_repo_ref
        );

        // 获取/创建工作空间目录
        let mut workspace_dir = self.workspaces_path.join(workspace_name);
        if let Err(e) = std::fs::create_dir_all(&workspace_dir) {
            error!("Failed to create workspace directory: {}", e);
            return ExecutionResult {
                exit_code: -1,
                stdout: String::new(),
                stderr: format!("Failed to create workspace directory: {}", e),
                timed_out: false,
                cancelled: false,
                result: HashMap::new(),
            };
        }

        // 统一使用绝对路径，避免 code 模式下脚本路径与 cwd 的相对路径叠加导致找不到文件
        if let Ok(canonical_workspace) = std::fs::canonicalize(&workspace_dir) {
            workspace_dir = canonical_workspace;
        } else if workspace_dir.is_relative() {
            if let Ok(cwd) = std::env::current_dir() {
                workspace_dir = cwd.join(workspace_dir);
            }
        }

        // 确定执行目录: 始终使用 workspace 目录作为 cwd
        let exec_dir = workspace_dir.clone();

        // 从 URL 提取仓库名称，连字符替换为下划线以兼容 Python 包名
        let repo_name = client_repo_url.filter(|u| !u.is_empty()).map(|u| {
            u.trim_end_matches('/')
                .split('/')
                .last()
                .unwrap_or("repo")
                .trim_end_matches(".git")
                .replace('-', "_")
        });

        if prepare_repo_before_execute {
            if let (Some(repo_name), Some(repo_url)) = (repo_name.as_ref(), client_repo_url) {
                if let Some(repo_result) = self
                    .ensure_repo_ready(
                        &workspace_dir,
                        repo_name,
                        repo_url,
                        client_repo_ref,
                        client_repo_token,
                        on_output.clone(),
                        false,
                        "Preparing repository before execution",
                    )
                    .await
                {
                    return repo_result;
                }
            }
        }

        // command 模式下 command 为空时，执行 clone/update 后直接返回（workspace_acquire 阶段）
        if normalized_mode == "command" && command.is_empty() {
            if !prepare_repo_before_execute {
                if let Some(ref repo_name) = repo_name {
                    if let Some(repo_url) = client_repo_url {
                        if let Some(repo_result) = self
                            .ensure_repo_ready(
                                &workspace_dir,
                                repo_name,
                                repo_url,
                                client_repo_ref,
                                client_repo_token,
                                on_output.clone(),
                                true,
                                "Preparing repository for empty command",
                            )
                            .await
                        {
                            return repo_result;
                        }
                    }
                }
            }

            info!("No command specified, skipping script execution");
            return ExecutionResult {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
                cancelled: false,
                result: HashMap::new(),
            };
        }

        // 设置环境变量
        let mut task_env = self.base_env.clone();
        task_env.insert("TASKNEXUS_TASK_ID".to_string(), task_id.to_string());
        task_env.insert(
            "TASKNEXUS_WORKSPACE".to_string(),
            workspace_name.to_string(),
        );

        // 合并任务自定义环境变量，允许覆盖默认代理配置
        if let Some(env) = environment {
            task_env.extend(env);
        }

        let mut temp_code_path: Option<PathBuf> = None;
        let actual_command = if normalized_mode == "code" {
            let inline_code = match code {
                Some(value) => value,
                None => {
                    return ExecutionResult {
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: "Code mode selected but no code payload provided".to_string(),
                        timed_out: false,
                        cancelled: false,
                        result: HashMap::new(),
                    }
                }
            };

            let language = inline_code.language.trim().to_lowercase();
            if language != "shell" && language != "python" {
                return ExecutionResult {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!("Unsupported code language: {}", inline_code.language),
                    timed_out: false,
                    cancelled: false,
                    result: HashMap::new(),
                };
            }

            if inline_code.content.trim().is_empty() {
                return ExecutionResult {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: "Code content is empty".to_string(),
                    timed_out: false,
                    cancelled: false,
                    result: HashMap::new(),
                };
            }

            let temp_path = match Self::create_temp_code_file(
                &workspace_dir,
                task_id,
                &language,
                &inline_code.content,
            ) {
                Ok(path) => path,
                Err(err_msg) => {
                    return ExecutionResult {
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: err_msg,
                        timed_out: false,
                        cancelled: false,
                        result: HashMap::new(),
                    }
                }
            };
            temp_code_path = Some(temp_path.clone());
            Self::build_inline_code_command(&language, &temp_path)
        } else {
            // 根据仓库名和脚本路径构建实际执行命令
            if let Some(ref repo_name) = repo_name {
                let script_path = format!("{}/{}", repo_name, command);
                Self::build_script_command(&script_path)
            } else {
                Self::build_script_command(command)
            }
        };
        info!("Resolved command: {}", actual_command);

        // 执行命令
        let result = self
            .executor
            .execute(
                &actual_command,
                Some(&exec_dir),
                Some(&task_env),
                Some(timeout_secs),
                on_output,
                cancel_rx,
            )
            .await;

        // 清理 code 模式下创建的临时脚本文件
        if let Some(path) = temp_code_path {
            if let Err(e) = std::fs::remove_file(&path) {
                warn!("Failed to remove temp code file {:?}: {}", path, e);
            } else {
                debug!("Removed temp code file {:?}", path);
            }
        }

        if result.exit_code != 0 {
            error!("Command failed with exit code {}", result.exit_code);
            if !result.stderr.is_empty() {
                let stderr_preview: String = result.stderr.chars().take(500).collect();
                error!("stderr: {}", stderr_preview);
            }
        }

        if cleanup_workspace_on_success
            && result.exit_code == 0
            && !result.timed_out
            && !result.cancelled
        {
            if let Err(e) = std::fs::remove_dir_all(&workspace_dir) {
                warn!(
                    "Failed to cleanup workspace directory {:?}: {}",
                    workspace_dir, e
                );
            } else {
                info!("Cleaned up workspace directory {:?}", workspace_dir);
            }
        }

        result
    }

    fn create_temp_code_file(
        workspace_dir: &Path,
        task_id: i64,
        language: &str,
        content: &str,
    ) -> Result<PathBuf, String> {
        let ext = if language == "python" { "py" } else { "sh" };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let file_name = format!(".tasknexus_inline_{}_{}.{}", task_id, now, ext);
        let file_path = workspace_dir.join(file_name);
        std::fs::write(&file_path, content)
            .map_err(|e| format!("Failed to write inline code file: {}", e))?;
        Ok(file_path)
    }

    fn build_inline_code_command(language: &str, script_path: &Path) -> String {
        if language == "python" {
            Self::build_python_command(script_path.to_str().unwrap_or(""))
        } else {
            format!("bash {}", script_path.display())
        }
    }

    /// 根据脚本路径的扩展名生成执行命令
    fn build_script_command(script_path: &str) -> String {
        let ext = script_path.rsplit('.').next().unwrap_or("");
        match ext {
            "py" => Self::build_python_command(script_path),
            "sh" => format!("bash {}", script_path),
            "js" => format!("node {}", script_path),
            "ts" => format!("npx ts-node {}", script_path),
            "rb" => format!("ruby {}", script_path),
            _ => script_path.to_string(),
        }
    }

    fn build_python_command(script_path: &str) -> String {
        #[cfg(windows)]
        {
            format!("python {}", script_path)
        }
        #[cfg(not(windows))]
        {
            format!(
                "if command -v python3 >/dev/null 2>&1; then python3 {}; else python {}; fi",
                script_path, script_path
            )
        }
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
            "git clone --progress --depth 1 --branch {} {} {}",
            ref_name, auth_url, repo_name
        );

        info!("Cloning: {} (in {:?})", repo_url, target_path.parent());

        let mut env = self.base_env.clone();
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

        let mut env = self.base_env.clone();
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

#[cfg(test)]
mod tests {
    use super::{StdoutCapture, RESULT_BEGIN_MARKER, RESULT_END_MARKER};

    #[test]
    fn stdout_capture_extracts_structured_result_and_hides_markers() {
        let mut capture = StdoutCapture::new();
        let first = capture.process_chunk("hello\n");
        let second = capture.process_chunk(RESULT_BEGIN_MARKER);
        let third = capture.process_chunk("{\"artifact\":\"demo.apk\"}");
        let fourth = capture.process_chunk(RESULT_END_MARKER);
        let fifth = capture.process_chunk("\nworld\n");
        let (stdout, result) = capture.finish();

        assert_eq!(first, "hello\n");
        assert_eq!(second, "");
        assert_eq!(third, "");
        assert_eq!(fourth, "");
        assert_eq!(fifth, "world\n");
        assert_eq!(stdout, "hello\nworld\n");
        assert_eq!(
            result.get("artifact").and_then(|value| value.as_str()),
            Some("demo.apk")
        );
    }

    #[test]
    fn stdout_capture_restores_incomplete_marker_block_to_output() {
        let mut capture = StdoutCapture::new();
        let emitted = capture.process_chunk("prefix\n");
        let hidden_start = capture.process_chunk(RESULT_BEGIN_MARKER);
        let hidden_body = capture.process_chunk("{\"artifact\":\"demo.apk\"");
        let (stdout, result) = capture.finish();

        assert_eq!(emitted, "prefix\n");
        assert_eq!(hidden_start, "");
        assert_eq!(hidden_body, "");
        assert_eq!(
            stdout,
            format!("prefix\n{}{{\"artifact\":\"demo.apk\"", RESULT_BEGIN_MARKER)
        );
        assert!(result.is_empty());
    }
}
