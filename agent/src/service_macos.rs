//! macOS launchd 服务管理实现
//!
//! 安装为当前用户的图形会话代理，这样任务里的 docker login 可以写入登录钥匙串。

use std::path::Path;

use super::display_name_for;

type BoxError = Box<dyn std::error::Error>;

fn plist_label(service_name: &str) -> String {
    format!("com.tasknexus.{}", service_name)
}

fn legacy_daemon_plist_path(service_name: &str) -> String {
    format!("/Library/LaunchDaemons/com.tasknexus.{service_name}.plist")
}

fn agent_plist_path(home: &str, service_name: &str) -> String {
    format!("{home}/Library/LaunchAgents/com.tasknexus.{service_name}.plist")
}

fn log_path(home: &str, service_name: &str, ext: &str) -> String {
    format!("{home}/Library/Logs/{service_name}.{ext}")
}

fn acting_user() -> Result<String, BoxError> {
    let uid = command_stdout("id", &["-u"])?;
    if uid == "0" {
        let user = std::env::var("SUDO_USER").map_err(|_| {
            "请从普通用户执行 sudo，服务会进入该用户的登录会话".to_string()
        })?;
        if user.is_empty() || user == "root" {
            return Err("请从普通用户执行 sudo，服务不能以 root 运行".into());
        }
        return Ok(user);
    }
    command_stdout("id", &["-un"])
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
    command_stdout("id", &["-gn", user])
}

fn user_id(user: &str) -> Result<String, BoxError> {
    command_stdout("id", &["-u", user])
}

fn service_account() -> Result<(String, String, String, String), BoxError> {
    let user = acting_user()?;
    let home = user_home(&user)?;
    let group = primary_group(&user)?;
    let uid = user_id(&user)?;
    Ok((user, group, home, uid))
}

fn ensure_owned_dir(path: &str, user: &str, group: &str) -> Result<(), BoxError> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("创建目录 '{}' 失败: {}", path, error))?;
    let status = std::process::Command::new("chown")
        .args([format!("{user}:{group}"), path.to_string()])
        .status()
        .map_err(|error| format!("修改目录 '{}' 的所有者失败: {}", path, error))?;
    if !status.success() {
        return Err(format!(
            "修改目录 '{}' 的所有者失败 (exit {})",
            path,
            status.code().unwrap_or(-1)
        )
        .into());
    }
    Ok(())
}

fn command_stdout(program: &str, args: &[&str]) -> Result<String, BoxError> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("执行 {} {:?} 失败: {}", program, args, error))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "执行 {} {:?} 失败: {}",
            program,
            args,
            stderr.trim()
        )
        .into());
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return Err(format!("执行 {} {:?} 没有输出", program, args).into());
    }
    Ok(text)
}

fn gui_target(uid: &str, service_name: &str) -> String {
    format!("gui/{}/{}", uid, plist_label(service_name))
}

fn remove_legacy_daemon(service_name: &str) -> Result<(), BoxError> {
    let label = plist_label(service_name);
    let _ = run_launchctl(&["bootout", &format!("system/{label}")]);
    let plist = legacy_daemon_plist_path(service_name);
    if Path::new(&plist).exists() {
        std::fs::remove_file(&plist).map_err(|error| {
            format!("删除旧的系统服务文件 '{}' 失败: {}", plist, error)
        })?;
    }
    Ok(())
}

/// 安装 launchd 服务
pub fn install_service(service_name: &str, config_path: &Path) -> Result<(), BoxError> {
    let (user, group, home, uid) = service_account()?;
    ensure_owned_dir(&format!("{home}/Library/Logs"), &user, &group)?;
    ensure_owned_dir(&format!("{home}/Library/LaunchAgents"), &user, &group)?;
    let current_exe = std::env::current_exe()?;
    let exe_path = current_exe.to_string_lossy();
    let config_str = config_path.to_string_lossy();
    let display_name = display_name_for(service_name);
    let label = plist_label(service_name);
    let plist = agent_plist_path(&home, service_name);
    let stdout_log = log_path(&home, service_name, "log");
    let stderr_log = log_path(&home, service_name, "err");
    let domain = format!("gui/{uid}");
    let target = gui_target(&uid, service_name);

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

  <key>LimitLoadToSessionType</key>
  <string>Aqua</string>

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
        stdout_log = stdout_log,
        stderr_log = stderr_log,
    );

    std::fs::write(&plist, &plist_content).map_err(|error| {
        format!("写入 plist 文件 '{}' 失败: {}", plist, error)
    })?;
    let status = std::process::Command::new("chown")
        .args([format!("{user}:{group}"), plist.clone()])
        .status()
        .map_err(|error| format!("修改 plist 所有者失败: {}", error))?;
    if !status.success() {
        return Err(format!(
            "修改 plist 所有者失败 (exit {})",
            status.code().unwrap_or(-1)
        )
        .into());
    }

    remove_legacy_daemon(service_name)?;
    let _ = run_launchctl(&["bootout", &target]);
    run_launchctl(&["bootstrap", &domain, &plist])?;
    run_launchctl(&["enable", &target])?;

    let name_arg = if service_name == super::DEFAULT_SERVICE_NAME {
        String::new()
    } else {
        format!(" --name {}", service_name)
    };

    println!("服务 '{}' 安装成功", display_name);
    println!("  Plist 文件: {}", plist);
    println!("  配置文件:   {}", config_str);
    println!("  运行用户:   {}", user);
    println!("  启动类型:   用户登录后自动启动");
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
    let (_, _, home, uid) = service_account()?;
    let plist = agent_plist_path(&home, service_name);
    let _ = run_launchctl(&["bootout", &gui_target(&uid, service_name)]);
    if Path::new(&plist).exists() {
        std::fs::remove_file(&plist)
            .map_err(|error| format!("删除 plist 文件 '{}' 失败: {}", plist, error))?;
    }
    remove_legacy_daemon(service_name)?;

    println!("服务 '{}' 已卸载", display_name);
    Ok(())
}

/// 启动服务
pub fn start_service(service_name: &str) -> Result<(), BoxError> {
    let display_name = display_name_for(service_name);
    let (_, _, home, uid) = service_account()?;
    let target = gui_target(&uid, service_name);
    let plist = agent_plist_path(&home, service_name);
    if run_launchctl(&["print", &target]).is_ok() {
        run_launchctl(&["kickstart", "-k", &target])?;
    } else {
        run_launchctl(&["bootstrap", &format!("gui/{uid}"), &plist])?;
    }
    println!("服务 '{}' 已启动", display_name);
    Ok(())
}

/// 停止服务
pub fn stop_service(service_name: &str) -> Result<(), BoxError> {
    let display_name = display_name_for(service_name);
    let (_, _, _, uid) = service_account()?;
    run_launchctl(&["bootout", &gui_target(&uid, service_name)])?;
    println!("服务 '{}' 已停止", display_name);
    Ok(())
}

/// 查询服务状态
pub fn query_service_status(service_name: &str) -> Result<(), BoxError> {
    let display_name = display_name_for(service_name);
    let (_, _, _, uid) = service_account()?;
    let label = plist_label(service_name);
    let target = gui_target(&uid, service_name);
    let output = std::process::Command::new("launchctl")
        .args(["print", &target])
        .output()
        .map_err(|error| format!("执行 launchctl 失败: {}", error))?;

    println!("服务: {} ({})", display_name, label);
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().take(12) {
            println!("{}", line);
        }
    } else {
        println!("状态: 未加载或不存在");
    }

    Ok(())
}

/// 通过 launchd 重启服务
pub fn restart_via_service_manager() {
    std::process::exit(42);
}

fn run_launchctl(args: &[&str]) -> Result<(), BoxError> {
    let output = std::process::Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|error| format!("执行 launchctl {:?} 失败: {}", args, error))?;

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
