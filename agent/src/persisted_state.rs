use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{AgentError, Result};

const STATE_DIR_NAME: &str = ".tasknexus_agent";
const STATE_FILE_NAME: &str = "agent_state.json";
const CURRENT_STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PersistedTaskStateKind {
    Running,
    CompletedPendingSync,
    FailedPendingSync,
    LostOnRestart,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistedFinalPayload {
    #[serde(default)]
    pub payload_type: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub result: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub error: String,
}

impl PersistedFinalPayload {
    pub fn completed(
        exit_code: i32,
        stdout: String,
        stderr: String,
        result: HashMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            payload_type: "task_completed".to_string(),
            exit_code: Some(exit_code),
            stdout,
            stderr,
            result,
            error: String::new(),
        }
    }

    pub fn failed(error: String) -> Self {
        Self {
            payload_type: "task_failed".to_string(),
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            result: HashMap::new(),
            error,
        }
    }

    pub fn to_state_sync_payload(&self) -> serde_json::Value {
        let mut payload = serde_json::Map::new();
        if !self.payload_type.is_empty() {
            payload.insert(
                "type".to_string(),
                serde_json::Value::String(self.payload_type.clone()),
            );
        }
        if let Some(exit_code) = self.exit_code {
            payload.insert(
                "exit_code".to_string(),
                serde_json::Value::Number(serde_json::Number::from(exit_code)),
            );
        }
        if !self.stdout.is_empty() {
            payload.insert(
                "stdout".to_string(),
                serde_json::Value::String(self.stdout.clone()),
            );
        }
        if !self.stderr.is_empty() {
            payload.insert(
                "stderr".to_string(),
                serde_json::Value::String(self.stderr.clone()),
            );
        }
        if !self.result.is_empty() {
            payload.insert(
                "result".to_string(),
                serde_json::Value::Object(self.result.clone().into_iter().collect()),
            );
        }
        if !self.error.is_empty() {
            payload.insert(
                "error".to_string(),
                serde_json::Value::String(self.error.clone()),
            );
        }
        serde_json::Value::Object(payload)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTaskState {
    pub task_id: i64,
    pub local_state: PersistedTaskStateKind,
    #[serde(default)]
    pub workspace_name: String,
    #[serde(default)]
    pub final_payload: PersistedFinalPayload,
    #[serde(default)]
    pub local_log_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedStateFile {
    version: u32,
    #[serde(default)]
    tasks: Vec<PersistedTaskState>,
}

impl Default for PersistedStateFile {
    fn default() -> Self {
        Self {
            version: CURRENT_STATE_VERSION,
            tasks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PersistedStateStore {
    state_file_path: PathBuf,
    tasks: HashMap<i64, PersistedTaskState>,
}

impl PersistedStateStore {
    pub fn load(workspaces_path: &Path) -> Result<Self> {
        let state_dir = workspaces_path.join(STATE_DIR_NAME);
        let state_file_path = state_dir.join(STATE_FILE_NAME);
        if !state_file_path.exists() {
            return Ok(Self {
                state_file_path,
                tasks: HashMap::new(),
            });
        }

        let content = fs::read_to_string(&state_file_path).map_err(|e| {
            AgentError::Execution(format!(
                "Failed to read persisted agent state '{}': {}",
                state_file_path.display(),
                e
            ))
        })?;
        let decoded: PersistedStateFile = serde_json::from_str(&content).map_err(|e| {
            AgentError::Execution(format!(
                "Failed to parse persisted agent state '{}': {}",
                state_file_path.display(),
                e
            ))
        })?;

        let tasks = decoded
            .tasks
            .into_iter()
            .map(|task| (task.task_id, task))
            .collect::<HashMap<_, _>>();

        Ok(Self {
            state_file_path,
            tasks,
        })
    }

    pub fn recover_after_restart(&mut self) -> bool {
        let mut changed = false;
        for task in self.tasks.values_mut() {
            if task.local_state == PersistedTaskStateKind::Running {
                task.local_state = PersistedTaskStateKind::LostOnRestart;
                changed = true;
            }
        }
        changed
    }

    pub fn upsert_running(
        &mut self,
        task_id: i64,
        workspace_name: String,
        local_log_path: PathBuf,
    ) {
        self.tasks.insert(
            task_id,
            PersistedTaskState {
                task_id,
                local_state: PersistedTaskStateKind::Running,
                workspace_name,
                final_payload: PersistedFinalPayload::default(),
                local_log_path: local_log_path.to_string_lossy().into_owned(),
            },
        );
    }

    pub fn mark_completed(
        &mut self,
        task_id: i64,
        workspace_name: String,
        local_log_path: PathBuf,
        exit_code: i32,
        stdout: String,
        stderr: String,
        result: HashMap<String, serde_json::Value>,
    ) {
        self.tasks.insert(
            task_id,
            PersistedTaskState {
                task_id,
                local_state: PersistedTaskStateKind::CompletedPendingSync,
                workspace_name,
                final_payload: PersistedFinalPayload::completed(exit_code, stdout, stderr, result),
                local_log_path: local_log_path.to_string_lossy().into_owned(),
            },
        );
    }

    pub fn mark_failed(
        &mut self,
        task_id: i64,
        workspace_name: String,
        local_log_path: PathBuf,
        error: String,
    ) {
        self.tasks.insert(
            task_id,
            PersistedTaskState {
                task_id,
                local_state: PersistedTaskStateKind::FailedPendingSync,
                workspace_name,
                final_payload: PersistedFinalPayload::failed(error),
                local_log_path: local_log_path.to_string_lossy().into_owned(),
            },
        );
    }

    pub fn remove(&mut self, task_id: i64) -> Option<PersistedTaskState> {
        self.tasks.remove(&task_id)
    }

    pub fn get(&self, task_id: i64) -> Option<&PersistedTaskState> {
        self.tasks.get(&task_id)
    }

    pub fn snapshot(&self) -> Vec<PersistedTaskState> {
        let mut entries = self.tasks.values().cloned().collect::<Vec<_>>();
        entries.sort_by_key(|item| item.task_id);
        entries
    }

    pub fn save(&self) -> Result<()> {
        let parent = self.state_file_path.parent().ok_or_else(|| {
            AgentError::Execution("Persisted agent state path has no parent directory".to_string())
        })?;
        fs::create_dir_all(parent).map_err(|e| {
            AgentError::Execution(format!(
                "Failed to create persisted agent state directory '{}': {}",
                parent.display(),
                e
            ))
        })?;

        let payload = PersistedStateFile {
            version: CURRENT_STATE_VERSION,
            tasks: self.snapshot(),
        };
        let serialized = serde_json::to_string_pretty(&payload).map_err(|e| {
            AgentError::Execution(format!("Failed to serialize persisted agent state: {}", e))
        })?;
        fs::write(&self.state_file_path, serialized).map_err(|e| {
            AgentError::Execution(format!(
                "Failed to write persisted agent state '{}': {}",
                self.state_file_path.display(),
                e
            ))
        })?;
        Ok(())
    }
}
