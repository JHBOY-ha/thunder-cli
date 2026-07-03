use anyhow::Result;
use daemonize::Daemonize;
use std::{
    fs::{File, Permissions},
    os::unix::prelude::PermissionsExt,
};
use std::{
    io::{self, BufRead},
    path::Path,
};

const PID_PATH: &str = "/var/run/thunder.pid";
const DEFAULT_STDOUT_PATH: &str = "/var/run/thunder.out";
const DEFAULT_STDERR_PATH: &str = "/var/run/thunder.err";
const DEFAULT_WORK_DIR: &str = "/";

/// Engine/launcher state files that must be removed on stop so a fresh start
/// doesn't inherit a dead instance's sockets or pid files.
const ENGINE_STATE_FILES: [&str; 4] = [
    crate::constant::PID_FILE,
    "/var/packages/pan-xunlei-com/target/var/pan-xunlei-com.pid.child",
    "/var/packages/pan-xunlei-com/target/var/pan-xunlei-com.sock",
    "/var/packages/pan-xunlei-com/target/var/pan-xunlei-com-launcher.sock",
];

/// Check if the user is root
pub fn check_root() {
    if !nix::unistd::Uid::effective().is_root() {
        println!("You must run this executable with root permissions");
        std::process::exit(-1)
    }
}

/// Whether a pid is a live process.
fn pid_alive(pid: i32) -> bool {
    // signal 0 performs error checking without actually sending a signal.
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// Get the pid of the daemon
pub fn get_pid() -> Option<i32> {
    if let Ok(data) = std::fs::read(PID_PATH) {
        let binding = String::from_utf8(data).ok()?;
        return binding.trim().parse().ok();
    }
    None
}

/// Kill any leftover launcher / core-engine processes and remove stale engine
/// state files. This guarantees a clean single instance: 迅雷 allows only one
/// online device per account, so a lingering launcher from a previous run
/// causes the new one to be kicked (UserKickout) and the panel to go blank.
fn cleanup_engine() {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_all();

    for (pid, proc_) in sys.processes() {
        // Match the launcher and the versioned core engine by their exe name.
        let name = proc_.name();
        let cmd = proc_.cmd().join(" ");
        let is_engine = name.contains("xunlei-pan-cli")
            || cmd.contains("xunlei-pan-cli-launcher")
            || cmd.contains("/xunlei-pan-cli.");
        if is_engine {
            let raw = pid.as_u32() as i32;
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(raw),
                nix::sys::signal::SIGKILL,
            );
        }
    }

    for f in ENGINE_STATE_FILES {
        let _ = std::fs::remove_file(f);
    }
}


/// Start the daemon
pub fn start() -> Result<()> {
    check_root();

    // If a pid file exists, only bail when that process is actually alive.
    // A stale pid file (previous crash / kill) would otherwise wedge start.
    if let Some(pid) = get_pid() {
        if pid_alive(pid) {
            println!("Thunder is already running with pid: {pid}");
            return Ok(());
        }
        println!("Removing stale pid file (pid {pid} not running)");
        let _ = std::fs::remove_file(PID_PATH);
    }

    // Guarantee a single instance: kill any orphaned launcher/engine left
    // over from a previous run (prevents 迅雷 UserKickout / blank panel).
    cleanup_engine();

    let pid_file = File::create(PID_PATH)?;
    pid_file.set_permissions(Permissions::from_mode(0o755))?;

    let stdout = File::create(DEFAULT_STDOUT_PATH)?;
    stdout.set_permissions(Permissions::from_mode(0o755))?;

    let stderr = File::create(DEFAULT_STDERR_PATH)?;
    stderr.set_permissions(Permissions::from_mode(0o755))?;

    let daemonize = Daemonize::new()
        .pid_file(PID_PATH) // Every method except `new` and `start`
        .chown_pid_file(true) // is optional, see `Daemonize` documentation
        .working_directory(DEFAULT_WORK_DIR) // for default behaviour.
        .umask(0o777) // Set umask, `0o027` by default.
        .stdout(stdout) // Redirect stdout to `/tmp/daemon.out`.
        .stderr(stderr) // Redirect stderr to `/tmp/daemon.err`.
        .privileged_action(|| "Executed before drop privileges");

    if let Some(err) = daemonize.start().err() {
        eprintln!("Error: {err}")
    }

    Ok(())
}

/// Stop the daemon
pub fn stop() -> Result<()> {
    use nix::sys::signal;
    use nix::unistd::Pid;

    check_root();

    if let Some(pid) = get_pid() {
        // Signal the daemon to shut down, waiting until it exits.
        for _ in 0..360 {
            if signal::kill(Pid::from_raw(pid), signal::SIGINT).is_err() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1))
        }
        let _ = std::fs::remove_file(PID_PATH);
    }

    // Always clean up: the launcher is spawned independently and is NOT killed
    // by signalling the daemon, so it (and the core engine) can linger as
    // orphans. Remove them plus stale sockets/pid files.
    cleanup_engine();

    Ok(())
}

/// Show the status of the daemon
pub fn status() -> Result<()> {
    use sysinfo::System;
    match get_pid() {
        Some(pid) => {
            let mut sys = System::new();

            // First we update all information of our `System` struct.
            sys.refresh_all();

            // Display processes ID
            let process = sys
                .processes()
                .into_iter()
                .find(|(raw_pid, _)| raw_pid.as_u32().eq(&(pid as u32)))
                .ok_or_else(|| anyhow::anyhow!("thunder is not running"))?;

            println!("{:<6} {:<6}  {:<6}", "PID", "CPU(%)", "MEM(MB)");
            println!(
                "{:<6}   {:<6.1}  {:<6.1}",
                process.0,
                process.1.cpu_usage(),
                (process.1.memory() as f64) / 1024.0 / 1024.0
            );
        }
        None => println!("thunder is not running"),
    }
    Ok(())
}

/// Show the log of the daemon
pub fn log() -> Result<()> {
    fn read_and_print_file(file_path: &Path, placeholder: &str) -> Result<()> {
        if !file_path.exists() {
            return Ok(());
        }

        // Check if the file is empty before opening it
        let metadata = std::fs::metadata(file_path)?;
        if metadata.len() == 0 {
            return Ok(());
        }

        let file = File::open(file_path)?;
        let reader = io::BufReader::new(file);
        let mut start = true;

        for line in reader.lines() {
            if let Ok(content) = line {
                if start {
                    start = false;
                    println!("{placeholder}");
                }
                println!("{}", content);
            } else if let Err(err) = line {
                eprintln!("Error reading line: {}", err);
            }
        }

        Ok(())
    }

    let stdout_path = Path::new(DEFAULT_STDOUT_PATH);
    read_and_print_file(stdout_path, "STDOUT>")?;

    let stderr_path = Path::new(DEFAULT_STDERR_PATH);
    read_and_print_file(stderr_path, "STDERR>")?;

    Ok(())
}
