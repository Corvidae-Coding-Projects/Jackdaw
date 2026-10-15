use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use browser_protocol::{CommandReply, CommandResult, EffectState, ErrorCode, ObservedItem};

const DEFAULT_TRUSTED_URLS: [&str; 2] = ["https://example.com/", "https://www.rust-lang.org/"];
const DEFAULT_REJECTED_URL: &str = "https://expired.badssl.com/";

struct DaemonGuard {
    child: Child,
    directory: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn temporary_directory() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory =
        std::env::temp_dir().join(format!("rwb-https-journey-{}-{nonce}", std::process::id()));
    fs::create_dir(&directory)?;
    Ok(directory)
}

fn run_rwb(
    rwb: &Path,
    socket: &Path,
    arguments: &[&str],
    expected_status: i32,
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
    if output.status.code() != Some(expected_status) {
        return Err(format!(
            "rwb {arguments:?} returned status {:?}, expected {expected_status}: stdout={} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    if !output.stderr.is_empty() {
        return Err(format!(
            "rwb {arguments:?} emitted unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn observation(reply: &CommandReply) -> Result<(&str, &str), Box<dyn std::error::Error>> {
    let Some(CommandResult::Observation(observation)) = reply.result.as_ref() else {
        return Err("observe returned the wrong result kind".into());
    };
    let text = observation
        .items
        .iter()
        .find_map(|item| match item {
            ObservedItem::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .ok_or("observation omitted semantic text")?;
    Ok((&observation.url, text))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let usage = "usage: https-journey BROWSERD RWB [TRUSTED_URL_1 TRUSTED_URL_2 REJECTED_URL]";
    let browserd = PathBuf::from(arguments.next().ok_or(usage)?);
    let rwb = PathBuf::from(arguments.next().ok_or(usage)?);
    let first_url = arguments
        .next()
        .unwrap_or_else(|| DEFAULT_TRUSTED_URLS[0].to_owned());
    let second_url = arguments
        .next()
        .unwrap_or_else(|| DEFAULT_TRUSTED_URLS[1].to_owned());
    let rejected_url = arguments
        .next()
        .unwrap_or_else(|| DEFAULT_REJECTED_URL.to_owned());
    if arguments.next().is_some() {
        return Err(usage.into());
    }
    for url in [&first_url, &second_url, &rejected_url] {
        if !url.starts_with("https://") {
            return Err(format!("HTTPS journey URL is not HTTPS: {url}").into());
        }
    }

    let directory = temporary_directory()?;
    let socket = directory.join("browser.sock");
    let profile_root = directory.join("profiles");
    let child = Command::new(&browserd)
        .arg("--socket")
        .arg(&socket)
        .arg("--profile-root")
        .arg(&profile_root)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("RWB_RENDERER", "cpu")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut daemon = DaemonGuard { child, directory };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        if let Some(status) = daemon.child.try_wait()? {
            return Err(format!("browserd exited before HTTPS readiness: {status}").into());
        }
        if Instant::now() >= deadline {
            return Err("browserd did not create its socket within ten seconds".into());
        }
        thread::sleep(Duration::from_millis(10));
    }

    let capabilities = run_rwb(&rwb, &socket, &["capabilities"], 0)?;
    let https_advertised = match capabilities.result.as_ref() {
        Some(CommandResult::Capabilities(manifest)) => !manifest
            .limitations
            .iter()
            .any(|limitation| limitation.capability == "https"),
        _ => return Err("capabilities returned the wrong result kind".into()),
    };
    if !https_advertised {
        return Err("capability manifest still reports HTTPS as unavailable".into());
    }

    let created = run_rwb(&rwb, &socket, &["tab", "new", &first_url], 0)?;
    let tab = created
        .target
        .as_ref()
        .and_then(|target| target.tab_id.as_ref())
        .ok_or("HTTPS tab creation omitted its tab ID")?
        .clone();
    let first_observed = run_rwb(&rwb, &socket, &["--tab", tab.as_str(), "observe"], 0)?;
    let (first_final_url, first_text) = observation(&first_observed)?;
    if !first_final_url.starts_with("https://") || first_text.trim().is_empty() {
        return Err(format!(
            "first HTTPS origin produced final URL {first_final_url:?} and {} semantic bytes",
            first_text.len()
        )
        .into());
    }

    let second_navigation = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab.as_str(), "open", &second_url],
        0,
    )?;
    let second_final_url = match second_navigation.result.as_ref() {
        Some(CommandResult::Navigation(navigation)) if navigation.url.starts_with("https://") => {
            navigation.url.clone()
        }
        _ => return Err("second HTTPS origin did not commit an HTTPS navigation".into()),
    };
    let second_observed = run_rwb(&rwb, &socket, &["--tab", tab.as_str(), "observe"], 0)?;
    let (observed_second_url, second_text) = observation(&second_observed)?;
    if observed_second_url != second_final_url || second_text.trim().is_empty() {
        return Err(
            "second HTTPS origin did not preserve its final URL and semantic content".into(),
        );
    }
    let retained_url = observed_second_url.to_owned();
    let retained_text = second_text.to_owned();

    let rejected = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab.as_str(), "open", &rejected_url],
        1,
    )?;
    let rejected_error = rejected
        .error
        .as_ref()
        .ok_or("certificate rejection omitted its protocol error")?;
    if rejected.success
        || rejected_error.code != ErrorCode::NetworkError
        || rejected_error.effect != EffectState::None
        || rejected.result.is_some()
    {
        return Err(format!(
            "certificate rejection returned success={} code={:?} effect={:?}",
            rejected.success, rejected_error.code, rejected_error.effect
        )
        .into());
    }

    let after_rejection = run_rwb(&rwb, &socket, &["--tab", tab.as_str(), "observe"], 0)?;
    let (after_url, after_text) = observation(&after_rejection)?;
    if after_url != retained_url || after_text != retained_text {
        return Err("failed TLS navigation replaced the committed HTTPS document".into());
    }

    println!(
        "https-journey: PASS fresh-processes=7 trusted-origins=2 first-final-url={} second-final-url={} certificate-rejection=NETWORK_ERROR/none committed-document=preserved https-advertised={} headless=true",
        first_final_url, second_final_url, https_advertised
    );
    Ok(())
}
