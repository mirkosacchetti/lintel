//! One connection to the sound server (PipeWire through its pulse
//! protocol), shared by the audio and the player sources. libpulse runs
//! its own threaded main loop; a thread of ours owns the context and runs
//! the queries the sources ask for, under the loop's lock; the server's
//! change events come out as a broadcast.

use anyhow::{anyhow, Context as _, Result};
use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::introspect::{SinkInfo, SinkInputInfo, SourceInfo};
use pulse::context::subscribe::{Facility, InterestMaskSet};
use pulse::context::{Context, FlagSet, State};
use pulse::mainloop::threaded::Mainloop;
use pulse::volume::Volume;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};

/// A sink or a source: the default output or input.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Device {
    pub name: String,
    pub description: String,
    pub headphones: bool,
    pub volume: u32,
    pub muted: bool,
}

/// A playback stream: an application's output.
#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    pub index: u32,
    pub binary: String,
    pub app_name: String,
    pub node_name: String,
    pub volume: u32,
    pub muted: bool,
    pub corked: bool,
}

/// What changed, as the server says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Sink,
    Source,
    SinkInput,
    Server,
    Card,
    /// The connection came (back).
    Connected,
}

type Job = Box<dyn FnOnce(&mut Context) + Send>;

pub struct Pulse {
    jobs: std_mpsc::Sender<Job>,
    pub events: broadcast::Sender<Change>,
}

impl Pulse {
    pub fn start() -> Arc<Pulse> {
        let (jobs, rx) = std_mpsc::channel::<Job>();
        let (events, _) = broadcast::channel(64);
        let ev = events.clone();
        std::thread::Builder::new()
            .name("pulse".into())
            .spawn(move || serve(rx, ev))
            .expect("pulse thread");
        Arc::new(Pulse { jobs, events })
    }

    /// Run `f` on the pulse thread, under the loop's lock; `f` starts a
    /// query whose callback answers on the channel it is given.
    async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Context, oneshot::Sender<T>) + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Box::new(move |ctx| f(ctx, tx)))
            .map_err(|_| anyhow!("pulse thread gone"))?;
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .context("pulse query timed out")?
            .context("pulse query dropped")
    }

    /// The default output and input.
    pub async fn defaults(&self) -> Result<(Device, Device)> {
        let (sink_name, source_name) = self
            .run(|ctx, tx| {
                let mut tx = Some(tx);
                ctx.introspect().get_server_info(move |info| {
                    if let Some(tx) = tx.take() {
                        let _ = tx.send((
                            info.default_sink_name.as_deref().unwrap_or("").to_string(),
                            info.default_source_name.as_deref().unwrap_or("").to_string(),
                        ));
                    }
                });
            })
            .await?;
        let sink = self
            .run(move |ctx, tx| {
                let mut tx = Some(tx);
                ctx.introspect().get_sink_info_by_name(&sink_name, move |r| match r {
                    ListResult::Item(i) => {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(Some(sink_device(i)));
                        }
                    }
                    _ => {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(None);
                        }
                    }
                });
            })
            .await?
            .unwrap_or_default();
        let source = self
            .run(move |ctx, tx| {
                let mut tx = Some(tx);
                ctx.introspect().get_source_info_by_name(&source_name, move |r| match r {
                    ListResult::Item(i) => {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(Some(source_device(i)));
                        }
                    }
                    _ => {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(None);
                        }
                    }
                });
            })
            .await?
            .unwrap_or_default();
        Ok((sink, source))
    }

    /// Every playback stream.
    pub async fn streams(&self) -> Result<Vec<Stream>> {
        self.run(|ctx, tx| {
            let mut tx = Some(tx);
            let mut list = Vec::new();
            ctx.introspect().get_sink_input_info_list(move |r| match r {
                ListResult::Item(i) => list.push(stream(i)),
                ListResult::End | ListResult::Error => {
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(std::mem::take(&mut list));
                    }
                }
            });
        })
        .await
    }
}

fn percent(v: &pulse::volume::ChannelVolumes) -> u32 {
    (v.avg().0 as f64 / Volume::NORMAL.0 as f64 * 100.0).round() as u32
}

fn headphones(form_factor: Option<String>, port: Option<&str>) -> bool {
    let ff = form_factor.unwrap_or_default().to_lowercase();
    let port = port.unwrap_or("").to_lowercase();
    ff == "headphone" || ff == "headset" || port.contains("headphone") || port.contains("headset")
}

fn sink_device(i: &SinkInfo) -> Device {
    Device {
        name: i.name.as_deref().unwrap_or("").to_string(),
        description: i.description.as_deref().unwrap_or("").to_string(),
        headphones: headphones(
            i.proplist.get_str("device.form_factor"),
            i.active_port.as_ref().and_then(|p| p.name.as_deref()),
        ),
        volume: percent(&i.volume),
        muted: i.mute,
    }
}

fn source_device(i: &SourceInfo) -> Device {
    Device {
        name: i.name.as_deref().unwrap_or("").to_string(),
        description: i.description.as_deref().unwrap_or("").to_string(),
        headphones: headphones(
            i.proplist.get_str("device.form_factor"),
            i.active_port.as_ref().and_then(|p| p.name.as_deref()),
        ),
        volume: percent(&i.volume),
        muted: i.mute,
    }
}

fn stream(i: &SinkInputInfo) -> Stream {
    Stream {
        index: i.index,
        binary: i.proplist.get_str("application.process.binary").unwrap_or_default(),
        app_name: i.proplist.get_str("application.name").unwrap_or_default(),
        node_name: i.proplist.get_str("node.name").unwrap_or_default(),
        volume: percent(&i.volume),
        muted: i.mute,
        corked: i.corked,
    }
}

/// The pulse thread: connect (again, when the server goes), then run the
/// jobs one by one under the lock.
fn serve(jobs: std_mpsc::Receiver<Job>, events: broadcast::Sender<Change>) {
    loop {
        let (mut ml, mut ctx) = match connect(&events) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("pulse: {e:#}; retrying in 5s");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        let _ = events.send(Change::Connected);
        for job in jobs.iter() {
            ml.lock();
            let state = ctx.get_state();
            if state != State::Ready {
                ml.unlock();
                eprintln!("pulse: connection lost ({state:?}); reconnecting");
                // the job is lost; the source retries on the Connected event
                break;
            }
            job(&mut ctx);
            ml.unlock();
        }
        ml.lock();
        ctx.disconnect();
        ml.unlock();
        ml.stop();
        if jobs.iter().peekable().peek().is_none() {
            return;
        }
    }
}

fn connect(events: &broadcast::Sender<Change>) -> Result<(Mainloop, Context)> {
    let mut ml = Mainloop::new().ok_or_else(|| anyhow!("creating the main loop"))?;
    let mut ctx = Context::new(&ml, "lintel").ok_or_else(|| anyhow!("creating the context"))?;
    // the state callback wakes the waiter below; the pointer outlives the
    // callback, which is removed before `ml` moves
    let ml_ptr: *mut Mainloop = &mut ml;
    ctx.set_state_callback(Some(Box::new(move || unsafe { (*ml_ptr).signal(false) })));
    ctx.connect(None, FlagSet::NOFLAGS, None).map_err(|e| anyhow!("connecting: {e}"))?;
    ml.lock();
    ml.start().map_err(|e| anyhow!("starting the main loop: {e}"))?;
    loop {
        match ctx.get_state() {
            State::Ready => break,
            State::Failed | State::Terminated => {
                ml.unlock();
                ml.stop();
                return Err(anyhow!("the server refused the connection"));
            }
            _ => ml.wait(),
        }
    }
    ctx.set_state_callback(None);
    let ev = events.clone();
    ctx.set_subscribe_callback(Some(Box::new(move |facility, _op, _idx| {
        let change = match facility {
            Some(Facility::Sink) => Change::Sink,
            Some(Facility::Source) => Change::Source,
            Some(Facility::SinkInput) => Change::SinkInput,
            Some(Facility::Server) => Change::Server,
            Some(Facility::Card) => Change::Card,
            _ => return,
        };
        let _ = ev.send(change);
    })));
    ctx.subscribe(
        InterestMaskSet::SINK | InterestMaskSet::SOURCE | InterestMaskSet::SINK_INPUT | InterestMaskSet::SERVER | InterestMaskSet::CARD,
        |_| {},
    );
    ml.unlock();
    Ok((ml, ctx))
}
