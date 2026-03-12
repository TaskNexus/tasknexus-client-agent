//! Self-update helper for TaskNexus agent.
//!
//! Agent resolves latest release by itself, downloads assets, verifies manifest
//! signature and file checksums, then launches external updater.

use crate::error::{AgentError, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_KEY_ID: &str = "main";
const DEFAULT_RELEASE_API: &str =
    "https://api.github.com/repos/TaskNexus/tasknexus-client-agent/releases/latest";
const UPDATE_USER_AGENT: &str = "TaskNexus-Agent-SelfUpdate/1.0";

// Build-time embedded default key for key_id=main.
// In CI set TASKNEXUS_UPDATE_PUBLIC_KEY_MAIN to match MANIFEST_SIGNING_PRIVATE_KEY_B64.
const DEFAULT_PUBLIC_KEY_MAIN_B64: &str = match option_env!("TASKNEXUS_UPDATE_PUBLIC_KEY_MAIN") {
    Some(value) => value,
    None => "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
};

pub struct SelfUpdateResult {
    pub target_version: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseManifest {
    #[serde(default)]
    key_id: Option<String>,
    #[serde(default)]
    assets: Vec<ManifestAsset>,
}

#[derive(Debug, Deserialize)]
struct ManifestAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    sha256: String,
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
    manifest_url: String,
    manifest_sig_url: String,
}

pub async fn perform_self_update(config_path: &Path) -> Result<SelfUpdateResult> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| AgentError::Execution(format!("Failed to build HTTP client: {}", e)))?;

    let plan = resolve_release_plan(&client).await?;

    let manifest_json = http_get_text(&client, &plan.manifest_url).await?;
    let manifest_sig = http_get_text(&client, &plan.manifest_sig_url).await?;
    let manifest_sig = manifest_sig.trim().to_string();
    if manifest_sig.is_empty() {
        return Err(AgentError::Execution(
            "Manifest signature is empty".to_string(),
        ));
    }

    let manifest: ReleaseManifest = serde_json::from_str(&manifest_json)
        .map_err(|e| AgentError::Execution(format!("Invalid manifest JSON payload: {}", e)))?;

    let key_id = manifest
        .key_id
        .as_deref()
        .unwrap_or(DEFAULT_KEY_ID)
        .trim()
        .to_string();
    verify_manifest_signature(&manifest_json, &manifest_sig, &key_id)?;

    let expected_agent_hash =
        find_expected_hash(&manifest, &plan.agent_url, Some(plan.agent_name.as_str()))?;
    let expected_updater_hash = find_expected_hash(
        &manifest,
        &plan.updater_url,
        Some(plan.updater_name.as_str()),
    )?;

    let temp_dir = create_temp_dir()?;
    let agent_download_path = temp_dir.join(&plan.agent_name);
    let updater_download_path = temp_dir.join(&plan.updater_name);

    download_file(&client, &plan.agent_url, &agent_download_path).await?;
    download_file(&client, &plan.updater_url, &updater_download_path).await?;

    verify_file_sha256(&agent_download_path, &expected_agent_hash)?;
    verify_file_sha256(&updater_download_path, &expected_updater_hash)?;

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
    let mut manifest_url = String::new();
    let mut manifest_sig_url = String::new();

    for asset in release.assets {
        if asset.name == agent_name {
            agent_url = asset.browser_download_url;
        } else if asset.name == updater_name {
            updater_url = asset.browser_download_url;
        } else if asset.name == "manifest.json" {
            manifest_url = asset.browser_download_url;
        } else if asset.name == "manifest.sig" {
            manifest_sig_url = asset.browser_download_url;
        }
    }

    if agent_url.is_empty()
        || updater_url.is_empty()
        || manifest_url.is_empty()
        || manifest_sig_url.is_empty()
    {
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
        manifest_url,
        manifest_sig_url,
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

fn verify_manifest_signature(manifest_json: &str, signature_b64: &str, key_id: &str) -> Result<()> {
    let key_id = if key_id.trim().is_empty() {
        DEFAULT_KEY_ID
    } else {
        key_id.trim()
    };
    let public_key_b64 = resolve_public_key(key_id)?;

    let public_key_raw = BASE64_STANDARD
        .decode(public_key_b64.trim())
        .map_err(|e| AgentError::Execution(format!("Invalid public key base64: {}", e)))?;
    let public_key_bytes: [u8; 32] = public_key_raw.try_into().map_err(|_| {
        AgentError::Execution("Invalid public key length: expected 32 bytes".to_string())
    })?;
    let verifying_key = VerifyingKey::from_bytes(&public_key_bytes)
        .map_err(|e| AgentError::Execution(format!("Invalid public key bytes: {}", e)))?;

    let signature_raw = BASE64_STANDARD
        .decode(signature_b64.trim())
        .map_err(|e| AgentError::Execution(format!("Invalid manifest signature base64: {}", e)))?;
    let signature = Signature::from_slice(&signature_raw)
        .map_err(|e| AgentError::Execution(format!("Invalid manifest signature bytes: {}", e)))?;

    verifying_key
        .verify(manifest_json.as_bytes(), &signature)
        .map_err(|e| {
            AgentError::Execution(format!("Manifest signature verification failed: {}", e))
        })?;
    Ok(())
}

fn resolve_public_key(key_id: &str) -> Result<String> {
    let env_key = format!("TASKNEXUS_UPDATE_PUBLIC_KEY_{}", sanitize_key_id(key_id));
    if let Ok(key) = std::env::var(env_key) {
        if !key.trim().is_empty() {
            return Ok(key);
        }
    }

    match key_id {
        DEFAULT_KEY_ID => Ok(DEFAULT_PUBLIC_KEY_MAIN_B64.to_string()),
        _ => Err(AgentError::Execution(format!(
            "No public key configured for key_id '{}'",
            key_id
        ))),
    }
}

fn sanitize_key_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn find_expected_hash(manifest: &ReleaseManifest, url: &str, name: Option<&str>) -> Result<String> {
    if let Some(asset) = manifest
        .assets
        .iter()
        .find(|item| !item.url.is_empty() && item.url == url)
    {
        return normalize_sha256(&asset.sha256);
    }

    if let Some(target_name) = name {
        if let Some(asset) = manifest.assets.iter().find(|item| item.name == target_name) {
            return normalize_sha256(&asset.sha256);
        }
    }

    Err(AgentError::Execution(format!(
        "Missing hash entry in manifest for url='{}' name='{}'",
        url,
        name.unwrap_or("")
    )))
}

fn normalize_sha256(value: &str) -> Result<String> {
    let trimmed = value.trim().to_ascii_lowercase();
    if trimmed.len() != 64 || !trimmed.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(AgentError::Execution(format!(
            "Invalid sha256 value in manifest: '{}'",
            value
        )));
    }
    Ok(trimmed)
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

fn verify_file_sha256(path: &Path, expected_sha: &str) -> Result<()> {
    let content = std::fs::read(path).map_err(|e| {
        AgentError::Execution(format!(
            "Failed to read file for sha256 '{}': {}",
            path.display(),
            e
        ))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&content);
    let actual_sha = format!("{:x}", hasher.finalize());
    if actual_sha != expected_sha {
        return Err(AgentError::Execution(format!(
            "SHA256 mismatch for '{}': expected {}, got {}",
            path.display(),
            expected_sha,
            actual_sha
        )));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::{normalize_sha256, sanitize_key_id};

    #[test]
    fn test_normalize_sha256_accepts_valid_hex() {
        let value = "A3c4e4a11f08d85b1bf40ba58f006f8d4a4f6d49f2c6fc7183762c508d57f075";
        let normalized = normalize_sha256(value).expect("sha should be valid");
        assert_eq!(
            normalized,
            "a3c4e4a11f08d85b1bf40ba58f006f8d4a4f6d49f2c6fc7183762c508d57f075"
        );
    }

    #[test]
    fn test_normalize_sha256_rejects_invalid_length() {
        assert!(normalize_sha256("1234").is_err());
    }

    #[test]
    fn test_sanitize_key_id() {
        assert_eq!(sanitize_key_id("main"), "MAIN");
        assert_eq!(sanitize_key_id("prod-key.2026"), "PROD_KEY_2026");
    }
}
