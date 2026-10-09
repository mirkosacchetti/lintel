//! The triggers: event streams of the system that tell a snapshot source
//! to run again. Each one is a task that sends a tag on the channel
//! whenever its event fires. None of them polls:
//!
//!   udev:SUBSYSTEM       the kernel's uevents (netlink), e.g. power_supply
//!                        when the battery reports, backlight when the
//!                        panel's brightness is written
//!   dbus:BUS:NAME:PATH   signals a daemon broadcasts: PropertiesChanged
//!                        of bluez, iwd, power-profiles-daemon, systemd
//!                        units ...
//!   netlink              rtnetlink: links, addresses and routes
//!   sway:EV[,EV]         sway IPC events (output, tick, workspace ...)
//!   timer:SECONDS        a plain interval, for whoever wants polling

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use std::os::fd::AsRawFd;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;

type Tick = UnboundedSender<&'static str>;

pub fn spawn(spec: &str, tx: Tick) -> Result<()> {
    let (kind, arg) = spec.split_once(':').unwrap_or((spec, ""));
    let arg = arg.to_string();
    match kind {
        "udev" => {
            std::thread::Builder::new()
                .name(format!("udev:{arg}"))
                .spawn(move || udev_thread(arg, tx))?;
        }
        "dbus" => {
            tokio::spawn(retrying("dbus", move || dbus(arg.clone(), tx.clone())));
        }
        "netlink" => {
            tokio::spawn(retrying("netlink", move || netlink(tx.clone())));
        }
        "sway" => {
            tokio::spawn(retrying("sway", move || sway(arg.clone(), tx.clone())));
        }
        "timer" => {
            let secs: u64 = arg.parse().context("timer:SECONDS")?;
            tokio::spawn(async move {
                let mut i = tokio::time::interval(Duration::from_secs(secs.max(1)));
                i.tick().await;
                loop {
                    i.tick().await;
                    if tx.send("timer").is_err() {
                        break;
                    }
                }
            });
        }
        _ => bail!("unknown trigger kind {kind:?}"),
    }
    Ok(())
}

/// Run a trigger loop again after it fails (a daemon restarted, sway
/// reloaded), a few seconds later.
async fn retrying<F, Fut>(what: &'static str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    loop {
        match f().await {
            Ok(()) => break,
            Err(e) => eprintln!("trigger {what}: {e:#}; retrying in 5s"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// libudev's monitor is not Send, so it lives on a thread of its own,
/// blocked in poll() between events.
fn udev_thread(subsystem: String, tx: Tick) {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::AsFd;
    loop {
        let socket = udev::MonitorBuilder::new()
            .and_then(|b| b.match_subsystem(&subsystem))
            .and_then(|b| b.listen());
        let socket = match socket {
            Ok(s) => s,
            Err(e) => {
                eprintln!("trigger udev:{subsystem}: {e}; retrying in 5s");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        loop {
            let mut fds = [PollFd::new(socket.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => {
                    eprintln!("trigger udev:{subsystem}: poll: {e}");
                    break;
                }
            }
            let n = socket.iter().count();
            if n > 0 && tx.send("udev").is_err() {
                return;
            }
        }
    }
}

/// `BUS:NAME:PATH`: every signal NAME sends from PATH or below. For
/// systemd the manager must be asked to emit unit signals (Subscribe).
async fn dbus(arg: String, tx: Tick) -> Result<()> {
    let mut parts = arg.splitn(3, ':');
    let bus = parts.next().unwrap_or("");
    let name = parts.next().ok_or_else(|| anyhow!("dbus:BUS:NAME:PATH"))?.to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let conn = match bus {
        "session" => zbus::Connection::session().await?,
        "system" => zbus::Connection::system().await?,
        other => bail!("bus must be session or system, not {other:?}"),
    };
    if name == "org.freedesktop.systemd1" {
        conn.call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "Subscribe",
            &(),
        )
        .await
        .context("systemd Subscribe")?;
    }
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(name.as_str())?
        .path_namespace(path.as_str())?
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, &conn, Some(64)).await?;
    while let Some(msg) = stream.next().await {
        msg?;
        if tx.send("dbus").is_err() {
            return Ok(());
        }
    }
    bail!("signal stream ended")
}

/// rtnetlink multicast groups: link, IPv4/IPv6 address, IPv4/IPv6 route.
async fn netlink(tx: Tick) -> Result<()> {
    use nix::sys::socket::{bind, recv, socket, AddressFamily, MsgFlags, NetlinkAddr, SockFlag, SockProtocol, SockType};
    const RTNLGRP_LINK: u32 = 1;
    const RTNLGRP_IPV4_IFADDR: u32 = 5;
    const RTNLGRP_IPV4_ROUTE: u32 = 7;
    const RTNLGRP_IPV6_IFADDR: u32 = 9;
    const RTNLGRP_IPV6_ROUTE: u32 = 11;
    let groups = [
        RTNLGRP_LINK,
        RTNLGRP_IPV4_IFADDR,
        RTNLGRP_IPV4_ROUTE,
        RTNLGRP_IPV6_IFADDR,
        RTNLGRP_IPV6_ROUTE,
    ]
    .iter()
    .fold(0u32, |m, g| m | (1 << (g - 1)));
    let sock = socket(
        AddressFamily::Netlink,
        SockType::Raw,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        SockProtocol::NetlinkRoute,
    )?;
    bind(sock.as_raw_fd(), &NetlinkAddr::new(0, groups))?;
    let fd = AsyncFd::new(sock)?;
    let mut buf = vec![0u8; 65536];
    loop {
        let mut guard = fd.readable().await?;
        let mut got = false;
        loop {
            match recv(fd.get_ref().as_raw_fd(), &mut buf, MsgFlags::MSG_DONTWAIT) {
                Ok(0) => break,
                Ok(_) => got = true,
                Err(nix::errno::Errno::EAGAIN) => break,
                Err(nix::errno::Errno::ENOBUFS) => {
                    got = true;
                    break;
                }
                Err(e) => return Err(e.into()),
            }
        }
        guard.clear_ready();
        if got && tx.send("netlink").is_err() {
            return Ok(());
        }
    }
}

async fn sway(events: String, tx: Tick) -> Result<()> {
    use swayipc_async::EventType;
    let mut types = Vec::new();
    for e in events.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        types.push(match e {
            "workspace" => EventType::Workspace,
            "output" => EventType::Output,
            "mode" => EventType::Mode,
            "window" => EventType::Window,
            "binding" => EventType::Binding,
            "tick" => EventType::Tick,
            "shutdown" => EventType::Shutdown,
            other => bail!("unknown sway event {other:?}"),
        });
    }
    if types.is_empty() {
        bail!("sway:EVENT[,EVENT]");
    }
    let conn = swayipc_async::Connection::new().await?;
    let mut stream = conn.subscribe(&types).await?;
    while let Some(ev) = stream.next().await {
        ev?;
        if tx.send("sway").is_err() {
            return Ok(());
        }
    }
    bail!("event stream ended")
}
