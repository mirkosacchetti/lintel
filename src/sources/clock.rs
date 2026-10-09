//! The clock: not a poll. A timerfd on CLOCK_REALTIME fires exactly when
//! the shown text changes next (the next minute when the format shows
//! minutes, the next second when it shows seconds), and is cancelled when
//! the system clock is set (TFD_TIMER_CANCEL_ON_SET), so a jump — resume
//! from suspend, an NTP step, a timezone change made in the format's
//! output — is seen at once. Between those instants nothing runs.

use super::Hub;
use crate::config::Source;
use anyhow::Result;
use chrono::Local;
use nix::sys::time::TimeSpec;
use nix::sys::timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags};
use serde_json::json;
use std::os::fd::{AsFd, AsRawFd};
use tokio::io::unix::AsyncFd;

pub async fn run(src: Source, hub: Hub) {
    if let Err(e) = tick_loop(&src, &hub).await {
        eprintln!("{}: {e:#}", src.name);
    }
}

async fn tick_loop(src: &Source, hub: &Hub) -> Result<()> {
    let format = if src.format.is_empty() { "%Y-%m-%d %H:%M" } else { &src.format };
    let info_format = if src.info_format.is_empty() {
        "%A %d %B %Y%nWeek %V, day %j"
    } else {
        &src.info_format
    };
    let per_second = shows_seconds(format);
    // months and days in the system's LC_TIME, like a localised program
    let locale = crate::locale::time();

    let timer = TimerFd::new(ClockId::CLOCK_REALTIME, TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC)?;
    // tokio wants a raw fd: a duplicate of the timer's, the timer stays for set()
    let fd = AsyncFd::new(timer.as_fd().try_clone_to_owned()?)?;
    loop {
        let now = Local::now();
        hub.emit(
            &src.name,
            json!({
                "text": now.format_localized(format, locale).to_string(),
                "info": now.format_localized(info_format, locale).to_string(),
                "epoch": now.timestamp(),
            }),
        );
        // the next instant the text changes: the next second or minute
        let step = if per_second { 1 } else { 60 };
        let next = now.timestamp() - (now.timestamp() % step) + step;
        timer.set(
            Expiration::OneShot(TimeSpec::new(next, 0)),
            TimerSetTimeFlags::TFD_TIMER_ABSTIME | TimerSetTimeFlags::TFD_TIMER_CANCEL_ON_SET,
        )?;
        let mut guard = fd.readable().await?;
        let mut buf = [0u8; 8];
        // ECANCELED: the clock was set, render now and re-arm
        let _ = nix::unistd::read(fd.get_ref().as_raw_fd(), &mut buf);
        guard.clear_ready();
    }
}

/// Does the strftime format show seconds (or anything finer)?
fn shows_seconds(format: &str) -> bool {
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some('S') | Some('T') | Some('s') | Some('f') | Some('r') | Some('X') | Some('c') | Some('+') => return true,
                Some('.') | Some('3') | Some('6') | Some('9') => return true,
                _ => {}
            }
        }
    }
    false
}
