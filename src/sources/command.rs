//! Script-backed sources.
//!
//! `listen`: a program that runs for good and prints a value whenever
//! something changed.
//! `snapshot`: a program that prints the value once; it runs at start and
//! again on each trigger. The triggers are events; no trigger is a clock
//! unless the config says `timer:N`.

use super::Hub;
use crate::config::Source;
use crate::runner;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// A listener's process group: the script and whatever it piped together
/// (`swaymsg subscribe | while read`, `pactl subscribe`), all terminated
/// when the source goes (quit, a signal, or the script ending on its own).
struct Group(i32);

impl Drop for Group {
    fn drop(&mut self) {
        unsafe { libc::killpg(self.0, libc::SIGTERM) };
    }
}

pub async fn listen(src: Source, hub: Hub) {
    loop {
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&src.command)
            .env("S", hub.scripts())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{}: {e}", src.name);
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let _group = child.id().map(|pid| Group(pid as i32));
        let stdout = child.stdout.take().expect("piped stdout");
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            hub.emit(&src.name, runner::parse_output(&line));
        }
        let status = child.wait().await;
        eprintln!("{}: listener ended ({status:?}), restarting in 2s", src.name);
        drop(_group);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// A script run on each trigger; its output is the value.
pub async fn snapshot(src: Source, hub: Hub) {
    super::snapshot::run(src, hub, |src, hub| async move {
        let out = runner::capture(&src.command, hub.scripts()).await?;
        Ok(runner::parse_output(&out))
    })
    .await
}
