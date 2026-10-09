//! Running shell commands: detached for the mouse (the bar never waits
//! for, nor kills, what a click started), captured for the snapshots.

use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::process::Stdio;

/// Run CMD through the compositor (`swaymsg exec`): the program gets the
/// session's own environment, Wayland and all, and lives under sway, not
/// under the bar's service, so a restart of the bar does not take it
/// down. `$S` is the scripts directory. Without sway, `sh -c` in a
/// systemd scope of its own, reaped by a thread.
pub fn detached(cmd: &str, scripts: &str) {
    if cmd.trim().is_empty() {
        return;
    }
    // the bar's PATH too: sway's own may lack ~/.local/bin, where lintel is
    let path = std::env::var("PATH").unwrap_or_default();
    // all one shell command: swaymsg splits on `;` into IPC commands, so
    // the variables come through an `export ... &&`, one shell command
    // that sway runs whole — an `export ...; cmd` here made sway run
    // `export` alone and reject `cmd` as an unknown sway command, and a
    // `S=.. cmd` prefix assignment would leave `$S` empty in the very
    // command it was set for (the shell expands before assigning)
    let script = format!(
        "export S='{}' PATH='{}' && {cmd}",
        scripts.replace('\'', "'\\''"),
        path.replace('\'', "'\\''"),
    );
    if let Some(sock) = crate::swaysock::path() {
        let ok = std::process::Command::new("swaymsg")
            .arg("-s")
            .arg(sock)
            .args(["exec", "--", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return;
        }
    }
    // without sway: a scope of its own under the user manager, so the
    // program is not in the bar's cgroup, which a restart of the bar's
    // service would kill whole
    let mut c = std::process::Command::new("systemd-run");
    c.args(["--user", "--scope", "--quiet", "--collect", "sh", "-c"])
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .process_group(0);
    match c.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("run {cmd:?}: {e}"),
    }
}

/// `sh -c CMD`, its stdout as a string (trailing newline dropped).
pub async fn capture(cmd: &str, scripts: &str) -> Result<String> {
    let out = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("S", scripts)
        .envs(crate::swaysock::path().map(|p| ("SWAYSOCK", p)))
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {cmd:?}"))?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    while s.ends_with('\n') {
        s.pop();
    }
    Ok(s)
}

/// Output text as a value: JSON when it parses, else the string.
pub fn parse_output(s: &str) -> serde_json::Value {
    let t = s.trim();
    if t.starts_with('{') || t.starts_with('[') || t.starts_with('"') {
        if let Ok(v) = serde_json::from_str(t) {
            return v;
        }
    }
    serde_json::Value::String(s.to_string())
}
