//! macOS launchd 服务管理实现
//!
//! 通过生成 plist 文件实现服务的安装、卸载、启停和状态查询。

use std::path::Path;

use super::SERVICE_DISPLAY_NAME;

const PLIST_LABEL: &str = "com.tasknexus.agent";
const PLIST_PATH: &str = "/Library/LaunchDaemons/com.tasknexus.agent.plist";

type BoxError = Box<dyn std::error::Error>;

/// 安装 launchd 服务
pub fn install_service(config_path: &Path) -> Result<(), BoxError> {
    let current_exe = std::env::current_exe()?;
    let exe_path = current_exe.to_string_lossy();
    let config_str = config_path.to_string_lossy();

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
  <string>/var/log/tasknexus-agent.log</string>

  <key>StandardErrorPath</key>
  <string>/var/log/tasknexus-agent.err</string>
</dict>
</plist>
"#,
        label = PLIST_LABEL,
        exe = exe_path,
        config = config_str,
    );

    std::fs::write(PLIST_PATH, &plist_content).map_err(|e| {
        format!(
            "写入 plist 文件 '{}' 失败（可能需要 sudo 权限）: {}",
            PLIST_PATH, e
        )
    })?;

    // 加载服务
    run_launchctl(&["load", "-w", PLIST_PATH])?;

    println!("服务 '{}' 安装成功", SERVICE_DISPLAY_NAME);
    println!("  Plist 文件: {}", PLIST_PATH);
    println!("  配置文件:   {}", config_str);
    println!("  启动类型:   开机自动启动");
    println!();
    println!("使用以下命令管理服务:");
    println!("  启动: sudo tasknexus-agent service start");
    println!("  停止: sudo tasknexus-agent service stop");
    println!("  状态: tasknexus-agent service status");
    println!("  卸载: sudo tasknexus-agent service uninstall");

    Ok(())
}

/// 卸载 launchd 服务
pub fn uninstall_service() -> Result<(), BoxError> {
    if std::path::Path::new(PLIST_PATH).exists() {
        // 先卸载服务
        let _ = run_launchctl(&["unload", PLIST_PATH]);

        // 删除 plist 文件
        std::fs::remove_file(PLIST_PATH).map_err(|e| {
            format!(
                "删除 plist 文件 '{}' 失败（可能需要 sudo 权限）: {}",
                PLIST_PATH, e
            )
        })?;
    }

    println!("服务 '{}' 已卸载", SERVICE_DISPLAY_NAME);
    Ok(())
}

/// 启动服务
pub fn start_service() -> Result<(), BoxError> {
    run_launchctl(&["start", PLIST_LABEL])?;
    println!("服务 '{}' 已启动", SERVICE_DISPLAY_NAME);
    Ok(())
}

/// 停止服务
pub fn stop_service() -> Result<(), BoxError> {
    run_launchctl(&["stop", PLIST_LABEL])?;
    println!("服务 '{}' 已停止", SERVICE_DISPLAY_NAME);
    Ok(())
}

/// 查询服务状态
pub fn query_service_status() -> Result<(), BoxError> {
    let output = std::process::Command::new("launchctl")
        .args(["list", PLIST_LABEL])
        .output()
        .map_err(|e| format!("执行 launchctl 失败: {}", e))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        println!("服务: {}", SERVICE_DISPLAY_NAME);
        println!("{}", stdout);
    } else {
        println!("服务: {}", SERVICE_DISPLAY_NAME);
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
