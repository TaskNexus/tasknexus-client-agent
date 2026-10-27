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

fn log_path(home: &str, service_name: &str, ext: &str) -> String {
    format!("{}/Library/Logs/{}.{}", home, service_name, ext)
}

fn install_account() -> Result<(String, String, String), BoxError> {
    let user = std::env::var("SUDO_USER").map_err(|_| {
        "请使用 sudo 安装，服务会以执行 sudo 的用户身份运行".to_string()
    })?;
    if user.is_empty() || user == "root" {
        return Err("请从普通用户执行 sudo 安装，服务不能以 root 运行".into());
    }

    let home = user_home(&user)?;
    let group = primary_group(&user)?;
    let log_dir = format!("{home}/Library/Logs");
    std::fs::create_dir_all(&log_dir)
        .map_err(|error| format!("创建日志目录 '{}' 失败: {}", log_dir, error))?;
    let status = std::process::Command::new("chown")
        .args([format!("{user}:{group}"), log_dir])
        .status()
        .map_err(|error| format!("修改日志目录所有者失败: {}", error))?;
    if !status.success() {
        return Err(format!(
            "修改日志目录所有者失败 (exit {})",
            status.code().unwrap_or(-1)
        )
        .into());
    }

    Ok((user, group, home))
}

fn user_home(user: &str) -> Result<String, BoxError> {
    let output = std::process::Command::new("dscl")
        .args([".", "-read", &format!("/Users/{user}"), "NFSHomeDirectory"])
        .output()
        .map_err(|error| format!("读取用户 {} 的主目录失败: {}", user, error))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("读取用户 {} 的主目录失败: {}", user, stderr.trim()).into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("NFSHomeDirectory:"))
        .map(str::trim)
        .filter(|home| !home.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("用户 {} 没有主目录", user).into())
}

fn primary_group(user: &str) -> Result<String, BoxError> {
    let output = std::process::Command::new("id")
        .args(["-gn", user])
        .output()
        .map_err(|error| format!("读取用户 {} 的组失败: {}", user, error))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("读取用户 {} 的组失败: {}", user, stderr.trim()).into());
    }

    let group = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if group.is_empty() {
        return Err(format!("用户 {} 没有主组", user).into());
    }
    Ok(group)
}

/// 安装 launchd 服务
pub fn install_service(service_name: &str, config_path: &Path) -> Result<(), BoxError> {
    let (user, group, home) = install_account()?;
    let current_exe = std::env::current_exe()?;
    let exe_path = current_exe.to_string_lossy();
    let config_str = config_path.to_string_lossy();
    let display_name = display_name_for(service_name);
    let label = plist_label(service_name);
    let plist = plist_path(service_name);
    let stdout_log = log_path(&home, service_name, "log");
    let stderr_log = log_path(&home, service_name, "err");

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

  <key>UserName</key>
  <string>{user}</string>

  <key>GroupName</key>
  <string>{group}</string>

  <key>InitGroups</key>
  <true/>

  <key>StandardOutPath</key>
  <string>{stdout_log}</string>

  <key>StandardErrorPath</key>
  <string>{stderr_log}</string>

  <key>SoftResourceLimits</key>
  <dict>
    <key>NumberOfFiles</key>
    <integer>10240</integer>
  </dict>
  <key>HardResourceLimits</key>
  <dict>
    <key>NumberOfFiles</key>
    <integer>10240</integer>
  </dict>
</dict>
</plist>
"#,
        label = label,
        exe = exe_path,
        config = config_str,
        svc_name = service_name,
        user = user,
        group = group,
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
    println!("  运行用户:   {}", user);
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
