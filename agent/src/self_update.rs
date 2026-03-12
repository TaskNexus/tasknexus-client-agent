//! Self-update helper for TaskNexus agent.
//!
//! Agent resolves latest release by itself, downloads assets, then launches
//! external updater.

use crate::error::{AgentError, Result};
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    updater_name: String,
    updater_url: String,
}

pub async fn perform_self_update(config_path: &Path) -> Result<SelfUpdateResult> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| AgentError::Execution(format!("Failed to build HTTP client: {}", e)))?;

    let plan = resolve_release_plan(&client).await?;

    let temp_dir = create_temp_dir()?;
    let agent_download_path = temp_dir.join(&plan.agent_name);
    let updater_download_path = temp_dir.join(&plan.updater_name);

    download_file(&client, &plan.agent_url, &agent_download_path).await?;
    download_file(&client, &plan.updater_url, &updater_download_path).await?;

    ensure_executable(&agent_download_path)?;
    ensure_executable(&updater_download_path)?;

    launch_updater(&updater_download_path, &agent_download_path, config_path)?;

    Ok(SelfUpdateResult {
        target_version: plan.target_version,
    })
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

    let (agent_name, updater_name) = platform_asset_names()?;

    let mut agent_url = String::new();
    let mut updater_url = String::new();

    for asset in release.assets {
        if asset.name == agent_name {
            agent_url = asset.browser_download_url;
        } else if asset.name == updater_name {
            updater_url = asset.browser_download_url;
        }
    }

    if agent_url.is_empty() || updater_url.is_empty() {
        return Err(AgentError::Execution(format!(
            "Release assets incomplete for self-update (agent='{}', updater='{}')",
            agent_name, updater_name
        )));
    }

    Ok(ReleasePlan {
        target_version,
        agent_name,
        agent_url,
        updater_name,
        updater_url,
    })
}

fn platform_asset_names() -> Result<(String, String)> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    match (os, arch) {
        ("macos", "aarch64") => Ok((
            "tasknexus-agent-macos-arm64".to_string(),
            "tasknexus-updater-macos-arm64".to_string(),
        )),
        ("windows", "x86_64") => Ok((
            "tasknexus-agent-windows-x86_64.exe".to_string(),
            "tasknexus-updater-windows-x86_64.exe".to_string(),
        )),
        ("linux", "x86_64") => Ok((
            "tasknexus-agent-linux-amd64".to_string(),
            "tasknexus-updater-linux-amd64".to_string(),
        )),
        ("linux", "aarch64") => Ok((
            "tasknexus-agent-linux-arm64".to_string(),
            "tasknexus-updater-linux-arm64".to_string(),
        )),
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

fn ensure_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn launch_updater(updater_path: &Path, new_agent_path: &Path, config_path: &Path) -> Result<()> {
    let current_exe = std::env::current_exe().map_err(|e| {
        AgentError::Execution(format!("Failed to resolve current executable path: {}", e))
    })?;

    let mut cmd = std::process::Command::new(updater_path);
    cmd.arg("--pid")
        .arg(std::process::id().to_string())
        .arg("--current-exe")
        .arg(&current_exe)
        .arg("--new-exe")
        .arg(new_agent_path)
        .arg("--restart-arg")
        .arg("--config")
        .arg("--restart-arg")
        .arg(config_path);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd.spawn().map_err(|e| {
        AgentError::Execution(format!(
            "Failed to launch updater '{}': {}",
            updater_path.display(),
            e
        ))
    })?;
    Ok(())
}
