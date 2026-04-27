//! Linux systemd 服务管理实现
//!
//! 通过生成 systemd unit 文件实现服务的安装、卸载、启停和状态查询。

use std::path::Path;

use super::{SERVICE_DESCRIPTION, SERVICE_DISPLAY_NAME};

const UNIT_FILE_PATH: &str = "/etc/systemd/system/tasknexus-agent.service";
const SYSTEMD_SERVICE_NAME: &str = "tasknexus-agent.service";

type BoxError = Box<dyn std::error::Error>;

/// 安装 systemd 服务
pub fn install_service(config_path: &Path) -> Result<(), BoxError> {
    let current_exe = std::env::current_exe()?;
    let exe_path = current_exe.to_string_lossy();
    let config_str = config_path.to_string_lossy();

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
ExecStart={exe} run --config {config}
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
        env_lines = env_lines,
    );

    // 写入 unit 文件
    std::fs::write(UNIT_FILE_PATH, &unit_content).map_err(|e| {
        format!(
            "写入 unit 文件 '{}' 失败（可能需要 root 权限）: {}",
            UNIT_FILE_PATH, e
        )
    })?;

    // daemon-reload
    run_systemctl(&["daemon-reload"])?;

    // enable
    run_systemctl(&["enable", SYSTEMD_SERVICE_NAME])?;

    println!("服务 '{}' 安装成功", SERVICE_DISPLAY_NAME);
    println!("  Unit 文件: {}", UNIT_FILE_PATH);
    println!("  配置文件:  {}", config_str);
    println!("  启动类型:  自动 (on boot)");
    if !env_lines.is_empty() {
        println!("  虚拟环境:  已自动检测并注入");
    }
    println!();
    println!("使用以下命令管理服务:");
    println!("  启动: sudo tasknexus-agent service start");
    println!("  停止: sudo tasknexus-agent service stop");
    println!("  状态: tasknexus-agent service status");
    println!("  卸载: sudo tasknexus-agent service uninstall");
    println!();
    println!("提示: 如需指定运行用户，请编辑 {} 并在 [Service] 下添加 User=<username>", UNIT_FILE_PATH);

    Ok(())
}

/// 卸载 systemd 服务
pub fn uninstall_service() -> Result<(), BoxError> {
    // 停止服务（忽略已停止的情况）
    let _ = run_systemctl(&["stop", SYSTEMD_SERVICE_NAME]);

    // 禁用服务
    let _ = run_systemctl(&["disable", SYSTEMD_SERVICE_NAME]);

    // 删除 unit 文件
    if std::path::Path::new(UNIT_FILE_PATH).exists() {
        std::fs::remove_file(UNIT_FILE_PATH).map_err(|e| {
            format!(
                "删除 unit 文件 '{}' 失败（可能需要 root 权限）: {}",
                UNIT_FILE_PATH, e
            )
        })?;
    }

    // daemon-reload
    run_systemctl(&["daemon-reload"])?;

    println!("服务 '{}' 已卸载", SERVICE_DISPLAY_NAME);
    Ok(())
}

/// 启动服务
pub fn start_service() -> Result<(), BoxError> {
    run_systemctl(&["start", SYSTEMD_SERVICE_NAME])?;
    println!("服务 '{}' 已启动", SERVICE_DISPLAY_NAME);
    Ok(())
}

/// 停止服务
pub fn stop_service() -> Result<(), BoxError> {
    run_systemctl(&["stop", SYSTEMD_SERVICE_NAME])?;
    println!("服务 '{}' 已停止", SERVICE_DISPLAY_NAME);
    Ok(())
}

/// 查询服务状态
pub fn query_service_status() -> Result<(), BoxError> {
    let output = std::process::Command::new("systemctl")
        .args(["status", SYSTEMD_SERVICE_NAME, "--no-pager"])
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
