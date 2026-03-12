use clap::Parser;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(name = "tasknexus-updater")]
#[command(about = "TaskNexus updater helper", version)]
struct Cli {
    #[arg(long)]
    pid: u32,

    #[arg(long)]
    current_exe: PathBuf,

    #[arg(long)]
    new_exe: PathBuf,

    #[arg(long = "restart-arg")]
    restart_args: Vec<String>,

    // Backward compatibility for older agent versions that passed
    // `--config <path>` directly to updater.
    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long, default_value_t = 120)]
    wait_timeout_seconds: u64,
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let restart_args = build_restart_args(&cli);

    wait_for_process_exit(cli.pid, Duration::from_secs(cli.wait_timeout_seconds));
    replace_binary(&cli.current_exe, &cli.new_exe)?;
    restart_agent(&cli.current_exe, &restart_args)?;

    Ok(())
}

fn build_restart_args(cli: &Cli) -> Vec<String> {
    let mut args = cli.restart_args.clone();
    if args.is_empty() {
        if let Some(config_path) = &cli.config {
            args.push("--config".to_string());
            args.push(config_path.to_string_lossy().to_string());
        }
    }
    args
}

fn wait_for_process_exit(pid: u32, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !is_process_alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn replace_binary(current_exe: &Path, new_exe: &Path) -> Result<(), Box<dyn Error>> {
    let parent = current_exe
        .parent()
        .ok_or("Current executable has no parent directory")?;
    let file_name = current_exe
        .file_name()
        .ok_or("Current executable has no file name")?
        .to_string_lossy()
        .to_string();

    let staged = parent.join(format!("{}.new", file_name));
    let backup = parent.join(format!("{}.bak", file_name));

    if staged.exists() {
        let _ = std::fs::remove_file(&staged);
    }
    if backup.exists() {
        let _ = std::fs::remove_file(&backup);
    }

    std::fs::copy(new_exe, &staged)?;
    set_executable_if_needed(&staged)?;

    // Retry a little while to handle delayed process handle release on Windows.
    let mut renamed_old = false;
    for _ in 0..100 {
        if !current_exe.exists() {
            break;
        }
        match std::fs::rename(current_exe, &backup) {
            Ok(_) => {
                renamed_old = true;
                break;
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    if current_exe.exists() {
        return Err(format!("Failed to move old executable '{}'", current_exe.display()).into());
    }

    std::fs::rename(&staged, current_exe)?;
    if renamed_old {
        let _ = std::fs::remove_file(&backup);
    }
    Ok(())
}

fn restart_agent(executable: &Path, restart_args: &[String]) -> Result<(), Box<dyn Error>> {
    let mut command = std::process::Command::new(executable);
    command
        .args(restart_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    command.spawn()?;
    Ok(())
}

#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    let ret = unsafe { libc::kill(pid as i32, 0) };
    if ret == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(windows)]
fn is_process_alive(pid: u32) -> bool {
    let filter = format!("PID eq {}", pid);
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .output();

    match output {
        Ok(result) => {
            if !result.status.success() {
                return false;
            }
            let text = String::from_utf8_lossy(&result.stdout).to_lowercase();
            !text.contains("no tasks are running")
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn set_executable_if_needed(path: &Path) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable_if_needed(_path: &Path) -> Result<(), Box<dyn Error>> {
    Ok(())
}
