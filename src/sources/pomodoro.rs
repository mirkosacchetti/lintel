//! The pomodoro and the countdowns.
//!
//! A timer here is a deadline, not a ticker: the task sleeps until the
//! next instant the shown text changes (the next minute, or the next
//! second when the config shows seconds) or until a command arrives over
//! IPC. The phases: work, short break, long break every N pomodori, a
//! cheer at the end of a session.
//! The state is saved on every change and read back at start, so a bar
//! restart does not lose the session.
//!
//! The day's totals count the pomodori done and the time really worked:
//! the work phases' running time, pauses and suspends left out, a phase
//! skipped or stopped halfway counted for what it ran.

use super::notify::{self, Urgency};
use super::Hub;
use crate::config::{PomodoroConfig, Source};
use crate::runner;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

pub type Command = (String, oneshot::Sender<String>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Work,
    ShortBreak,
    LongBreak,
    Stopped,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Work => "work",
            Phase::ShortBreak => "short_break",
            Phase::LongBreak => "long_break",
            Phase::Stopped => "stopped",
        }
    }
    fn title(self) -> &'static str {
        match self {
            Phase::Work => "Work",
            Phase::ShortBreak => "Short break",
            Phase::LongBreak => "Long break",
            Phase::Stopped => "Stopped",
        }
    }
    fn minutes(self, cfg: &PomodoroConfig) -> u64 {
        match self {
            Phase::Work => cfg.work_minutes,
            Phase::ShortBreak => cfg.short_break_minutes,
            Phase::LongBreak => cfg.long_break_minutes,
            Phase::Stopped => 0,
        }
    }
}

/// The day's totals.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Today {
    /// The local date, "2026-10-09"; another date means another day.
    date: String,
    count: u32,
    worked_secs: u64,
}

/// What is saved: enough to come back after a restart.
#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    phase: Phase,
    paused: bool,
    remaining_secs: u64,
    count: u32,
    saved_at: u64,
    #[serde(default)]
    today: Today,
}

struct Session {
    phase: Phase,
    /// The remaining time while paused.
    paused: Option<Duration>,
    /// The end of the phase while running.
    end: Option<Instant>,
    count: u32,
    today: Today,
    /// Since when a work phase has been running, not yet in `today`.
    working_since: Option<Instant>,
}

fn local_date() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

impl Session {
    fn summary(&self, what: &str) -> String {
        format!("Pomodoro — {what}")
    }
}

impl Session {
    fn remaining(&self) -> Duration {
        match (self.paused, self.end) {
            (Some(d), _) => d,
            (None, Some(end)) => end.saturating_duration_since(Instant::now()),
            _ => Duration::ZERO,
        }
    }
    fn running(&self) -> bool {
        self.paused.is_none() && self.end.is_some()
    }

    /// A new day starts the totals again.
    fn roll_day(&mut self) {
        let date = local_date();
        if self.today.date != date {
            self.today = Today { date, ..Today::default() };
        }
    }

    /// The running work time into the day's total, before any change of
    /// phase or pause.
    fn flush(&mut self) {
        if let Some(since) = self.working_since.take() {
            self.roll_day();
            self.today.worked_secs += since.elapsed().as_secs();
        }
    }

    /// After a change: a work phase that runs starts counting.
    fn mark(&mut self) {
        if self.phase == Phase::Work && self.running() {
            self.working_since.get_or_insert_with(Instant::now);
        }
    }

    /// The day's worked time, the running stretch included.
    fn worked(&self) -> Duration {
        let base = if self.today.date == local_date() { self.today.worked_secs } else { 0 };
        Duration::from_secs(base) + self.working_since.map(|s| s.elapsed()).unwrap_or_default()
    }

    fn today_count(&self) -> u32 {
        if self.today.date == local_date() {
            self.today.count
        } else {
            0
        }
    }
}

fn state_path() -> PathBuf {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state"))
        .join("lintel");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("pomodoro.json")
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load() -> Session {
    let fallback = Session {
        phase: Phase::Stopped,
        paused: None,
        end: None,
        count: 0,
        today: Today::default(),
        working_since: None,
    };
    let Ok(text) = std::fs::read_to_string(state_path()) else {
        return fallback;
    };
    let Ok(s) = serde_json::from_str::<Saved>(&text) else {
        return fallback;
    };
    if s.phase == Phase::Stopped {
        return Session {
            count: s.count,
            today: s.today,
            ..fallback
        };
    }
    if s.paused {
        return Session {
            phase: s.phase,
            paused: Some(Duration::from_secs(s.remaining_secs)),
            end: None,
            count: s.count,
            today: s.today,
            working_since: None,
        };
    }
    // running: the wall clock went on while the bar was away, and a work
    // phase worked meanwhile, up to its end
    let elapsed = now_unix().saturating_sub(s.saved_at);
    let left = s.remaining_secs.saturating_sub(elapsed);
    let mut session = Session {
        phase: s.phase,
        paused: None,
        end: Some(Instant::now() + Duration::from_secs(left)),
        count: s.count,
        today: s.today,
        working_since: None,
    };
    if s.phase == Phase::Work {
        session.roll_day();
        session.today.worked_secs += elapsed.min(s.remaining_secs);
    }
    session.mark();
    session
}

fn save(s: &Session) {
    let saved = Saved {
        phase: s.phase,
        paused: s.paused.is_some(),
        remaining_secs: s.remaining().as_secs(),
        count: s.count,
        saved_at: now_unix(),
        today: Today {
            worked_secs: s.today.worked_secs + s.working_since.map(|w| w.elapsed().as_secs()).unwrap_or(0),
            ..s.today.clone()
        },
    };
    if let Ok(text) = serde_json::to_string_pretty(&saved) {
        if let Err(e) = std::fs::write(state_path(), text) {
            eprintln!("pomodoro: saving state: {e}");
        }
    }
}

/// "24m" or "24:37".
fn fmt_remaining(d: Duration, per_second: bool) -> String {
    let secs = d.as_secs() + if d.subsec_nanos() > 0 { 1 } else { 0 };
    if per_second {
        format!("{}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{}m", secs.div_ceil(60))
    }
}

/// The next instant the shown text changes, never in the past.
fn next_wake(end: Instant, per_second: bool) -> Instant {
    let now = Instant::now();
    let remaining = end.saturating_duration_since(now);
    if remaining.is_zero() {
        return now;
    }
    let step = if per_second { 1.0 } else { 60.0 };
    let shown = (remaining.as_secs_f64() / step).ceil();
    let wake = end - Duration::from_secs_f64((shown - 1.0) * step);
    wake.max(now + Duration::from_millis(1))
}

fn icon<'a>(cfg: &'a PomodoroConfig, key: &str) -> &'a str {
    cfg.icons.get(key).map(String::as_str).unwrap_or("")
}

/// "1h 18m", "52m".
fn fmt_worked(d: Duration) -> String {
    let m = d.as_secs() / 60;
    if m >= 60 {
        format!("{}h {:02}m", m / 60, m % 60)
    } else {
        format!("{m}m")
    }
}

/// "Today: 3 pomodori, 1h 18m worked", when there is anything to say.
fn today_line(s: &Session) -> Option<String> {
    let (count, worked) = (s.today_count(), s.worked());
    if count == 0 && worked.as_secs() < 60 {
        return None;
    }
    let pomodori = if count == 1 { "pomodoro" } else { "pomodori" };
    Some(format!("Today: {count} {pomodori}, {} worked", fmt_worked(worked)))
}

fn render(s: &Session, cfg: &PomodoroConfig, per_second: bool) -> serde_json::Value {
    let (text, info, class) = match s.phase {
        Phase::Stopped => (
            icon(cfg, "stopped").to_string(),
            "No session".to_string(),
            "stopped".to_string(),
        ),
        phase => {
            let left = fmt_remaining(s.remaining(), per_second);
            let (ic, class) = if s.paused.is_some() {
                (icon(cfg, "paused"), "paused")
            } else {
                (icon(cfg, phase.name()), phase.name())
            };
            let next = match phase {
                Phase::Work if (s.count + 1).is_multiple_of(cfg.pomodori_until_long) => "long break",
                Phase::Work => "short break",
                _ => "work",
            };
            let mut info = format!(
                "{}{}: {} left",
                phase.title(),
                if s.paused.is_some() { " (paused)" } else { "" },
                left
            );
            info += &format!("\nPomodori: {} of {}", s.count, cfg.pomodori_per_session);
            info += &format!("\nNext: {next}");
            (format!("{ic} {left}"), info, class.to_string())
        }
    };
    let info = match today_line(s) {
        Some(line) => format!("{info}\n{line}"),
        None => info,
    };
    json!({
        "text": text,
        "info": info,
        "class": class,
        "phase": s.phase.name(),
        "paused": s.paused.is_some(),
        "remaining_secs": s.remaining().as_secs(),
        "count": s.count,
        "state": if s.phase == Phase::Stopped { "stopped" } else if s.paused.is_some() { "paused" } else { "running" },
        "today_count": s.today_count(),
        "today_worked_secs": s.worked().as_secs(),
    })
}

fn begin(s: &mut Session, phase: Phase, cfg: &PomodoroConfig) {
    s.phase = phase;
    s.paused = None;
    s.end = Some(Instant::now() + Duration::from_secs(phase.minutes(cfg) * 60));
}

/// A new session: the first pomodoro.
fn start(s: &mut Session, cfg: &PomodoroConfig) {
    s.count = 0;
    begin(s, Phase::Work, cfg);
    notify::spawn(s.summary("start"), "Session started.".into(), Urgency::Normal);
}

/// The buttons on a phase notification: skip the phase, stop the session.
/// They send the pomodoro its own commands.
fn phase_actions(tx: &mpsc::UnboundedSender<Command>) -> (Vec<(String, String)>, Option<super::notifyd::ActionHandler>) {
    let tx = tx.clone();
    let handler: super::notifyd::ActionHandler = Arc::new(move |key: &str| {
        let (reply, _) = oneshot::channel();
        let _ = tx.send((key.to_string(), reply));
    });
    (vec![("skip".into(), "Skip".into()), ("stop".into(), "Stop".into())], Some(handler))
}

/// The phase ran out (or was skipped): count, tell, start the next one.
fn advance(s: &mut Session, cfg: &PomodoroConfig, scripts: &str, tx: &mpsc::UnboundedSender<Command>) {
    let tell = |summary: String, body: String, urgency: Urgency| {
        let (actions, handler) = phase_actions(tx);
        notify::spawn_actions(summary, body, urgency, notify::DEFAULT, actions, handler);
    };
    match s.phase {
        Phase::Work => {
            s.count += 1;
            s.roll_day();
            s.today.count += 1;
            if s.count.is_multiple_of(cfg.pomodori_per_session) {
                notify::spawn(
                    s.summary("session complete!"),
                    format!("{} pomodori done.", s.count),
                    Urgency::Critical,
                );
            }
            if s.count.is_multiple_of(cfg.pomodori_until_long) {
                begin(s, Phase::LongBreak, cfg);
                tell(
                    s.summary("long break"),
                    format!("Good work. {} minutes off.", cfg.long_break_minutes),
                    Urgency::Normal,
                );
            } else {
                begin(s, Phase::ShortBreak, cfg);
                tell(
                    s.summary("short break"),
                    format!("Pomodoro done. {} minutes off.", cfg.short_break_minutes),
                    Urgency::Normal,
                );
            }
        }
        Phase::ShortBreak | Phase::LongBreak => {
            begin(s, Phase::Work, cfg);
            tell(s.summary("work"), "Break over. Next pomodoro.".into(), Urgency::Normal);
        }
        Phase::Stopped => return,
    }
    runner::detached(&cfg.on_phase_end, scripts);
}

pub async fn run_pomodoro(src: Source, hub: Hub, mut rx: mpsc::UnboundedReceiver<Command>, tx: mpsc::UnboundedSender<Command>) {
    let cfg = hub.cfg.pomodoro.clone();
    let per_second = cfg.display == "seconds";
    let mut s = load();
    loop {
        hub.emit(&src.name, render(&s, &cfg, per_second));
        let wake = s.end.filter(|_| s.running()).map(|end| next_wake(end, per_second));
        tokio::select! {
            cmd = rx.recv() => {
                let Some((cmd, reply)) = cmd else { break };
                s.flush();
                let answer = match cmd.trim() {
                    "start" => {
                        start(&mut s, &cfg);
                        "ok"
                    }
                    "stop" => {
                        s.phase = Phase::Stopped;
                        s.paused = None;
                        s.end = None;
                        notify::spawn(s.summary("stopped"), "Session closed.".into(), Urgency::Normal);
                        "ok"
                    }
                    "pause" if s.running() => {
                        s.paused = Some(s.remaining());
                        s.end = None;
                        "ok"
                    }
                    "resume" if s.paused.is_some() => {
                        s.end = Some(Instant::now() + s.paused.take().unwrap());
                        "ok"
                    }
                    "toggle" => {
                        if s.phase == Phase::Stopped {
                            start(&mut s, &cfg);
                        } else if let Some(left) = s.paused.take() {
                            s.end = Some(Instant::now() + left);
                        } else {
                            s.paused = Some(s.remaining());
                            s.end = None;
                        }
                        "ok"
                    }
                    "skip" if s.phase != Phase::Stopped => {
                        advance(&mut s, &cfg, hub.scripts(), &tx);
                        "ok"
                    }
                    "status" => {
                        s.mark();
                        let _ = reply.send(render(&s, &cfg, per_second).to_string());
                        continue;
                    }
                    "pause" | "resume" | "skip" => "nothing to do",
                    _ => "error: start | stop | pause | resume | toggle | skip | status",
                };
                s.mark();
                save(&s);
                let _ = reply.send(answer.into());
            }
            _ = tokio::time::sleep_until(wake.unwrap_or_else(Instant::now).into()), if wake.is_some() => {
                if let Some(end) = s.end {
                    if Instant::now() >= end {
                        s.flush();
                        advance(&mut s, &cfg, hub.scripts(), &tx);
                        s.mark();
                        save(&s);
                    }
                }
            }
        }
    }
}

/// `lintel timer 7m [LABEL]`: countdowns, as many as you like, each with
/// a label that is its key (the same label again restarts it); `timer
/// stop [LABEL]` ends one, or all; the text shows the seconds. Emits
/// `NAME` = {items: [{label, text, remaining_secs}], count, text}.
pub async fn run_countdown(src: Source, hub: Hub, mut rx: mpsc::UnboundedReceiver<Command>, tx: mpsc::UnboundedSender<Command>) {
    const DEFAULT_LABEL: &str = "Countdown";
    let cfg = hub.cfg.pomodoro.clone();
    let mut timers: Vec<(String, Instant)> = Vec::new();
    // the length each one started with, for the Again button
    let mut lengths: HashMap<String, Duration> = HashMap::new();
    loop {
        let now = Instant::now();
        let items: Vec<serde_json::Value> = timers
            .iter()
            .map(|(label, end)| {
                let left = end.saturating_duration_since(now);
                json!({ "label": label, "text": fmt_remaining(left, true), "remaining_secs": left.as_secs() })
            })
            .collect();
        let text = items
            .iter()
            .map(|i| format!("{} {}", i["text"].as_str().unwrap_or(""), i["label"].as_str().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("  ");
        hub.emit(&src.name, json!({ "items": items, "count": timers.len(), "text": text }));

        let wake = timers.iter().map(|(_, end)| next_wake(*end, true)).min();
        tokio::select! {
            cmd = rx.recv() => {
                let Some((cmd, reply)) = cmd else { break };
                let cmd = cmd.trim();
                let (word, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
                let answer = match word {
                    "stop" => {
                        let label = rest.trim();
                        let stopped: Vec<(String, Instant)> = if label.is_empty() {
                            std::mem::take(&mut timers)
                        } else if let Some(i) = timers.iter().position(|(l, _)| l == label) {
                            vec![timers.remove(i)]
                        } else {
                            Vec::new()
                        };
                        if stopped.is_empty() && !label.is_empty() {
                            format!("error: no countdown {label:?}")
                        } else {
                            for (l, end) in stopped {
                                let left = fmt_remaining(end.saturating_duration_since(Instant::now()), true);
                                notify::spawn(l, format!("Countdown stopped, {left} left."), Urgency::Normal);
                            }
                            "ok".to_string()
                        }
                    }
                    "status" => {
                        if timers.is_empty() {
                            "no countdown".to_string()
                        } else {
                            timers
                                .iter()
                                .map(|(l, e)| format!("{l}: {} left", fmt_remaining(e.saturating_duration_since(Instant::now()), true)))
                                .collect::<Vec<_>>()
                                .join("; ")
                        }
                    }
                    spec => match parse_duration(spec) {
                        Some(d) => {
                            let label = if rest.trim().is_empty() { DEFAULT_LABEL.to_string() } else { rest.trim().to_string() };
                            let end = Instant::now() + d;
                            lengths.insert(label.clone(), d);
                            let restarted = match timers.iter_mut().find(|(l, _)| *l == label) {
                                Some(t) => {
                                    t.1 = end;
                                    true
                                }
                                None => {
                                    timers.push((label.clone(), end));
                                    false
                                }
                            };
                            let shown = fmt_remaining(d, true);
                            notify::spawn(
                                label.clone(),
                                format!("Countdown {}, {shown}.", if restarted { "restarted" } else { "started" }),
                                Urgency::Normal,
                            );
                            format!("{label}: {shown} started")
                        }
                        None => "error: timer DURATION [LABEL] (7m, 90s, 1h30m) | stop [LABEL] | status".to_string(),
                    },
                };
                let _ = reply.send(answer);
            }
            _ = tokio::time::sleep_until(wake.unwrap_or_else(Instant::now).into()), if wake.is_some() => {
                let now = Instant::now();
                let (done, running): (Vec<_>, Vec<_>) = timers.drain(..).partition(|(_, end)| now >= *end);
                timers = running;
                for (label, _) in done {
                    // stays until closed by hand; Again restarts the same length
                    let again = lengths.get(&label).copied().unwrap_or(Duration::from_secs(300));
                    let (tx, l) = (tx.clone(), label.clone());
                    let handler: super::notifyd::ActionHandler = Arc::new(move |_| {
                        let (reply, _) = oneshot::channel();
                        let _ = tx.send((format!("{}s {l}", again.as_secs()), reply));
                    });
                    notify::spawn_actions(label, "Time is up.".into(), Urgency::Critical, notify::STICKY, vec![("again".into(), "Again".into())], Some(handler));
                    runner::detached(&cfg.on_phase_end, hub.scripts());
                }
            }
        }
    }
}

/// "25m", "90s", "1h", "1h30m", or bare minutes.
fn parse_duration(spec: &str) -> Option<Duration> {
    let mut total = 0u64;
    let mut num = String::new();
    let mut any = false;
    for c in spec.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num.parse().ok()?;
        num.clear();
        total += match c {
            's' => n,
            'm' => n * 60,
            'h' => n * 3600,
            _ => return None,
        };
        any = true;
    }
    if !num.is_empty() {
        total += num.parse::<u64>().ok()? * 60;
        any = true;
    }
    (any && total > 0).then(|| Duration::from_secs(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("25m"), Some(Duration::from_secs(1500)));
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("10"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration("x"), None);
    }

    #[test]
    fn shown_minutes_round_up() {
        assert_eq!(fmt_remaining(Duration::from_secs(1500), false), "25m");
        assert_eq!(fmt_remaining(Duration::from_secs(1499), false), "25m");
        assert_eq!(fmt_remaining(Duration::from_secs(1440), false), "24m");
        assert_eq!(fmt_remaining(Duration::from_millis(1499500), true), "25:00");
    }

    #[test]
    fn wakes_at_the_next_change() {
        let end = Instant::now() + Duration::from_secs(1499);
        let wake = next_wake(end, false);
        // 25m shows until 24:00 remain: 59 seconds from now
        let d = wake.duration_since(Instant::now()).as_secs();
        assert!((58..=59).contains(&d), "{d}");
    }

    #[test]
    fn worked_time_leaves_pauses_out() {
        let mut s = Session {
            phase: Phase::Work,
            paused: None,
            end: Some(Instant::now() + Duration::from_secs(600)),
            count: 0,
            today: Today::default(),
            working_since: Some(Instant::now() - Duration::from_secs(90)),
        };
        // a pause: the 90 seconds go into the day
        s.flush();
        s.paused = Some(s.remaining());
        s.end = None;
        s.mark();
        assert_eq!(s.today.worked_secs, 90);
        assert!(s.working_since.is_none());
        // paused, nothing more counts
        assert_eq!(s.worked().as_secs(), 90);
        assert_eq!(today_line(&s).as_deref(), Some("Today: 0 pomodori, 1m worked"));
        // resumed: counting again
        s.flush();
        s.end = Some(Instant::now() + s.paused.take().unwrap());
        s.mark();
        assert!(s.working_since.is_some());
        // a break never counts
        s.flush();
        s.phase = Phase::ShortBreak;
        s.mark();
        assert!(s.working_since.is_none());
    }

    #[test]
    fn worked_reads_as_hours_and_minutes() {
        assert_eq!(fmt_worked(Duration::from_secs(52 * 60 + 30)), "52m");
        assert_eq!(fmt_worked(Duration::from_secs(78 * 60)), "1h 18m");
    }
}
