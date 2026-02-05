//! 开机自启动管理模块
//!
//! 提供跨平台的开机自启动功能，支持 Windows、Linux 和 macOS。

use auto_launch::AutoLaunchBuilder;
use std::path::PathBuf;
use tracing::{error, info};

/// 开机自启动管理器
pub struct AutoStartManager {
    auto_launch: auto_launch::AutoLaunch,
}

impl AutoStartManager {
    /// 创建自启动管理器
    ///
    /// # Arguments
    /// * `app_name` - 应用名称
    /// * `config_path` - 配置文件路径（可选），用于启动时传递参数
    /// * `extra_args` - 额外的启动参数（可选）
    pub fn new(
        app_name: &str,
        config_path: Option<&PathBuf>,
        extra_args: Option<&[String]>,
    ) -> Result<Self, String> {
        let current_exe = std::env::current_exe()
            .map_err(|e| format!("无法获取当前可执行文件路径: {}", e))?;
        
        let app_path = current_exe.to_string_lossy().to_string();
        
        // 构建启动参数
        let mut args: Vec<String> = Vec::new();
        
        // 添加配置文件参数
        if let Some(config) = config_path {
            let config_str = config.to_string_lossy().to_string();
            args.push("--config".to_string());
            args.push(config_str);
        }
        
        // 添加额外参数
        if let Some(extra) = extra_args {
            args.extend(extra.iter().cloned());
        }
        
        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        
        let auto_launch = AutoLaunchBuilder::new()
            .set_app_name(app_name)
            .set_app_path(&app_path)
            .set_args(&args_refs)
            .build()
            .map_err(|e| format!("创建自启动管理器失败: {}", e))?;
        
        Ok(Self { auto_launch })
    }

    /// 启用开机自启动
    pub fn enable(&self) -> Result<(), String> {
        self.auto_launch
            .enable()
            .map_err(|e| format!("启用开机自启动失败: {}", e))?;
        
        info!("开机自启动已启用");
        Ok(())
    }

    /// 禁用开机自启动
    pub fn disable(&self) -> Result<(), String> {
        self.auto_launch
            .disable()
            .map_err(|e| format!("禁用开机自启动失败: {}", e))?;
        
        info!("开机自启动已禁用");
        Ok(())
    }

    /// 检查开机自启动是否已启用
    pub fn is_enabled(&self) -> Result<bool, String> {
        self.auto_launch
            .is_enabled()
            .map_err(|e| format!("检查开机自启动状态失败: {}", e))
    }

    /// 获取开机自启动状态的描述
    pub fn status(&self) -> String {
        match self.is_enabled() {
            Ok(true) => "已启用".to_string(),
            Ok(false) => "已禁用".to_string(),
            Err(e) => {
                error!("获取状态失败: {}", e);
                format!("未知 ({})", e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_manager() {
        let result = AutoStartManager::new("test-app", None, None);
        assert!(result.is_ok());
    }
}
