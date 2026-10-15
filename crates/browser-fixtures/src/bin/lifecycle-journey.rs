use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use browser_protocol::{CommandReply, CommandResult};

struct LifecycleGuard {
    rwb: PathBuf,
    socket: PathBuf,
    directory: PathBuf,
}

impl Drop for LifecycleGuard {
    fn drop(&mut self) {
        let _ = Command::new(&self.rwb)
            .arg("--socket")
            .arg(&self.socket)
            .arg("--json")
            .args(["daemon", "stop"])
            .output();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn temporary_directory() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "rwb-lifecycle-journey-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&directory)?;
    Ok(directory)
}

fn parse_reply(
    action: &str,
    output: std::process::Output,
) -> Result<CommandReply, Box<dyn std::error::Error>> {
    if !output.status.success() {
        return Err(format!(
            "{action} exited with {:?}: stdout={} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    if !output.stderr.is_empty() {
        return Err(format!(
            "{action} emitted unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let reply: CommandReply = serde_json::from_slice(&output.stdout)?;
    if !reply.success {
        return Err(format!("{action} returned an unsuccessful protocol reply: {reply:?}").into());
    }
    Ok(reply)
}

fn run_request(
    rwb: &Path,
    socket: &Path,
    arguments: &[&str],
) -> Result<CommandReply, Box<dyn std::error::Error>> {
    let output = Command::new(rwb)
        .arg("--socket")
        .arg(socket)
        .arg("--json")
        .args(arguments)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("RWB_RENDERER", "cpu")
        .output()?;
    parse_reply(&format!("rwb {arguments:?}"), output)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let usage = "usage: lifecycle-journey BROWSERD RWB";
    let browserd = PathBuf::from(arguments.next().ok_or(usage)?);
    let rwb = PathBuf::from(arguments.next().ok_or(usage)?);
    if arguments.next().is_some() {
        return Err(usage.into());
    }

    let directory = temporary_directory()?;
    let socket = directory.join("browser.sock");
    let profile_root = directory.join("profiles");
    let _guard = LifecycleGuard {
        rwb: rwb.clone(),
        socket: socket.clone(),
        directory,
    };

    let start_output = Command::new(&rwb)
        .arg("--socket")
        .arg(&socket)
        .arg("--json")
        .args(["daemon", "start", "--profile-root"])
        .arg(&profile_root)
        .arg("--browserd")
        .arg(&browserd)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("RWB_RENDERER", "cpu")
        .output()?;
    let started = parse_reply("rwb daemon start", start_output)?;
    let started_instance = match started.result.as_ref() {
        Some(CommandResult::Status(status)) if status.running => status.service_instance_id.clone(),
        _ => return Err("daemon start did not return a running status".into()),
    };

    let status = run_request(&rwb, &socket, &["daemon", "status"])?;
    let status_instance = match status.result.as_ref() {
        Some(CommandResult::Status(status)) if status.running => status.service_instance_id.clone(),
        _ => return Err("fresh-process daemon status did not report running".into()),
    };
    if status_instance != started_instance {
        return Err("daemon start and status observed different service instances".into());
    }

    let version = run_request(&rwb, &socket, &["version"])?;
    if !matches!(version.result, Some(CommandResult::Version(_))) {
        return Err("fresh-process version request returned the wrong result kind".into());
    }

    let stopped = run_request(&rwb, &socket, &["daemon", "stop"])?;
    if !matches!(stopped.result, Some(CommandResult::Empty)) {
        return Err("daemon stop returned the wrong result kind".into());
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() {
        if Instant::now() >= deadline {
            return Err("daemon socket remained after successful stop".into());
        }
        thread::sleep(Duration::from_millis(10));
    }

    let after_stop = Command::new(&rwb)
        .arg("--socket")
        .arg(&socket)
        .arg("--json")
        .args(["daemon", "status"])
        .output()?;
    if after_stop.status.success() {
        return Err("daemon status unexpectedly succeeded after stop".into());
    }

    println!(
        "lifecycle-journey: PASS fresh-processes=5 start=running status=same-instance version=ok stop=socket-removed post-stop=unreachable headless=true"
    );
    Ok(())
}
