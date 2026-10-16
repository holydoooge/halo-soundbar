//! Device session over the EC protocol.
//!
//! The Halo Soundbar is a `SOUND_BOX`/`'EDIFIER'` device, so `ECDeviceFactory`
//! owns it and everything travels as raw byte payloads on the EC command set —
//! not the chunked JSON "home" protocol.
//!
//! One thread owns the HID handle. It performs the initial state read, services
//! commands from the UI, refreshes the visible state automatically, and — most
//! importantly — re-enumerates and reconnects on its own after the device
//! reboots (factory reset, power cycle, unplug/replug). The user never has to
//! ask for a refresh.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::ec;
use crate::ec_state::{self, EcState};
use crate::hid::{find_control_collection, Collection, HidDevice, WriteStrategy};
use crate::protocol::{DeviceHeader, Response};
use crate::transport::{EcTransport, Link};

/// Something the session thread should do.
#[derive(Debug, Clone)]
pub enum Command {
    /// Re-read the whole device state.
    Refresh,
    /// Set the volume (`0x67`).
    SetVolume(u8),
    /// Select an equaliser scene (`0xC4` with `[index]`).
    SetEq(u8),
    /// Apply one mood-lighting effect (`0x6B`).
    SetEffect {
        array_index: u8,
        index: u8,
        rgb: (u8, u8, u8),
        brightness: u8,
        speed: u8,
    },
    /// Restore an effect to its default (`0x72`).
    ResetEffect { array_index: u8, index: u8 },
    /// Mood-light master switch (`0x6B` with effect index 8).
    SetLightSwitch { array_index: u8, on: bool },
    /// Factory reset (`0x07`).
    FactoryReset,
    /// Power the device off (`0xCE`).
    Shutdown,
}

/// Live view of the connection, shared with the UI.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub connected: bool,
    pub collection: Option<String>,
    pub usage_page: u16,
    pub output_len: u16,
    pub input_len: u16,
    pub strategy: String,
    pub device_product: String,
    pub device_serial: String,
    pub state: EcState,
    pub header: Option<DeviceHeader>,
    pub last_error: Option<String>,
    pub last_reply: Option<String>,
    pub frames: u64,
    pub greeting: Vec<String>,
    pub log: Vec<String>,
    /// Human-readable transcript of the EC exchanges.
    pub transcript: Vec<String>,
}

/// Events pushed from the session thread to the UI.
#[derive(Debug, Clone)]
pub enum Event {
    Connected {
        collection: Box<Collection>,
        strategy: WriteStrategy,
    },
    State(Box<EcState>),
    Raw(String),
    Error(String),
    Disconnected,
}

/// Owns the HID link and the session thread.
pub struct Session {
    events: Receiver<Event>,
    jobs: Sender<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    pub fn start() -> Self {
        let (event_tx, events) = mpsc::channel();
        let (job_tx, jobs) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let snapshot_for_thread = snapshot.clone();

        let thread = std::thread::Builder::new()
            .name("halo-session".into())
            .spawn(move || run(event_tx, jobs, snapshot_for_thread))
            .ok();

        Self {
            events,
            jobs: job_tx,
            snapshot,
            thread,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn poll_events(&self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            out.push(event);
        }
        out
    }

    /// Ask the device for its full state.
    pub fn refresh(&self) {
        let _ = self.jobs.send(Command::Refresh);
    }

    pub fn send(&self, command: Command) {
        let _ = self.jobs.send(command);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let (dead, _) = mpsc::channel();
        let _ = std::mem::replace(&mut self.jobs, dead);
        if let Some(join) = self.thread.take() {
            let _ = join.join();
        }
    }
}

fn run(events: Sender<Event>, jobs: Receiver<Command>, snapshot: Arc<Mutex<Snapshot>>) {
    // The soundbar reboots after a factory reset or a power cycle, and Windows
    // then hands out a stale handle. Rather than surface that to the user, the
    // session thread keeps re-enumerating until the device answers again.
    loop {
        // Wait for the device to be present and talking.
        let Some(hid) = connect(&events, &snapshot) else {
            // Nothing to talk to yet: mark disconnected and retry.
            mark_disconnected(&events, &snapshot);
            if !wait_for_reconnect(&jobs) {
                return;
            }
            continue;
        };

        let mut link = Link::new(hid);

        // Initial read so the UI has data immediately.
        publish_state(&mut link, &events, &snapshot);

        let mut last_ok = Instant::now();
        let mut last_volume_poll = Instant::now();
        let mut last_light_poll = Instant::now();
        let mut last_eq_poll = Instant::now();
        let mut volume_poll_failures = 0u8;
        let mut queued = VecDeque::new();

        loop {
            let mut handled_command = false;
            match receive_command(&jobs, &mut queued) {
                Ok(command) => {
                    handled_command = true;
                    if apply(&mut link, command, &events, &snapshot) {
                        last_ok = Instant::now();
                    } else {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = events.send(Event::Disconnected);
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }

            // Track inbound traffic as a liveness signal.
            let drained = link.drain();
            if !drained.is_empty() {
                last_ok = Instant::now();
                if let Ok(mut snap) = snapshot.lock() {
                    for frame in drained {
                        let data = frame.data;
                        snap.frames += 1;
                        if let Some(response) = Response::parse(&data) {
                            snap.last_reply = Some(format!(
                                "{:#04X} [{}]",
                                response.index,
                                hex(&response.payload)
                            ));
                        }
                    }
                }
            }

            // Idle watchdog: the device stops answering the moment it reboots,
            // so probe it and reconnect when the probe goes unanswered.
            if last_ok.elapsed() >= SILENCE_TIMEOUT {
                let answered = ec_state::query(
                    &mut link,
                    ec::cmd::BASE_FUNCTION,
                    &[],
                    Duration::from_millis(700),
                )
                .is_some();
                if answered {
                    last_ok = Instant::now();
                } else {
                    break;
                }
            }

            // Keep the visible state current so changes made on the device
            // itself, or by another client, show up without user action.
            //
            // The official client does this by polling one command at a time
            // with a short timeout and only pushing a new state when something
            // actually changed (`SoundBoxHomeModel` / `SoundBoxLightModel`
            // `receive_data_from_usb`). Re-reading the whole state instead would
            // cost six commands per turn and swamp the device's small buffer.
            //
            // Only one command goes out per turn, chosen by whichever is most
            // overdue. A plain `if/else if` chain would starve the slower ones:
            // volume is due every 150 ms while the light table is due every
            // 300 ms, so volume would always win.
            //
            // Writes get priority: polling is skipped on a turn that handled a
            // command, so a drag is never starved by the poll.
            if !handled_command {
                // Pick by how far past its *own* deadline each command is, not
                // by raw elapsed time: comparing absolute elapsed across
                // different periods let a not-yet-due slow poll block a due
                // fast one, so all three converged on the same cadence instead
                // of the documented 150/300/400 ms.
                let late = |elapsed: Duration, interval: Duration| elapsed.saturating_sub(interval);
                let candidates = [
                    (late(last_volume_poll.elapsed(), VOLUME_POLL), 0u8),
                    (late(last_light_poll.elapsed(), LIGHT_POLL), 1),
                    (late(last_eq_poll.elapsed(), EQ_POLL), 2),
                ];
                // The winner is the most overdue; `is_zero` means nothing is due.
                let (how_late, which) = candidates
                    .into_iter()
                    .max_by_key(|(overdue, _)| *overdue)
                    .unwrap_or((Duration::ZERO, 0));

                if !how_late.is_zero() {
                    match which {
                        0 => {
                            last_volume_poll = Instant::now();
                            if poll_volume(&mut link, &events, &snapshot) {
                                volume_poll_failures = 0;
                                last_ok = Instant::now();
                            } else {
                                volume_poll_failures = volume_poll_failures.saturating_add(1);
                            }
                        }
                        1 => {
                            last_light_poll = Instant::now();
                            if poll_lighting(&mut link, &events, &snapshot) {
                                last_ok = Instant::now();
                            }
                        }
                        _ => {
                            last_eq_poll = Instant::now();
                            if poll_eq(&mut link, &events, &snapshot) {
                                last_ok = Instant::now();
                            }
                        }
                    }
                }
            }

            // A run of unanswered volume polls is logged but no longer tears the
            // link down. Measured on hardware the device answers in ~2 ms
            // (p50 = p99 = max over 40 samples per command), so a timeout here
            // does not mean the link is broken — it means the reply was missed
            // or consumed elsewhere, and reconnecting over that costs a full
            // re-enumeration plus a seven-command read.
            //
            // Real link loss is caught by the two signals that cannot false
            // positive: the silence watchdog above (`SILENCE_TIMEOUT` with no
            // inbound traffic at all) and any write failure, which returns
            // `false` from `apply` and breaks out of this loop.
            if volume_poll_failures >= VOLUME_POLL_FAILURES_BEFORE_LOG {
                volume_poll_failures = 0;
                if let Ok(mut snap) = snapshot.lock() {
                    snap.transcript
                        .push("volume poll missed several times in a row".to_string());
                }
            }

            // Idle watchdog: the device stops answering the moment it reboots,
            // so probe it and reconnect when the probe goes unanswered.
            if last_ok.elapsed() >= SILENCE_TIMEOUT {
                let answered = ec_state::query(
                    &mut link,
                    ec::cmd::BASE_FUNCTION,
                    &[],
                    Duration::from_millis(700),
                )
                .is_some();
                if answered {
                    last_ok = Instant::now();
                } else {
                    break;
                }
            }
        }

        // Device went away: drop the handle and go back to re-enumerating.
        drop(link);
        while jobs.try_recv().is_ok() {}
        mark_disconnected(&events, &snapshot);
    }
}

fn wait_for_reconnect(jobs: &Receiver<Command>) -> bool {
    match jobs.recv_timeout(Duration::from_millis(1500)) {
        // Commands that raced with a disconnection are discarded. Only losing
        // the channel (the app closing) should stop automatic reconnection.
        Ok(_) => {
            while jobs.try_recv().is_ok() {}
            true
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => false,
        Err(mpsc::RecvTimeoutError::Timeout) => true,
    }
}

/// Keep the latest consecutive value while preserving action order and barriers
/// such as effect selection, reset, and power-off.
fn enqueue_command(queue: &mut VecDeque<Command>, command: Command) {
    let replace = match (queue.back(), &command) {
        (Some(Command::SetVolume(_)), Command::SetVolume(_))
        | (Some(Command::SetEq(_)), Command::SetEq(_)) => true,
        (
            Some(Command::SetEffect {
                array_index: a,
                index: i,
                ..
            }),
            Command::SetEffect {
                array_index: b,
                index: j,
                ..
            },
        ) => a == b && i == j,
        (
            Some(Command::SetLightSwitch { array_index: a, .. }),
            Command::SetLightSwitch { array_index: b, .. },
        ) => a == b,
        _ => false,
    };
    if replace {
        queue.pop_back();
    }
    queue.push_back(command);
}

fn receive_command(
    jobs: &Receiver<Command>,
    queue: &mut VecDeque<Command>,
) -> Result<Command, mpsc::RecvTimeoutError> {
    if queue.is_empty() {
        enqueue_command(queue, jobs.recv_timeout(Duration::from_millis(60))?);
    }
    for command in jobs.try_iter() {
        enqueue_command(queue, command);
    }
    Ok(queue
        .pop_front()
        .expect("at least one command was received"))
}

/// How long the device may stay silent before we probe it.
const SILENCE_TIMEOUT: Duration = Duration::from_millis(2000);
/// How often the volume is polled while idle. The official client uses 150 ms.
const VOLUME_POLL: Duration = Duration::from_millis(150);
/// How often the light table is polled while idle (official: 300 ms).
const LIGHT_POLL: Duration = Duration::from_millis(300);
/// How many consecutive missed volume polls are noted in the transcript before
/// the counter resets. This is diagnostic only — it does not drop the link.
const VOLUME_POLL_FAILURES_BEFORE_LOG: u8 = 3;

/// How often the equaliser scene is polled while idle.
const EQ_POLL: Duration = Duration::from_millis(400);

/// Re-read just the volume. Returns false when the device did not answer.
fn poll_volume<T: EcTransport>(
    link: &mut Link<T>,
    events: &Sender<Event>,
    snapshot: &Arc<Mutex<Snapshot>>,
) -> bool {
    let Some(payload) =
        ec_state::query(link, ec::cmd::VOLUME_QUERY, &[], Duration::from_millis(120))
    else {
        return false;
    };
    // `read_state` accepts both shapes: `[max, current]`, and a bare
    // `[current]` when there is no range to report. Rejecting the short form
    // here made the poller disagree with the reader about what a valid reply
    // is, so a device using it would look permanently unresponsive.
    let (max, current) = match payload.len() {
        0 => return false,
        1 => (None, payload[0] as i64),
        _ => (Some(payload[0] as i64), payload[1] as i64),
    };

    let mut changed = false;
    let Ok(mut snap) = snapshot.lock() else {
        return false;
    };
    let max_changed = max.is_some_and(|m| snap.state.max_volume != Some(m));
    if max_changed || snap.state.volume != Some(current) {
        if max_changed {
            snap.state.max_volume = max;
        }
        snap.state.volume = Some(current);
        changed = true;
    }
    if changed {
        let _ = events.send(Event::State(Box::new(snap.state.clone())));
    }
    true
}

/// Re-read just the light table. Returns false when the device did not answer.
fn poll_lighting<T: EcTransport>(
    link: &mut Link<T>,
    events: &Sender<Event>,
    snapshot: &Arc<Mutex<Snapshot>>,
) -> bool {
    let Some(payload) = ec_state::query(
        link,
        ec::cmd::MOOD_LIGHT_QUERY,
        &[],
        Duration::from_millis(160),
    ) else {
        return false;
    };
    let Ok(mut snap) = snapshot.lock() else {
        return false;
    };
    let previous = snap.state.clone();
    if !ec_state::update_light_state(&mut snap.state, payload) {
        return false;
    }
    if snap.state != previous {
        let _ = events.send(Event::State(Box::new(snap.state.clone())));
    }
    true
}

/// Re-read just the equaliser scene. Returns false when the device did not answer.
fn poll_eq<T: EcTransport>(
    link: &mut Link<T>,
    events: &Sender<Event>,
    snapshot: &Arc<Mutex<Snapshot>>,
) -> bool {
    let Some(payload) = ec_state::query(link, ec::cmd::EQ_QUERY, &[], Duration::from_millis(140))
    else {
        return false;
    };
    let Some(scene) = payload.first().map(|b| *b as i64) else {
        return false;
    };

    let mut changed = false;
    let Ok(mut snap) = snapshot.lock() else {
        return false;
    };
    if snap.state.eq != Some(scene) {
        snap.state.eq = Some(scene);
        changed = true;
    }
    if changed {
        let _ = events.send(Event::State(Box::new(snap.state.clone())));
    }
    true
}

fn mark_disconnected(events: &Sender<Event>, snapshot: &Arc<Mutex<Snapshot>>) {
    if let Ok(mut snap) = snapshot.lock() {
        if snap.connected {
            snap.connected = false;
            snap.last_error = Some("设备未响应，正在自动重连…".to_string());
        }
        snap.strategy = "未连接".to_string();
        snap.state = crate::ec_state::EcState::default();
    }
    let _ = events.send(Event::Disconnected);
}

fn connect(events: &Sender<Event>, snapshot: &Arc<Mutex<Snapshot>>) -> Option<HidDevice> {
    let collection = match find_control_collection() {
        Ok(c) => c,
        Err(e) => {
            set_error(snapshot, e.clone());
            let _ = events.send(Event::Error(e));
            return None;
        }
    };
    let hid = match HidDevice::open(collection.clone()) {
        Ok(h) => h,
        Err(e) => {
            set_error(snapshot, e.clone());
            let _ = events.send(Event::Error(e));
            return None;
        }
    };
    let strategy = hid.strategy();
    let greeting: Vec<String> = hid.greeting().iter().map(|f| hex(f)).collect();
    let header = hid
        .greeting()
        .iter()
        .find_map(|f| Response::parse(f).and_then(|r| DeviceHeader::parse(&r.payload)));

    {
        let Ok(mut snap) = snapshot.lock() else {
            return None;
        };
        snap.connected = true;
        snap.collection = Some(collection.path.clone());
        snap.usage_page = collection.usage_page;
        snap.output_len = collection.output_len;
        snap.input_len = collection.input_len;
        snap.strategy = strategy.label().to_string();
        snap.device_product = collection.product.clone();
        snap.device_serial = collection.serial.clone();
        snap.header = header;
        snap.greeting = greeting;
        snap.log = hid.log().to_vec();
        if strategy == WriteStrategy::Unknown {
            snap.last_error = Some(
                "connected, but the HID driver rejected every output-report mechanism".to_string(),
            );
        }
    }

    let _ = events.send(Event::Connected {
        collection: Box::new(collection),
        strategy,
    });
    Some(hid)
}

fn publish_state<T: EcTransport>(
    link: &mut Link<T>,
    events: &Sender<Event>,
    snapshot: &Arc<Mutex<Snapshot>>,
) {
    // Refresh from the previous state rather than from scratch: a round where
    // the device stays silent must not blank the fields it did not answer.
    let previous = snapshot.lock().map(|s| s.state.clone()).unwrap_or_default();
    let state = ec_state::read_state_into(link, previous);
    let mut transcript = Vec::new();
    if let Some(base) = &state.base {
        transcript.push(format!("0x65 base function -> {}", hex(base)));
    }
    if let Some(features) = &state.features {
        transcript.push(format!("0xD8 device functions -> {}", hex(features)));
    }
    if let Some(v) = state.volume {
        transcript.push(format!("0x66 volume -> {v} (max {:?})", state.max_volume));
    }
    if let Some(v) = state.eq {
        transcript.push(format!("0xC4 eq -> {v}"));
    }
    if let Some(s) = state.status {
        transcript.push(format!(
            "0xF2 device status -> Peer={} LB={} RB={} DB={} DC={} ED={}",
            s[0], s[1], s[2], s[3], s[4], s[5]
        ));
    }
    if let Some(raw) = &state.light_raw {
        transcript.push(format!("0x6B mood lighting -> {}", hex(raw)));
    }

    if let Ok(mut snap) = snapshot.lock() {
        snap.state = state.clone();
        snap.transcript = transcript;
    }
    let _ = events.send(Event::State(Box::new(state)));
}

/// Run one command. Returns `false` when the link looks dead, which tells the
/// caller to drop the handle and re-enumerate.
fn apply<T: EcTransport>(
    link: &mut Link<T>,
    command: Command,
    events: &Sender<Event>,
    snapshot: &Arc<Mutex<Snapshot>>,
) -> bool {
    if matches!(command, Command::SetVolume(_) | Command::SetEq(_)) {
        return apply_verified_value(link, command, events, snapshot);
    }
    let result = match command {
        Command::Refresh => {
            publish_state(link, events, snapshot);
            return true;
        }
        Command::SetVolume(level) => ec_state::set_volume(link, level),
        Command::SetEq(index) => ec_state::set_eq(link, index),
        Command::SetEffect {
            array_index,
            index,
            rgb,
            brightness,
            speed,
        } => ec_state::set_mood_light(link, array_index, index, rgb, brightness, speed),
        Command::ResetEffect { array_index, index } => {
            ec_state::reset_mood_light(link, array_index, index)
        }
        Command::SetLightSwitch { array_index, on } => apply_light_switch(link, array_index, on),
        Command::FactoryReset => ec_state::factory_reset(link),
        Command::Shutdown => ec_state::shutdown(link),
    };

    match result {
        Ok(()) => {
            if let Ok(mut snap) = snapshot.lock() {
                snap.last_error = None;
                snap.transcript.push(format!("{command:?} sent"));
            }
            // Refresh only what the command could have changed.
            //
            // This used to run the whole seven-command `read_state` after every
            // write. Three sliders fire `on_value_changed` per pixel, so a
            // single drag queued dozens of writes that each triggered seven more
            // commands — which is exactly the dense burst that silences this
            // device. One targeted read per write keeps the pressure flat.
            let settle = match command {
                Command::FactoryReset | Command::Shutdown => Duration::from_millis(1400),
                Command::SetEffect { .. } | Command::ResetEffect { .. } => {
                    Duration::from_millis(900)
                }
                // The switch path verifies itself and already waited.
                Command::SetLightSwitch { .. } => Duration::from_millis(120),
                _ => Duration::from_millis(120),
            };
            std::thread::sleep(settle);

            match command {
                // The device reboots: every field is stale, so read everything.
                Command::FactoryReset | Command::Shutdown => {
                    publish_state(link, events, snapshot);
                }
                Command::SetVolume(_) => {
                    poll_volume(link, events, snapshot);
                }
                Command::SetEq(_) => {
                    poll_eq(link, events, snapshot);
                }
                Command::SetEffect { .. } | Command::ResetEffect { .. } => {
                    poll_lighting(link, events, snapshot);
                }
                // `apply_light_switch` and the EQ read-back already updated the
                // snapshot, and `Command::Refresh` handled itself above.
                Command::SetLightSwitch { .. } => {}
                Command::Refresh => unreachable!("handled before the write"),
            }
            let _ = events.send(Event::Raw(format!("{command:?} ok")));
            true
        }
        Err(e) => {
            set_error(snapshot, e.clone());
            let _ = events.send(Event::Error(e));
            // A write failure usually means the handle is gone.
            false
        }
    }
}

/// The driver accepting a report does not mean the soundbar applied it. In a
/// mixed burst the unit can ignore an EQ write after a volume write. Read the
/// actual value back and retry a bounded number of times.
fn apply_verified_value<T: EcTransport>(
    link: &mut Link<T>,
    command: Command,
    events: &Sender<Event>,
    snapshot: &Arc<Mutex<Snapshot>>,
) -> bool {
    for _ in 0..3 {
        let result = match command {
            Command::SetVolume(value) => ec_state::set_volume(link, value),
            Command::SetEq(value) => ec_state::set_eq(link, value),
            _ => unreachable!("only volume and EQ use this path"),
        };
        if let Err(error) = result {
            set_error(snapshot, error.clone());
            let _ = events.send(Event::Error(error));
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
        let read = match command {
            Command::SetVolume(_) => poll_volume(link, events, snapshot),
            Command::SetEq(_) => poll_eq(link, events, snapshot),
            _ => unreachable!(),
        };
        let confirmed = read
            && snapshot.lock().is_ok_and(|s| match command {
                Command::SetVolume(value) => s.state.volume == Some(value as i64),
                Command::SetEq(value) => s.state.eq == Some(value as i64),
                _ => false,
            });
        if confirmed {
            if let Ok(mut s) = snapshot.lock() {
                s.last_error = None;
                s.transcript.push(format!("{command:?} confirmed"));
            }
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let error = "音箱未确认设置，请重试".to_string();
    set_error(snapshot, error.clone());
    let _ = events.send(Event::Error(error));
    // The link can still be healthy; an unconfirmed value is not a disconnect.
    true
}

/// Write the master switch and make sure it stuck.
///
/// A single `0x6B` is often not confirmed: the device applies it but then
/// reports the previous state for a while, and a rewrite is what makes it
/// observable. Measured on hardware, a plain alternating run matched only 5 of
/// 10 reads; confirming and rewriting the same value is the fix.
fn apply_light_switch<T: EcTransport>(
    link: &mut Link<T>,
    array_index: u8,
    on: bool,
) -> Result<(), String> {
    for attempt in 0..3 {
        ec_state::set_light_switch(link, array_index, on)?;
        std::thread::sleep(Duration::from_millis(350));
        match ec_state::read_light_switch(link) {
            // The device agrees (or holds no table at all, which is its own
            // way of saying "off").
            Some(seen) if seen == on => return Ok(()),
            // Not confirmed yet; write it again.
            _ => {
                if attempt < 2 {
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }
    Ok(())
}

fn set_error(snapshot: &Arc<Mutex<Snapshot>>, message: String) {
    if let Ok(mut snap) = snapshot.lock() {
        snap.last_error = Some(message);
    }
}

pub fn hex(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Convenience used by tools: one-shot state read.
pub fn read_state() -> Result<EcState, String> {
    let collection = find_control_collection()?;
    let hid = HidDevice::open(collection)?;
    if hid.strategy() == WriteStrategy::Unknown {
        return Err("the HID driver rejects output reports on this device".to_string());
    }
    let mut link = Link::new(hid);
    Ok(ec_state::read_state(&mut link))
}

/// Kept so the public surface documents the protocol family in use.
#[allow(dead_code)]
pub const QUERY_COMMAND: u8 = ec::cmd::BASE_FUNCTION;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::testing::FakeTransport;

    /// `read_state` accepts a bare `[current]` volume reply, so the poller must
    /// too — otherwise the two disagree about the same frame.
    #[test]
    fn volume_poll_accepts_the_short_reply() {
        let mut link = Link::new(FakeTransport::new());
        link.transport_mut()
            .push_response(ec::cmd::VOLUME_QUERY, &[0x09]);
        let events = std::sync::mpsc::channel().0;
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));

        assert!(poll_volume(&mut link, &events, &snapshot));
        let snap = snapshot.lock().unwrap();
        assert_eq!(snap.state.volume, Some(9));
        // No max was reported, so none is invented.
        assert_eq!(snap.state.max_volume, None);
    }

    #[test]
    fn volume_poll_reads_both_fields() {
        let mut link = Link::new(FakeTransport::new());
        link.transport_mut()
            .push_response(ec::cmd::VOLUME_QUERY, &[0x10, 0x0B]);
        let events = std::sync::mpsc::channel().0;
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));

        assert!(poll_volume(&mut link, &events, &snapshot));
        let snap = snapshot.lock().unwrap();
        assert_eq!(snap.state.max_volume, Some(16));
        assert_eq!(snap.state.volume, Some(11));
    }

    /// An empty payload is not a reading.
    #[test]
    fn volume_poll_rejects_an_empty_reply() {
        let mut link = Link::new(FakeTransport::new());
        link.transport_mut()
            .push_response(ec::cmd::VOLUME_QUERY, &[]);
        let events = std::sync::mpsc::channel().0;
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        assert!(!poll_volume(&mut link, &events, &snapshot));
    }
    #[test]
    fn lighting_off_clears_records_but_preserves_array_index() {
        let mut link = Link::new(FakeTransport::new());
        let (events, _) = mpsc::channel();
        let mut initial = Snapshot::default();
        ec_state::update_light_state(&mut initial.state, vec![8, 1, 1, 20, 40, 60, 50, 255]);
        let snapshot = Arc::new(Mutex::new(initial));
        link.transport_mut()
            .push_response(ec::cmd::MOOD_LIGHT_QUERY, &[0]);
        assert!(poll_lighting(&mut link, &events, &snapshot));
        let state = &snapshot.lock().unwrap().state;
        assert_eq!(state.light_group, Some(8));
        assert!(!ec_state::light_table_is_live(state));
    }

    #[test]
    fn malformed_light_reply_does_not_change_the_last_good_state() {
        let mut link = Link::new(FakeTransport::new());
        let (events, _) = mpsc::channel();
        let mut initial = Snapshot::default();
        ec_state::update_light_state(&mut initial.state, vec![8, 1, 1, 20, 40, 60, 50, 255]);
        let expected = initial.state.clone();
        let snapshot = Arc::new(Mutex::new(initial));
        for payload in [&[][..], &[8, 1], &[8, 1, 1, 20, 40, 60, 50, 255, 3]] {
            link.transport_mut()
                .push_response(ec::cmd::MOOD_LIGHT_QUERY, payload);
            assert!(!poll_lighting(&mut link, &events, &snapshot));
            assert_eq!(snapshot.lock().unwrap().state, expected);
        }
    }

    #[test]
    fn successful_write_without_readback_does_not_fake_device_confirmation() {
        let mut link = Link::new(FakeTransport::new());
        let (events, _) = mpsc::channel();
        let mut initial = Snapshot::default();
        initial.state.eq = Some(0);
        initial.state.volume = Some(5);
        let snapshot = Arc::new(Mutex::new(initial));
        assert!(apply(&mut link, Command::SetEq(2), &events, &snapshot));
        assert!(apply(&mut link, Command::SetVolume(9), &events, &snapshot));
        assert_eq!(snapshot.lock().unwrap().state.eq, Some(0));
        assert_eq!(snapshot.lock().unwrap().state.volume, Some(5));
    }

    #[test]
    fn command_backlog_keeps_latest_values_and_preserves_reset_order() {
        let (tx, rx) = mpsc::channel();
        for value in 1..=10 {
            tx.send(Command::SetVolume(value)).unwrap();
        }
        tx.send(Command::ResetEffect {
            array_index: 8,
            index: 3,
        })
        .unwrap();
        for value in 11..=16 {
            tx.send(Command::SetVolume(value)).unwrap();
        }
        let mut queue = VecDeque::new();
        assert!(matches!(
            receive_command(&rx, &mut queue),
            Ok(Command::SetVolume(10))
        ));
        assert!(matches!(
            receive_command(&rx, &mut queue),
            Ok(Command::ResetEffect { index: 3, .. })
        ));
        assert!(matches!(
            receive_command(&rx, &mut queue),
            Ok(Command::SetVolume(16))
        ));
        assert!(queue.is_empty());
    }

    #[test]
    fn effect_backlog_is_coalesced_only_within_the_same_effect() {
        let mut queue = VecDeque::new();
        for (index, brightness) in [(3, 50), (3, 60), (1, 70), (3, 80)] {
            enqueue_command(
                &mut queue,
                Command::SetEffect {
                    array_index: 8,
                    index,
                    rgb: (20, 40, 60),
                    brightness,
                    speed: 5,
                },
            );
        }
        assert_eq!(queue.len(), 3);
        assert!(matches!(
            queue.pop_front(),
            Some(Command::SetEffect {
                index: 3,
                brightness: 60,
                ..
            })
        ));
        assert!(matches!(
            queue.pop_front(),
            Some(Command::SetEffect {
                index: 1,
                brightness: 70,
                ..
            })
        ));
        assert!(matches!(
            queue.pop_front(),
            Some(Command::SetEffect {
                index: 3,
                brightness: 80,
                ..
            })
        ));
    }
    #[test]
    fn offline_power_commands_do_not_terminate_reconnection_or_replay_later() {
        let (tx, rx) = mpsc::channel();
        tx.send(Command::Shutdown).unwrap();
        tx.send(Command::FactoryReset).unwrap();
        tx.send(Command::SetVolume(9)).unwrap();
        assert!(wait_for_reconnect(&rx));
        assert!(rx.try_recv().is_err());
        drop(tx);
        assert!(!wait_for_reconnect(&rx));
    }

    #[test]
    fn eq_write_retries_until_the_device_really_agrees() {
        let mut link = Link::new(FakeTransport::new());
        link.transport_mut().push_response(ec::cmd::EQ_QUERY, &[0]);
        link.transport_mut().push_response(ec::cmd::EQ_QUERY, &[2]);
        let (events, _) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        assert!(apply(&mut link, Command::SetEq(2), &events, &snapshot));
        assert_eq!(snapshot.lock().unwrap().state.eq, Some(2));
        let writes = link
            .transport()
            .written_frames()
            .iter()
            .filter(|f| f[3] == ec::cmd::EQ_SET)
            .count();
        assert_eq!(writes, 2);
        assert!(snapshot.lock().unwrap().last_error.is_none());
    }
}
