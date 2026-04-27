//! Linux systemd 服务管理实现
//!
//! 通过生成 systemd unit 文件实现服务的安装、卸载、启停和状态查询。

use std::path::Path;

use super::{display_name_for, SERVICE_DESCRIPTION};

type BoxError = Box<dyn std::error::Error>;

/// 根据服务名称生成 systemd unit 文件路径和 unit 名称
fn unit_file_path(service_name: &str) -> String {
    format!("/etc/systemd/system/{}.service", service_name)
}

fn unit_name(service_name: &str) -> String {
    format!("{}.service", service_name)
}

/// 安装 systemd 服务
pub fn install_service(service_name: &str, config_path: &Path) -> Result<(), BoxError> {
    let current_exe = std::env::current_exe()?;
    let exe_path = current_exe.to_string_lossy();
    let config_str = config_path.to_string_lossy();
    let display_name = display_name_for(service_name);
    let unit_path = unit_file_path(service_name);
    let unit = unit_name(service_name);

    // 检测 exe 同级目录下是否存在 .venv
    let mut env_lines = String::new();
    if let Some(exe_dir) = current_exe.parent() {
        let venv_path = exe_dir.join(".venv");
        if venv_path.exists() {
            let venv_str = venv_path.to_string_lossy();
            env_lines = format!(
                r#"Environment="VIRTUAL_ENV={venv}"
Environment="PATH={venv}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
"#,
                venv = venv_str
            );
        }
    }

    let unit_content = format!(
        r#"[Unit]
Description={description}
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exe} run --config {config} --service-name {svc_name}
Restart=on-failure
RestartSec=5
KillMode=mixed
TimeoutStopSec=30
{env_lines}
[Install]
WantedBy=multi-user.target
"#,
        description = SERVICE_DESCRIPTION,
        exe = exe_path,
        config = config_str,
        svc_name = service_name,
        env_lines = env_lines,
    );

    // 写入 unit 文件
    std::fs::write(&unit_path, &unit_content).map_err(|e| {
        format!(
            "写入 unit 文件 '{}' 失败（可能需要 root 权限）: {}",
            unit_path, e
        )
    })?;

    // daemon-reload
    run_systemctl(&["daemon-reload"])?;

    // enable
    run_systemctl(&["enable", &unit])?;

    let name_arg = if service_name == super::DEFAULT_SERVICE_NAME {
        String::new()
    } else {
        format!(" --name {}", service_name)
    };

    println!("服务 '{}' 安装成功", display_name);
    println!("  Unit 文件: {}", unit_path);
    println!("  配置文件:  {}", config_str);
    println!("  启动类型:  自动 (on boot)");
    if !env_lines.is_empty() {
        println!("  虚拟环境:  已自动检测并注入");
    }
    println!();
    println!("使用以下命令管理服务:");
    println!("  启动: sudo tasknexus-agent service start{}", name_arg);
    println!("  停止: sudo tasknexus-agent service stop{}", name_arg);
    println!("  重启: sudo tasknexus-agent service restart{}", name_arg);
    println!("  状态: tasknexus-agent service status{}", name_arg);
    println!("  卸载: sudo tasknexus-agent service uninstall{}", name_arg);
    println!();
    println!(
        "提示: 如需指定运行用户，请编辑 {} 并在 [Service] 下添加 User=<username>",
        unit_path
    );

    Ok(())
}

/// 卸载 systemd 服务
pub fn uninstall_service(service_name: &str) -> Result<(), BoxError> {
    let display_name = display_name_for(service_name);
    let unit_path = unit_file_path(service_name);
    let unit = unit_name(service_name);

    // 停止服务（忽略已停止的情况）
    let _ = run_systemctl(&["stop", &unit]);

    // 禁用服务
    let _ = run_systemctl(&["disable", &unit]);

    // 删除 unit 文件
    if std::path::Path::new(&unit_path).exists() {
        std::fs::remove_file(&unit_path).map_err(|e| {
            format!(
                "删除 unit 文件 '{}' 失败（可能需要 root 权限）: {}",
                unit_path, e
            )
        })?;
    }

    // daemon-reload
    run_systemctl(&["daemon-reload"])?;

    println!("服务 '{}' 已卸载", display_name);
    Ok(())
}

/// 启动服务
pub fn start_service(service_name: &str) -> Result<(), BoxError> {
    let display_name = display_name_for(service_name);
    let unit = unit_name(service_name);
    run_systemctl(&["start", &unit])?;
    println!("服务 '{}' 已启动", display_name);
    Ok(())
}

/// 停止服务
pub fn stop_service(service_name: &str) -> Result<(), BoxError> {
    let display_name = display_name_for(service_name);
    let unit = unit_name(service_name);
    run_systemctl(&["stop", &unit])?;
    println!("服务 '{}' 已停止", display_name);
    Ok(())
}

/// 查询服务状态
pub fn query_service_status(service_name: &str) -> Result<(), BoxError> {
    let unit = unit_name(service_name);
    let output = std::process::Command::new("systemctl")
        .args(["status", &unit, "--no-pager"])
        .output()
        .map_err(|e| format!("执行 systemctl 失败: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !stdout.is_empty() {
        print!("{}", stdout);
    }
    if !stderr.is_empty() {
        eprint!("{}", stderr);
    }

    Ok(())
}

/// 通过 systemd 重启服务
pub fn restart_via_service_manager() {
    // 使用非零退出码让 systemd 的 Restart=on-failure 策略自动重启
    std::process::exit(42);
}

/// 执行 systemctl 命令
fn run_systemctl(args: &[&str]) -> Result<(), BoxError> {
    let output = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|e| format!("执行 systemctl {:?} 失败: {}", args, e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "systemctl {:?} 失败 (exit {}): {}",
            args,
            output.status.code().unwrap_or(-1),
            stderr.trim()
        )
        .into());
    }

    Ok(())
}
