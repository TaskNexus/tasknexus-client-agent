//! Self-update for TaskNexus agent.
//!
//! Agent resolves latest release, downloads the new agent binary, replaces
//! itself in-process, then exits so the service manager can restart with
//! the updated binary.  No external updater binary is needed.

use crate::error::{AgentError, Result};
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

const DEFAULT_RELEASE_API: &str =
    "https://api.github.com/repos/TaskNexus/tasknexus-client-agent/releases/latest";
const UPDATE_USER_AGENT: &str = "TaskNexus-Agent-SelfUpdate/1.0";

pub struct SelfUpdateResult {
    pub target_version: String,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubReleaseAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
}

struct ReleasePlan {
    target_version: String,
    agent_name: String,
    agent_url: String,
}

/// 执行自更新：下载新二进制并就地替换当前可执行文件。
///
/// 成功后调用者应使用非零退出码退出，让服务管理器自动重启。
///
/// 如果提供了 `download_url`，则跳过 GitHub Release API 解析，
/// 直接从给定 URL 下载二进制文件。
pub async fn perform_self_update(download_url: Option<&str>) -> Result<SelfUpdateResult> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| AgentError::Execution(format!("Failed to build HTTP client: {}", e)))?;

    let plan = if let Some(url) = download_url {
        resolve_direct_download_plan(url)?
    } else {
        resolve_release_plan(&client).await?
    };

    let temp_dir = create_temp_dir()?;
    let download_path = temp_dir.join(&plan.agent_name);

    info!(
        "Downloading agent binary '{}' from release {}",
        plan.agent_name, plan.target_version
    );
    download_file(&client, &plan.agent_url, &download_path).await?;
    ensure_executable(&download_path)?;

    // 就地替换当前二进制
    replace_current_binary(&download_path)?;

    // 清理临时目录
    let _ = std::fs::remove_dir_all(&temp_dir);

    Ok(SelfUpdateResult {
        target_version: plan.target_version,
    })
}

/// 启动时清理上次自更新遗留的 .bak 文件。
pub fn cleanup_previous_update() {
    if let Ok(current_exe) = std::env::current_exe() {
        let file_name = current_exe
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if file_name.is_empty() {
            return;
        }

        if let Some(parent) = current_exe.parent() {
            let bak_path = parent.join(format!("{}.bak", file_name));
            if bak_path.exists() {
                match std::fs::remove_file(&bak_path) {
                    Ok(_) => info!(
                        "Cleaned up previous update backup: {:?}",
                        bak_path
                    ),
                    Err(e) => warn!(
                        "Failed to clean up previous update backup {:?}: {}",
                        bak_path, e
                    ),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// 就地替换当前运行的可执行文件。
///
/// 流程:
///   1. 将新文件 copy 到 `<name>.new` (staging)
///   2. 将当前 exe rename 为 `<name>.bak`
///   3. 将 `.new` rename 为原 exe 名
///
/// Windows 允许 rename 正在运行的 exe，只是不能覆盖写入。
fn replace_current_binary(new_binary: &Path) -> Result<()> {
    let current_exe = std::env::current_exe().map_err(|e| {
        AgentError::Execution(format!("Failed to resolve current executable path: {}", e))
    })?;
    let parent = current_exe.parent().ok_or_else(|| {
        AgentError::Execution("Current executable has no parent directory".to_string())
    })?;
    let file_name = current_exe
        .file_name()
        .ok_or_else(|| {
            AgentError::Execution("Current executable has no file name".to_string())
        })?
        .to_string_lossy()
        .to_string();

    let staged = parent.join(format!("{}.new", file_name));
    let backup = parent.join(format!("{}.bak", file_name));

    // 清理上次可能遗留的文件
    let _ = std::fs::remove_file(&staged);
    let _ = std::fs::remove_file(&backup);

    // 复制新文件到 staging 位置
    std::fs::copy(new_binary, &staged).map_err(|e| {
        AgentError::Execution(format!(
            "Failed to stage new binary to '{}': {}",
            staged.display(),
            e
        ))
    })?;
    ensure_executable(&staged)?;

    // 将当前运行的 exe rename 为 .bak
    std::fs::rename(&current_exe, &backup).map_err(|e| {
        AgentError::Execution(format!(
            "Failed to rename current executable to backup '{}': {}",
            backup.display(),
            e
        ))
    })?;

    // 将 staged 移到正式位置
    std::fs::rename(&staged, &current_exe).map_err(|e| {
        // 尝试恢复：将 backup rename 回去
        let _ = std::fs::rename(&backup, &current_exe);
        AgentError::Execution(format!(
            "Failed to rename staged binary to '{}': {}",
            current_exe.display(),
            e
        ))
    })?;

    info!("Binary replaced successfully: {:?}", current_exe);
    Ok(())
}

async fn resolve_release_plan(client: &reqwest::Client) -> Result<ReleasePlan> {
    let release_api = std::env::var("TASKNEXUS_UPDATE_RELEASE_API")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASE_API.to_string());

    let release_json = http_get_text(client, &release_api).await?;
    let release: GithubRelease = serde_json::from_str(&release_json).map_err(|e| {
        AgentError::Execution(format!("Failed to parse release metadata JSON: {}", e))
    })?;

    let target_version = release.tag_name.trim().to_string();
    if target_version.is_empty() {
        return Err(AgentError::Execution(
            "Release metadata missing tag_name".to_string(),
        ));
    }

    let agent_name = platform_asset_name()?;
    let mut agent_url = String::new();

    for asset in release.assets {
        if asset.name == agent_name {
            agent_url = asset.browser_download_url;
            break;
        }
    }

    if agent_url.is_empty() {
        return Err(AgentError::Execution(format!(
            "Release asset '{}' not found for self-update",
            agent_name
        )));
    }

    Ok(ReleasePlan {
        target_version,
        agent_name,
        agent_url,
    })
}

/// 从外部传入的下载链接直接构建更新计划，跳过 GitHub Release API。
fn resolve_direct_download_plan(url: &str) -> Result<ReleasePlan> {
    // 从 URL 路径推断文件名，回退到平台默认名称
    let agent_name = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty() && !s.contains('?'))
        .map(|s| {
            // 去掉 query string
            if let Some(pos) = s.find('?') {
                s[..pos].to_string()
            } else {
                s.to_string()
            }
        })
        .unwrap_or_else(|| platform_asset_name().unwrap_or_else(|_| "tasknexus-agent".to_string()));

    Ok(ReleasePlan {
        target_version: "direct-download".to_string(),
        agent_name,
        agent_url: url.to_string(),
    })
}

/// 返回当前平台对应的 agent release asset 名称。
fn platform_asset_name() -> Result<String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    match (os, arch) {
        ("macos", "aarch64") => Ok("tasknexus-agent-macos-arm64".to_string()),
        ("windows", "x86_64") => Ok("tasknexus-agent-windows-x86_64.exe".to_string()),
        ("linux", "x86_64") => Ok("tasknexus-agent-linux-amd64".to_string()),
        ("linux", "aarch64") => Ok("tasknexus-agent-linux-arm64".to_string()),
        _ => Err(AgentError::Execution(format!(
            "Unsupported OS/ARCH for self-update: {}/{}",
            os, arch
        ))),
    }
}

async fn http_get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let mut request = client
        .get(url)
        .header(ACCEPT, "application/vnd.github+json")
        .header(USER_AGENT, UPDATE_USER_AGENT);

    if let Some(token) = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        request = request.header(AUTHORIZATION, format!("Bearer {}", token));
    }

    let response = request
        .send()
        .await
        .map_err(|e| AgentError::Execution(format!("HTTP request failed for '{}': {}", url, e)))?;
    let status = response.status();
    if !status.is_success() {
        return Err(AgentError::Execution(format!(
            "HTTP {} while requesting '{}'",
            status, url
        )));
    }

    response.text().await.map_err(|e| {
        AgentError::Execution(format!("Failed to read response text '{}': {}", url, e))
    })
}

async fn download_file(client: &reqwest::Client, url: &str, output_path: &Path) -> Result<()> {
    let bytes = http_get_bytes(client, url).await?;
    tokio::fs::write(output_path, &bytes).await.map_err(|e| {
        AgentError::Execution(format!(
            "Failed to write downloaded file '{}': {}",
            output_path.display(),
            e
        ))
    })?;
    Ok(())
}

async fn http_get_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let mut request = client
        .get(url)
        .header(ACCEPT, "application/octet-stream")
        .header(USER_AGENT, UPDATE_USER_AGENT);

    if let Some(token) = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        request = request.header(AUTHORIZATION, format!("Bearer {}", token));
    }

    let response = request
        .send()
        .await
        .map_err(|e| AgentError::Execution(format!("Failed to download '{}': {}", url, e)))?;
    let status = response.status();
    if !status.is_success() {
        return Err(AgentError::Execution(format!(
            "Failed to download '{}': HTTP {}",
            url, status
        )));
    }

    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|e| {
            AgentError::Execution(format!("Failed to read downloaded bytes '{}': {}", url, e))
        })
}

fn create_temp_dir() -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let temp_dir = std::env::temp_dir().join(format!(
        "tasknexus-self-update-{}-{}",
        std::process::id(),
        stamp
    ));
    std::fs::create_dir_all(&temp_dir)?;
    Ok(temp_dir)
}

fn ensure_executable(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(_path, perms)?;
    }
    Ok(())
}
