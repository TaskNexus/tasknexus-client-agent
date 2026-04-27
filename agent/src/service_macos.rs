//! macOS launchd 服务管理实现
//!
//! 通过生成 plist 文件实现服务的安装、卸载、启停和状态查询。

use std::path::Path;

use super::display_name_for;

type BoxError = Box<dyn std::error::Error>;

/// 根据服务名称生成 launchd label 和 plist 路径
fn plist_label(service_name: &str) -> String {
    format!("com.tasknexus.{}", service_name)
}

fn plist_path(service_name: &str) -> String {
    format!(
        "/Library/LaunchDaemons/com.tasknexus.{}.plist",
        service_name
    )
}

fn log_path(service_name: &str, ext: &str) -> String {
    format!("/var/log/{}.{}", service_name, ext)
}

/// 安装 launchd 服务
pub fn install_service(service_name: &str, config_path: &Path) -> Result<(), BoxError> {
    let current_exe = std::env::current_exe()?;
    let exe_path = current_exe.to_string_lossy();
    let config_str = config_path.to_string_lossy();
    let display_name = display_name_for(service_name);
    let label = plist_label(service_name);
    let plist = plist_path(service_name);
    let stdout_log = log_path(service_name, "log");
    let stderr_log = log_path(service_name, "err");

    let plist_content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>

  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>run</string>
    <string>--config</string>
    <string>{config}</string>
    <string>--service-name</string>
    <string>{svc_name}</string>
  </array>

  <key>RunAtLoad</key>
  <true/>

  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>

  <key>ThrottleInterval</key>
  <integer>5</integer>

  <key>StandardOutPath</key>
  <string>{stdout_log}</string>

  <key>StandardErrorPath</key>
  <string>{stderr_log}</string>
</dict>
</plist>
"#,
        label = label,
        exe = exe_path,
        config = config_str,
        svc_name = service_name,
        stdout_log = stdout_log,
        stderr_log = stderr_log,
    );

    std::fs::write(&plist, &plist_content).map_err(|e| {
        format!(
            "写入 plist 文件 '{}' 失败（可能需要 sudo 权限）: {}",
            plist, e
        )
    })?;

    // 加载服务
    run_launchctl(&["load", "-w", &plist])?;

    let name_arg = if service_name == super::DEFAULT_SERVICE_NAME {
        String::new()
    } else {
        format!(" --name {}", service_name)
    };

    println!("服务 '{}' 安装成功", display_name);
    println!("  Plist 文件: {}", plist);
    println!("  配置文件:   {}", config_str);
    println!("  启动类型:   开机自动启动");
    println!();
    println!("使用以下命令管理服务:");
    println!("  启动: sudo tasknexus-agent service start{}", name_arg);
    println!("  停止: sudo tasknexus-agent service stop{}", name_arg);
    println!("  重启: sudo tasknexus-agent service restart{}", name_arg);
    println!("  状态: tasknexus-agent service status{}", name_arg);
    println!("  卸载: sudo tasknexus-agent service uninstall{}", name_arg);

    Ok(())
}

/// 卸载 launchd 服务
pub fn uninstall_service(service_name: &str) -> Result<(), BoxError> {
    let display_name = display_name_for(service_name);
    let plist = plist_path(service_name);

    if std::path::Path::new(&plist).exists() {
        // 先卸载服务
        let _ = run_launchctl(&["unload", &plist]);

        // 删除 plist 文件
        std::fs::remove_file(&plist).map_err(|e| {
            format!(
                "删除 plist 文件 '{}' 失败（可能需要 sudo 权限）: {}",
                plist, e
            )
        })?;
    }

    println!("服务 '{}' 已卸载", display_name);
    Ok(())
}

/// 启动服务
pub fn start_service(service_name: &str) -> Result<(), BoxError> {
    let display_name = display_name_for(service_name);
    let label = plist_label(service_name);
    run_launchctl(&["start", &label])?;
    println!("服务 '{}' 已启动", display_name);
    Ok(())
}

/// 停止服务
pub fn stop_service(service_name: &str) -> Result<(), BoxError> {
    let display_name = display_name_for(service_name);
    let label = plist_label(service_name);
    run_launchctl(&["stop", &label])?;
    println!("服务 '{}' 已停止", display_name);
    Ok(())
}

/// 查询服务状态
pub fn query_service_status(service_name: &str) -> Result<(), BoxError> {
    let display_name = display_name_for(service_name);
    let label = plist_label(service_name);
    let output = std::process::Command::new("launchctl")
        .args(["list", &label])
        .output()
        .map_err(|e| format!("执行 launchctl 失败: {}", e))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        println!("服务: {} ({})", display_name, label);
        println!("{}", stdout);
    } else {
        println!("服务: {} ({})", display_name, label);
        println!("状态: 未加载或不存在");
    }

    Ok(())
}

/// 通过 launchd 重启服务
pub fn restart_via_service_manager() {
    // 使用非零退出码让 launchd 的 KeepAlive/SuccessfulExit 策略自动重启
    std::process::exit(42);
}

/// 执行 launchctl 命令
fn run_launchctl(args: &[&str]) -> Result<(), BoxError> {
    let output = std::process::Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|e| format!("执行 launchctl {:?} 失败: {}", args, e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "launchctl {:?} 失败 (exit {}): {}",
            args,
            output.status.code().unwrap_or(-1),
            stderr.trim()
        )
        .into());
    }

    Ok(())
}
