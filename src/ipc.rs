//! The control socket: `$XDG_RUNTIME_DIR/lintel.sock`, one request per
//! connection, a line in, a line out.
//!
//!   refresh NAME          run the command source NAME again (your own
//!                         action changed what it shows)
//!   update NAME VALUE     set a variable (VALUE is JSON, or a string)
//!   get NAME              print a variable's current value
//!   state                 every variable, as one JSON object
//!   pomodoro CMD          start | stop | pause | resume | toggle | skip | status
//!   timer DURATION [LABEL]  a countdown (7m, 90s, 1h); the label is its key;
//!                         `timer stop [LABEL]`, `timer status`
//!   pick NAME [PROMPT]    then the lines to choose from, one per line, up
//!                         to EOF: answers "INDEX<TAB>LINE", or nothing
//!   launch [TEXT]         the application launcher, TEXT already typed
//!   windows [TEXT]        the window switcher
//!   cancel                close the picker
//!   notifications CMD     close | close-all | action | pop | history | clear | toggle | status | list
//!   quit

use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

/// What arrives from the socket (and from the bar itself, for hovers).
#[derive(Debug)]
pub enum Request {
    /// `lintel refresh NAME`: an action of yours changed something.
    Refresh(String),
    /// The card of a module that reads NAME opened.
    Hover(String),
    Update(String, serde_json::Value),
    Get(String, oneshot::Sender<String>),
    /// Every variable at once, for `lintel mcp`.
    State(oneshot::Sender<String>),
    Pomodoro(String, oneshot::Sender<String>),
    Timer(Vec<String>, oneshot::Sender<String>),
    /// The mouse on a tray icon (from the bar itself).
    Tray(crate::sources::tray::TrayCommand),
    /// `lintel pick`: dmenu lines, the choice back as "INDEX\tLINE".
    Pick {
        name: String,
        prompt: String,
        lines: Vec<String>,
        reply: oneshot::Sender<String>,
    },
    /// The launcher and the switcher, with text already typed.
    Launch(String, oneshot::Sender<String>),
    Windows(String, oneshot::Sender<String>),
    /// Close the picker.
    Cancel,
    /// The mouse on a notification (from the bar itself).
    Notify(crate::sources::notifyd::NotifyEvent),
    /// `lintel notifications CMD`.
    Notifications(String, oneshot::Sender<String>),
    Quit,
}

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join("lintel.sock")
}

/// The client side: send one line, print the answer.
pub fn send(line: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    let mut s = std::os::unix::net::UnixStream::connect(socket_path()).context("lintel is not running (no socket)")?;
    s.write_all(line.as_bytes())?;
    s.write_all(b"\n")?;
    let mut reply = String::new();
    std::io::BufReader::new(s).read_line(&mut reply)?;
    Ok(reply.trim_end().to_string())
}

/// `pick`: the header line, then the lines up to EOF (the client shuts
/// its writing side), then the answer.
pub fn send_lines(header: &str, lines: &[String]) -> Result<String> {
    use std::io::{BufRead, Write};
    let mut s = std::os::unix::net::UnixStream::connect(socket_path()).context("lintel is not running (no socket)")?;
    s.write_all(header.as_bytes())?;
    s.write_all(b"\n")?;
    for l in lines {
        s.write_all(l.as_bytes())?;
        s.write_all(b"\n")?;
    }
    s.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    std::io::BufReader::new(s).read_line(&mut reply)?;
    Ok(reply.trim_end_matches('\n').to_string())
}

/// The server side: parse each line into a `Request`, answer with the
/// reply the handler gives (empty means "ok").
pub async fn serve(tx: mpsc::UnboundedSender<Request>) -> Result<()> {
    let path = socket_path();
    // a stale socket from a crashed bar: if nothing answers, take it over
    if path.exists() && std::os::unix::net::UnixStream::connect(&path).is_err() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    loop {
        let (stream, _) = listener.accept().await?;
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, tx).await {
                eprintln!("ipc: {e}");
            }
        });
    }
}

async fn handle(stream: UnixStream, tx: mpsc::UnboundedSender<Request>) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let reply = if line.starts_with("pick ") || line.trim() == "pick" {
        // the lines follow, to EOF
        let mut lines = Vec::new();
        let mut l = String::new();
        while reader.read_line(&mut l).await? > 0 {
            lines.push(l.trim_end_matches('\n').to_string());
            l.clear();
        }
        let mut words = line.trim().splitn(3, ' ');
        words.next();
        let name = words.next().unwrap_or("dmenu").to_string();
        let prompt = words.next().unwrap_or("").to_string();
        let (reply_tx, reply_rx) = oneshot::channel();
        if tx
            .send(Request::Pick {
                name,
                prompt,
                lines,
                reply: reply_tx,
            })
            .is_err()
        {
            "error: bar is shutting down".to_string()
        } else {
            reply_rx.await.unwrap_or_default()
        }
    } else {
        dispatch(line.trim(), &tx).await
    };
    w.write_all(reply.as_bytes()).await?;
    w.write_all(b"\n").await?;
    Ok(())
}

async fn dispatch(line: &str, tx: &mpsc::UnboundedSender<Request>) -> String {
    let mut words = line.splitn(3, ' ');
    let cmd = words.next().unwrap_or("");
    let a = words.next().unwrap_or("").to_string();
    let b = words.next().unwrap_or("").to_string();
    let (reply_tx, reply_rx) = oneshot::channel();
    let req = match cmd {
        "refresh" if !a.is_empty() => Request::Refresh(a),
        "update" if !a.is_empty() => {
            let value = serde_json::from_str(&b).unwrap_or(serde_json::Value::String(b));
            Request::Update(a, value)
        }
        "get" if !a.is_empty() => Request::Get(a, reply_tx),
        "state" => Request::State(reply_tx),
        "pomodoro" => Request::Pomodoro(format!("{a} {b}").trim().to_string(), reply_tx),
        "timer" => {
            let mut args = vec![a];
            if !b.is_empty() {
                args.push(b);
            }
            Request::Timer(args, reply_tx)
        }
        "launch" => Request::Launch(format!("{a} {b}").trim().to_string(), reply_tx),
        "windows" => Request::Windows(format!("{a} {b}").trim().to_string(), reply_tx),
        "cancel" => Request::Cancel,
        "notifications" => Request::Notifications(format!("{a} {b}").trim().to_string(), reply_tx),
        "quit" => Request::Quit,
        _ => return format!("error: unknown request {line:?}"),
    };
    let wants_reply = matches!(
        req,
        Request::Get(..)
            | Request::State(..)
            | Request::Pomodoro(..)
            | Request::Timer(..)
            | Request::Launch(..)
            | Request::Windows(..)
            | Request::Notifications(..)
    );
    if tx.send(req).is_err() {
        return "error: bar is shutting down".into();
    }
    if wants_reply {
        reply_rx.await.unwrap_or_else(|_| "error: no answer".into())
    } else {
        "ok".into()
    }
}
