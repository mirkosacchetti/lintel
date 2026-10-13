//! Zen: the bar out of sight and the notifications held back. Two hands
//! on the one switch: yours (`lintel zen on | off | toggle`, a key or a
//! switch in a card) and the pomodoro's, when `[pomodoro] zen = true`:
//! zen while a work phase runs, the bar back for the breaks, the pauses
//! and the end of the session. Yours outlasts the pomodoro's: switched
//! on by hand, zen stays through the breaks. Switched off by hand while
//! the pomodoro holds it, it stays off until work runs again.
//!
//! The state is the `zen` variable: `active`, and `manual` and `work`
//! for who holds it. The GTK side hides the bars on `active`; the
//! notification daemon keeps the notifications in the history, except
//! the bar's own (the pomodoro's, the countdowns'), which are the point.

use super::notifyd;
use super::Hub;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

/// A request line with its reply, or the pomodoro's word.
pub enum ZenCommand {
    Line(String, oneshot::Sender<String>),
    /// A work phase is running (true), or not any more.
    Work(bool),
}

#[derive(Default)]
struct Zen {
    manual: bool,
    work: bool,
    /// Switched off by hand while the pomodoro held it: until work runs
    /// again.
    suspended: bool,
}

impl Zen {
    fn active(&self) -> bool {
        self.manual || (self.work && !self.suspended)
    }

    fn render(&self) -> serde_json::Value {
        let active = self.active();
        let info = if self.manual {
            "On, by hand"
        } else if active {
            "On, for the pomodoro"
        } else if self.suspended {
            "Off until work runs again"
        } else {
            "Off"
        };
        json!({
            "active": active,
            "text": if active { "\u{f06c}" } else { "" },
            "info": info,
            "class": if active { "on" } else { "off" },
            "manual": self.manual,
            "work": self.work,
        })
    }
}

pub async fn run(hub: Hub, mut rx: mpsc::UnboundedReceiver<ZenCommand>, notifications: Option<mpsc::UnboundedSender<notifyd::Command>>) {
    let mut z = Zen::default();
    hub.emit("zen", z.render());
    let mut was = false;
    while let Some(cmd) = rx.recv().await {
        match cmd {
            ZenCommand::Work(on) => {
                if on && !z.work {
                    z.suspended = false;
                }
                z.work = on;
            }
            ZenCommand::Line(line, reply) => {
                let answer = match line.trim() {
                    "on" => {
                        z.manual = true;
                        z.suspended = false;
                        "on"
                    }
                    "off" => {
                        z.manual = false;
                        z.suspended = z.work;
                        "off"
                    }
                    "toggle" => {
                        if z.active() {
                            z.manual = false;
                            z.suspended = z.work;
                            "off"
                        } else {
                            z.manual = true;
                            z.suspended = false;
                            "on"
                        }
                    }
                    "status" => {
                        let _ = reply.send(z.render().to_string());
                        continue;
                    }
                    _ => "error: on | off | toggle | status",
                };
                let _ = reply.send(answer.into());
            }
        }
        hub.emit("zen", z.render());
        let now = z.active();
        if now != was {
            was = now;
            if let Some(tx) = &notifications {
                let (reply, _) = oneshot::channel();
                let _ = tx.send((format!("zen {}", if now { "on" } else { "off" }), reply));
            }
        }
    }
}
