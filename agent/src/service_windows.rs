//! Windows Service 实现
//!
//! 使用 windows-service crate 实现 Windows 服务的安装、卸载、启停和状态查询。
//! SCM 服务入口逻辑位于 main.rs 中（因为需要调用 binary crate 的 run_agent）。

use std::path::Path;

#[cfg(windows)]
use std::ffi::OsString;
#[cfg(windows)]
use std::time::Duration;
#[cfg(windows)]
use windows_service::{
    service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState,
        ServiceType,
    },
    service_manager::{ServiceManager, ServiceManagerAccess},
};

use super::{display_name_for, SERVICE_DESCRIPTION};

type BoxError = Box<dyn std::error::Error>;

/// 安装 Windows 服务
pub fn install_service(service_name: &str, config_path: &Path) -> Result<(), BoxError> {
    #[cfg(not(windows))]
    {
        let _ = (service_name, config_path);
        Err("install_service is only supported on Windows".into())
    }

    #[cfg(windows)]
    {
        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)?;

        let current_exe = std::env::current_exe()?;
        let config_str = config_path.to_string_lossy().to_string();
        let display_name = display_name_for(service_name);

        let service_info = ServiceInfo {
            name: OsString::from(service_name),
            display_name: OsString::from(&display_name),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: current_exe,
            launch_arguments: vec![
                OsString::from("run"),
                OsString::from("--config"),
                OsString::from(&config_str),
                OsString::from("--service-name"),
                OsString::from(service_name),
            ],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };

        let service = manager.create_service(
            &service_info,
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::START,
        )?;

        // 设置服务描述
        service.set_description(SERVICE_DESCRIPTION)?;

        // 配置故障恢复策略：失败后自动重启
        use windows_service::service::{
            ServiceAction, ServiceActionType, ServiceFailureActions, ServiceFailureResetPeriod,
        };
        let failure_actions = ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86400)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(5),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(30),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(60),
                },
            ]),
        };
        service.update_failure_actions(failure_actions)?;

        // 允许非崩溃退出码也触发恢复操作（用于自更新场景）
        service.set_failure_actions_on_non_crash_failures(true)?;

        let name_arg = if service_name == super::DEFAULT_SERVICE_NAME {
            String::new()
        } else {
            format!(" --name {}", service_name)
        };

        println!("服务 '{}' 安装成功", display_name);
        println!("  服务名: {}", service_name);
        println!("  配置文件: {}", config_str);
        println!("  启动类型: 自动");
        println!();
        println!("使用以下命令管理服务:");
        println!("  启动: tasknexus-agent service start{}", name_arg);
        println!("  停止: tasknexus-agent service stop{}", name_arg);
        println!("  重启: tasknexus-agent service restart{}", name_arg);
        println!("  状态: tasknexus-agent service status{}", name_arg);
        println!("  卸载: tasknexus-agent service uninstall{}", name_arg);

        Ok(())
    }
}

/// 卸载 Windows 服务
pub fn uninstall_service(service_name: &str) -> Result<(), BoxError> {
    #[cfg(not(windows))]
    {
        let _ = service_name;
        Err("uninstall_service is only supported on Windows".into())
    }

    #[cfg(windows)]
    {
        let display_name = display_name_for(service_name);
        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(
            service_name,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
        )?;

        // 如果服务正在运行，先停止
        let status = service.query_status()?;
        if status.current_state != ServiceState::Stopped {
            println!("正在停止服务...");
            service.stop()?;

            // 等待停止完成
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let status = service.query_status()?;
                if status.current_state == ServiceState::Stopped {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    eprintln!("警告: 等待服务停止超时");
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }

        service.delete()?;
        println!("服务 '{}' 已卸载", display_name);
        Ok(())
    }
}

/// 启动服务
pub fn start_service(service_name: &str) -> Result<(), BoxError> {
    #[cfg(not(windows))]
    {
        let _ = service_name;
        Err("start_service is only supported on Windows".into())
    }

    #[cfg(windows)]
    {
        let display_name = display_name_for(service_name);
        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(
            service_name,
            ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )?;

        // 检查当前状态
        let status = service.query_status()?;
        if status.current_state == ServiceState::Running {
            println!("服务 '{}' 已经在运行中", display_name);
            return Ok(());
        }

        service.start::<OsString>(&[])?;

        // 等待启动完成
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let status = service.query_status()?;
            match status.current_state {
                ServiceState::Running => {
                    println!("服务 '{}' 已启动", display_name);
                    return Ok(());
                }
                ServiceState::Stopped => {
                    return Err(format!(
                        "服务 '{}' 启动后立即停止，请检查配置和日志",
                        display_name
                    )
                    .into());
                }
                _ => {
                    if std::time::Instant::now() >= deadline {
                        return Err(
                            format!("等待服务 '{}' 启动超时", display_name).into()
                        );
                    }
                }
            }
        }
    }
}

/// 停止服务
pub fn stop_service(service_name: &str) -> Result<(), BoxError> {
    #[cfg(not(windows))]
    {
        let _ = service_name;
        Err("stop_service is only supported on Windows".into())
    }

    #[cfg(windows)]
    {
        let display_name = display_name_for(service_name);
        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(
            service_name,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        )?;

        let status = service.query_status()?;
        if status.current_state == ServiceState::Stopped {
            println!("服务 '{}' 已经处于停止状态", display_name);
            return Ok(());
        }

        service.stop()?;

        // 等待停止完成
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let status = service.query_status()?;
            if status.current_state == ServiceState::Stopped {
                println!("服务 '{}' 已停止", display_name);
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                eprintln!("警告: 等待服务停止超时");
                return Ok(());
            }
        }
    }
}

/// 查询服务状态
pub fn query_service_status(service_name: &str) -> Result<(), BoxError> {
    #[cfg(not(windows))]
    {
        let _ = service_name;
        Err("query_service_status is only supported on Windows".into())
    }

    #[cfg(windows)]
    {
        let display_name = display_name_for(service_name);
        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(service_name, ServiceAccess::QUERY_STATUS)?;
        let status = service.query_status()?;

        let state_str = match status.current_state {
            ServiceState::Stopped => "已停止",
            ServiceState::StartPending => "正在启动",
            ServiceState::StopPending => "正在停止",
            ServiceState::Running => "运行中",
            ServiceState::ContinuePending => "正在恢复",
            ServiceState::PausePending => "正在暂停",
            ServiceState::Paused => "已暂停",
        };

        println!("服务: {} ({})", display_name, service_name);
        println!("状态: {}", state_str);
        if let Some(pid) = status.process_id {
            println!("PID:  {}", pid);
        }
        Ok(())
    }
}

/// 通过服务管理器重启（在自更新场景中使用非零退出码触发）
pub fn restart_via_service_manager() {
    // SCM 的故障恢复策略会在进程非零退出后自动重启
    std::process::exit(42);
}
