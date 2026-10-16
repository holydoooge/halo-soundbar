//! WinUI 3 front-end built on `windows-reactor`.
//!
//! Mirrors the official client's device page: a `NavigationView` shell with
//! 首页 / 灯效 / 设置. All HID work happens on a background thread; the UI only
//! ever receives snapshots.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_reactor::*;
// `windows_reactor::TitleBar` (the widget) collides with `Symbol::TitleBar`.
use windows_reactor::TitleBar as TitleBarTrigger;

use halo_soundbar::device::{Command, Session, Snapshot};
use halo_soundbar::ec_state;

/// How often the UI asks the device thread for its latest snapshot.
const UI_POLL: Duration = Duration::from_millis(250);

/// Shared handle to the device session thread.
pub struct Bridge {
    commands: Mutex<Option<std::sync::mpsc::Sender<Command>>>,
    latest: Arc<Mutex<Snapshot>>,
    dirty: Arc<AtomicBool>,
}

impl Bridge {
    pub fn send(&self, command: Command) {
        if let Ok(guard) = self.commands.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(command);
            }
        }
    }

    /// Snapshot plus whether anything changed since the last read.
    fn take(&self) -> Option<Snapshot> {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return None;
        }
        self.latest.lock().ok().map(|s| s.clone())
    }
}

/// Messages the shell reacts to.
#[derive(Clone)]
pub enum Message {
    Tick,
    Navigate(String),
    VolumeChanged(f64),
    /// Kept for tooling/tests. The GUI never asks for a refresh: the session
    /// thread reconnects and polls on its own.
    #[allow(dead_code)]
    Refresh,
    EqSelected(u8),
    EffectSelected(u8),
    BrightnessChanged(u8),
    SpeedChanged(u8),
    /// Mood-light master switch.
    AmbienceToggled(bool),
    /// The user picked a colour for the active effect.
    ColorChanged(u8, u8, u8),
    /// Apply the colour selected in the picker.
    ApplyColor,
    /// The user asked to flip the pane.
    TogglePane,
    /// The control reported a new pane state.
    PaneOpenChanged(bool),
    /// A destructive button was pressed; ask first.
    ConfirmRequested(ConfirmKind),
    /// The confirmation dialog closed.
    Confirmed(ContentDialogResult),
}

/// How long an optimistic value may mask the device before it is abandoned.
const PENDING_TTL: Duration = Duration::from_secs(4);

/// This application's own version, shown in 设置 next to the firmware version.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Sidebar width when expanded. The WinUI default (320) is wider than this
/// two-item menu needs.
const PANE_WIDTH: f64 = 158.0;

/// A destructive action that must be confirmed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    FactoryReset,
    Shutdown,
    ResetEffect,
}

/// Title, body and primary-button label for a confirmation dialog.
fn confirm_text(kind: ConfirmKind) -> (&'static str, &'static str, &'static str) {
    match kind {
        ConfirmKind::FactoryReset => (
            "恢复出厂设置",
            "设备会重启，所有设备端设置将被清除。此操作不可撤销。",
            "恢复",
        ),
        ConfirmKind::Shutdown => ("关机", "关闭音箱。下次使用时按设备上的电源键开机。", "关机"),
        ConfirmKind::ResetEffect => (
            "恢复默认灯效",
            "当前效果的颜色、亮度和速度将恢复为出厂值。",
            "恢复",
        ),
    }
}

/// Shell state.
pub struct App {
    bridge: Arc<Bridge>,
    snapshot: Snapshot,
    page: String,
    /// Live slider position, so the UI stays responsive before the device answers.
    volume_drag: Option<f64>,
    volume_at: Option<Instant>,
    /// Last value sent to the device for the effect's brightness/speed.
    ///
    /// Cleared when the device confirms it, so a value changed on the device
    /// itself (or by another client) takes over the slider again instead of
    /// being masked by a stale local value.
    brightness_sent: Option<u8>,
    speed_sent: Option<u8>,
    /// Pane expansion, mirrored so the title-bar toggle can flip it. The
    /// control is also free to change it (clicking the pane title does).
    pane_open: bool,
    /// Which destructive action is waiting for confirmation, if any.
    confirm: Option<ConfirmKind>,
    /// When an optimistic value was last sent, for [`PENDING_TTL`] expiry.
    pending_at: Option<Instant>,
    /// Optimistic EQ scene plus when it was asked for.
    eq_sent: Option<(u8, Instant)>,
    effect_sent: Option<(u8, Instant)>,
    /// Colour shown by the picker, derived from the active effect's record.
    ///
    /// `None` means "not currently held", and the picker takes whatever the
    /// device reports on the next tick — the same relationship the sliders have
    /// with `brightness_sent` / `speed_sent`.
    picker_color: Option<(u8, u8, u8)>,
    /// Unconfirmed selection, scoped to the effect it was picked for.
    color_draft: Option<(u8, (u8, u8, u8))>,
    /// Slider values waiting to be written. A drag fires `on_value_changed` per
    /// pixel, so writes are coalesced to one per tick instead of one per pixel —
    /// a drag used to queue dozens of commands and stall the session thread.
    pending_volume: Option<u8>,
    pending_brightness: bool,
    pending_speed: bool,
    /// Last master-switch state the user asked for.
    ///
    /// With the switch off the device reports no light records at all (`0x6A`
    /// answers a bare `[00]`), so there is nothing to read the state from.
    /// Without this the toggle would fall back to "on" and never show the state
    /// the user actually set.
    ambience_sent: Option<bool>,
    _timer: Option<ComponentTimer>,
}

impl App {
    /// The window's single confirmation dialog, driven by `self.confirm`.
    fn confirm_dialog(&self, context: &mut ViewContext<Self>) -> ContentDialog {
        let pending = self.confirm.unwrap_or(ConfirmKind::FactoryReset);
        let (title, body, primary) = confirm_text(pending);
        ContentDialog::new()
            .title(title)
            .content(body)
            .primary_button_text(primary)
            .close_button_text("取消")
            .is_open(self.confirm.is_some())
            .on_closed(context.callback(|result: ContentDialogResult| Message::Confirmed(result)))
    }

    /// Note that an optimistic value just went out, (re)starting its TTL.
    fn mark_pending(&mut self) {
        self.pending_at = Some(Instant::now());
    }

    fn send(&self, command: Command) {
        self.bridge.send(command);
    }

    fn effect_record(&self, index: u8) -> Option<&ec_state::LightGroup> {
        self.snapshot
            .state
            .light_groups
            .iter()
            .find(|g| g.index == index)
    }

    /// The effect the device is actually driving.
    ///
    /// `0x6A` reports this as `payload[1]` (kept in `state.light_index`). The
    /// first table row must not be used as a fallback: `EA_08` often lists row 1
    /// even while effect 2/3 is selected, and row 1 deliberately has no rate
    /// control.
    ///
    /// Index 8 is the master switch, not an effect. When the switch is reported
    /// there, `EA_08` gives no effect number at all, so the first real record in
    /// the table is the best available answer.
    fn active_effect(&self) -> u8 {
        self.effect_sent
            .map(|(index, _)| index)
            .unwrap_or_else(|| self.device_effect())
    }

    fn device_effect(&self) -> u8 {
        let from_device = self
            .snapshot
            .state
            .light_index
            .map(|i| i as u8)
            .filter(|i| (1..=7).contains(i));
        from_device
            .or_else(|| {
                self.snapshot
                    .state
                    .light_groups
                    .iter()
                    .map(|g| g.index)
                    .find(|i| (1..=7).contains(i))
            })
            .unwrap_or(ec_state::HALO_FIRST_EFFECT)
    }

    /// Brightness shown on the slider, in UI units (1..10).
    ///
    /// While a value is in flight the local one wins, so the thumb does not
    /// jump back mid-drag.
    fn brightness_display(&self, effect: u8) -> u8 {
        if let Some(sent) = self.brightness_sent {
            return sent;
        }
        self.effect_record(effect)
            .map(|r| ec_state::brightness_display_from_wire(r.brightness))
            .unwrap_or(ec_state::HALO_BRIGHTNESS_MAX)
    }

    /// Speed shown on the slider, in UI units (1..10).
    fn speed_display(&self, effect: u8) -> u8 {
        if let Some(sent) = self.speed_sent {
            return sent;
        }
        self.effect_record(effect)
            .map(|r| ec_state::speed_display_from_wire(r.speed))
            .unwrap_or(ec_state::HALO_BRIGHTNESS_MAX)
    }

    /// Colour shown by the picker, in RGB.
    fn color_display(&self, effect: u8) -> (u8, u8, u8) {
        if let Some((index, rgb)) = self.color_draft {
            if index == effect {
                return rgb;
            }
        }
        if let Some(rgb) = self.picker_color {
            return rgb;
        }
        self.effect_record(effect)
            .map(|r| (r.r, r.g, r.b))
            .unwrap_or((255, 255, 255))
    }

    /// Drop a pending value once the device reports it, so later changes made on
    /// the device itself are no longer masked by our own optimistic value.
    ///
    /// A pending value also expires after [`PENDING_TTL`]. Without that, a write
    /// the device never echoes — a frame lost in one of the dense bursts this
    /// codebase documents, or a value the device normalises — latched the
    /// slider at a position the device does not hold, permanently.
    fn reconcile_pending(&mut self) {
        let now = Instant::now();
        if !self.snapshot.connected {
            self.volume_drag = None;
            self.volume_at = None;
            self.eq_sent = None;
            self.effect_sent = None;
            self.ambience_sent = None;
            self.confirm = None;
            self.pending_volume = None;
            self.clear_light_edits();
            return;
        }
        if self
            .volume_at
            .is_some_and(|at| now.duration_since(at) >= PENDING_TTL)
            || self
                .volume_drag
                .is_some_and(|v| self.snapshot.state.volume == Some(v as i64))
        {
            self.volume_drag = None;
            self.volume_at = None;
        }
        if let Some((index, at)) = self.effect_sent {
            if self.device_effect() == index || now.duration_since(at) >= PENDING_TTL {
                self.effect_sent = None;
            }
        }
        if self
            .pending_at
            .is_some_and(|sent| now.duration_since(sent) >= PENDING_TTL)
        {
            self.brightness_sent = None;
            self.speed_sent = None;
            self.picker_color = None;
            self.ambience_sent = None;
            self.pending_at = None;
        }
        // Polling must preserve an unconfirmed selection for the same effect.
        if !self.snapshot.connected
            || self
                .color_draft
                .is_some_and(|(index, _)| index != self.active_effect())
        {
            self.color_draft = None;
            self.picker_color = None;
        }
        if let Some((wanted, at)) = self.eq_sent {
            let agreed = self.snapshot.state.eq == Some(wanted as i64);
            if agreed || now.duration_since(at) >= PENDING_TTL {
                self.eq_sent = None;
            }
        }
        if let Some(wanted) = self.ambience_sent {
            if self
                .snapshot
                .state
                .light_groups
                .iter()
                .find(|g| g.index == ec_state::HALO_LIGHT_SWITCH_INDEX)
                .is_some_and(|g| (g.b == 1) == wanted)
            {
                self.ambience_sent = None;
            }
        }
        if let Some((r, g, b)) = self.picker_color {
            if self
                .effect_record(self.active_effect())
                .is_some_and(|rec| (rec.r, rec.g, rec.b) == (r, g, b))
            {
                self.picker_color = None;
            }
        }
        if let Some(sent) = self.brightness_sent {
            let wire = ec_state::brightness_wire_from_display(sent);
            if self
                .effect_record(self.active_effect())
                .is_some_and(|r| r.brightness == wire)
            {
                self.brightness_sent = None;
            }
        }
        if let Some(sent) = self.speed_sent {
            if self
                .effect_record(self.active_effect())
                .is_some_and(|r| r.speed == sent)
            {
                self.speed_sent = None;
            }
        }
    }

    /// Currently selected effect scene (`0xD5` query).
    fn selected_eq(&self) -> Option<u8> {
        self.eq_sent
            .map(|(index, _)| index)
            .or(self.snapshot.state.eq.map(|v| v as u8))
    }

    fn array_index(&self) -> u8 {
        self.snapshot.state.light_group.unwrap_or(8) as u8
    }

    /// Connection state shown in the title bar. Reconnection is automatic, so
    /// this is purely informational.
    fn connection_label(&self) -> String {
        if self.snapshot.connected {
            "已连接".to_string()
        } else {
            "正在连接…".to_string()
        }
    }

    /// Mood-light master switch.
    ///
    /// `EA_08` reports the switch as effect index 8, with the on/off state in
    /// the record's **`b`** field (`1` = on, `0` = off) — not in `speed`.
    fn light_switch_on(&self) -> bool {
        if let Some(on) = self.ambience_sent {
            return on;
        }
        // A bare `[00]` reply *is* "the switch is off" — the device holds no
        // table at all then, so a stale record must not be consulted. Checking
        // the live flag first is what stops the toggle sticking at "on" after
        // the light is switched off from the device or the official client.
        if !ec_state::light_table_is_live(&self.snapshot.state) {
            // Prefer what the user just asked for, so the toggle still responds
            // while the device has not confirmed yet.
            return self.ambience_sent.unwrap_or(false);
        }
        self.snapshot
            .state
            .light_groups
            .iter()
            .find(|g| g.index == ec_state::HALO_LIGHT_SWITCH_INDEX)
            .map(|g| g.b == 1)
            // Records exist but the switch record is missing: treat as on, the
            // device is clearly driving light.
            .unwrap_or(true)
    }

    /// Push one effect to the device.
    ///
    /// The wire command writes a whole record. Merge every applied local edit
    /// before sending, so another slider change cannot undo a colour or slider
    /// value that the device has not reported back yet. The unconfirmed picker
    /// draft is deliberately excluded.
    ///
    /// Effects whose `EA_08` entry forbids a parameter keep whatever the device
    /// reports for it, so a disabled control can never corrupt the record.
    fn push_effect(&mut self) {
        if !self.can_edit_light() {
            return;
        }
        let array_index = self.array_index();
        let index = self.active_effect();
        let record = self.effect_record(index);

        // Keep whatever the device reports for fields we are not touching.
        let rgb = self
            .picker_color
            .or_else(|| record.map(|r| (r.r, r.g, r.b)))
            .unwrap_or((255, 255, 255));
        let wire_brightness = record.map(|r| r.brightness).unwrap_or(255);
        let wire_speed = record.map(|r| r.speed).unwrap_or(255);
        let brightness = if ec_state::halo_brightness_settable(index, wire_brightness) {
            self.brightness_sent
                .map(ec_state::brightness_wire_from_display)
                .unwrap_or(wire_brightness)
        } else {
            wire_brightness
        };
        let speed = if ec_state::halo_rate_settable(index, wire_speed) {
            self.speed_sent
                .map(ec_state::speed_wire_from_display)
                .unwrap_or(wire_speed)
        } else {
            wire_speed
        };

        self.send(Command::SetEffect {
            array_index,
            index,
            rgb,
            brightness,
            speed,
        });
    }

    fn can_edit_light(&self) -> bool {
        self.snapshot.connected
            && self.light_switch_on()
            && self.effect_record(self.active_effect()).is_some()
    }

    fn clear_light_edits(&mut self) {
        self.brightness_sent = None;
        self.speed_sent = None;
        self.picker_color = None;
        self.color_draft = None;
        self.pending_brightness = false;
        self.pending_speed = false;
        self.pending_at = None;
    }

    fn flush_pending(&mut self) {
        if let Some(level) = self.pending_volume.take() {
            if self.snapshot.connected {
                self.send(Command::SetVolume(level));
            }
        }
        let bright = std::mem::take(&mut self.pending_brightness);
        let speed = std::mem::take(&mut self.pending_speed);
        if bright || speed {
            self.push_effect();
        }
    }
}

impl App {
    fn handle_message(&mut self, message: Message) {
        // Controls can emit a final event while the connection is disappearing.
        if !self.snapshot.connected
            && matches!(
                message,
                Message::VolumeChanged(_)
                    | Message::EqSelected(_)
                    | Message::EffectSelected(_)
                    | Message::ColorChanged(..)
                    | Message::ApplyColor
                    | Message::BrightnessChanged(_)
                    | Message::SpeedChanged(_)
                    | Message::AmbienceToggled(_)
                    | Message::ConfirmRequested(_)
                    | Message::Confirmed(_)
            )
        {
            self.confirm = None;
            return;
        }
        match message {
            Message::Tick => {
                let previous_effect = self.active_effect();
                if let Some(snapshot) = self.bridge.take() {
                    self.snapshot = snapshot;
                    // Only release the drag once the device agrees with it:
                    // clearing on *any* snapshot snapped the thumb back to a
                    // volume the device had not caught up to yet.
                    if self.volume_drag.is_some_and(|drag| {
                        self.snapshot.state.volume.map(|v| v as f64) == Some(drag)
                    }) {
                        self.volume_drag = None;
                    }
                    // Adopt device-reported values once they land, so changes
                    // made elsewhere take over the sliders again.
                }
                // Timeouts must also run when the device state is unchanged.
                self.reconcile_pending();
                if previous_effect != self.active_effect() {
                    self.clear_light_edits();
                }
                // Flush the coalesced slider values: at most one volume write and
                // one effect write per tick, however many pixels the drag moved.
                self.flush_pending();
            }
            Message::Navigate(page) => {
                // Preserve the final slider value when navigation happens
                // before the next 250 ms tick.
                self.flush_pending();
                self.page = page;
            }
            Message::VolumeChanged(value) => {
                let level = value
                    .round()
                    .clamp(0.0, self.snapshot.state.max_volume.unwrap_or(16) as f64);
                self.volume_drag = Some(level);
                self.volume_at = Some(Instant::now());
                // Written on the next tick, so a drag produces one command per
                // tick rather than one per pixel.
                self.pending_volume = Some(level as u8);
            }
            Message::Refresh => self.send(Command::Refresh),
            Message::EqSelected(index) => {
                self.send(Command::SetEq(index));
                // Optimistic, so the card moves immediately. If the device
                // rejects the write the session snapshot stays byte-identical
                // and the UI would never hear about it, so the pending scene is
                // remembered and only accepted once the device agrees.
                self.eq_sent = Some((index, Instant::now()));
            }
            Message::EffectSelected(index) => {
                if !self.can_edit_light() {
                    return;
                }
                self.clear_light_edits();
                // Selecting an effect writes its stored colour back, which is
                // how the official model applies a scene.
                if let Some(rgb) = self.effect_record(index).map(|r| (r.r, r.g, r.b)) {
                    self.send(Command::SetEffect {
                        array_index: self.array_index(),
                        index,
                        rgb,
                        brightness: self
                            .effect_record(index)
                            .map(|r| r.brightness)
                            .unwrap_or(255),
                        speed: self.effect_record(index).map(|r| r.speed).unwrap_or(255),
                    });
                    self.effect_sent = Some((index, Instant::now()));
                }
            }
            Message::ColorChanged(r, g, b) => {
                if self.can_edit_light() && ec_state::halo_rgb_settable(self.active_effect()) {
                    self.color_draft = Some((self.active_effect(), (r, g, b)));
                }
            }
            Message::ApplyColor => {
                if let Some((index, rgb)) = self.color_draft {
                    if index == self.active_effect()
                        && self.snapshot.connected
                        && ec_state::light_table_is_live(&self.snapshot.state)
                        && self.light_switch_on()
                        && ec_state::halo_rgb_settable(index)
                    {
                        self.color_draft = None;
                        self.picker_color = Some(rgb);
                        self.mark_pending();
                        self.push_effect();
                    }
                }
            }
            Message::BrightnessChanged(value) => {
                if !self.can_edit_light()
                    || !self
                        .effect_record(self.active_effect())
                        .is_some_and(|r| ec_state::halo_brightness_settable(r.index, r.brightness))
                {
                    return;
                }
                self.brightness_sent =
                    Some(value.clamp(ec_state::HALO_BRIGHTNESS_MIN, ec_state::HALO_BRIGHTNESS_MAX));
                self.mark_pending();
                self.pending_brightness = true;
            }
            Message::SpeedChanged(value) => {
                if !self.can_edit_light()
                    || !self
                        .effect_record(self.active_effect())
                        .is_some_and(|r| ec_state::halo_rate_settable(r.index, r.speed))
                {
                    return;
                }
                self.speed_sent =
                    Some(value.clamp(ec_state::HALO_BRIGHTNESS_MIN, ec_state::HALO_BRIGHTNESS_MAX));
                self.mark_pending();
                self.pending_speed = true;
            }
            Message::AmbienceToggled(on) => {
                self.clear_light_edits();
                self.ambience_sent = Some(on);
                self.mark_pending();
                let array_index = self.array_index();
                self.send(Command::SetLightSwitch { array_index, on });
            }
            Message::TogglePane => self.pane_open = !self.pane_open,
            Message::PaneOpenChanged(open) => self.pane_open = open,
            Message::ConfirmRequested(kind) => self.confirm = Some(kind),
            Message::Confirmed(result) => {
                // Take the pending action first, then clear, so dismissing with
                // Esc or the close button cannot leave the dialog latched open.
                let kind = self.confirm.take();
                if result == ContentDialogResult::Primary {
                    match kind {
                        Some(ConfirmKind::FactoryReset) => self.send(Command::FactoryReset),
                        Some(ConfirmKind::Shutdown) => self.send(Command::Shutdown),
                        Some(ConfirmKind::ResetEffect) => {
                            if !self.can_edit_light() {
                                return;
                            }
                            self.clear_light_edits();
                            let array_index = self.array_index();
                            let index = self.active_effect();
                            self.send(Command::ResetEffect { array_index, index });
                        }
                        None => {}
                    }
                }
            }
        }
    }
}

impl Component for App {
    type Message = Message;
    type Input = ();

    fn create(_input: &(), context: &ComponentContext<Self>) -> Self {
        let latest = Arc::new(Mutex::new(Snapshot::default()));
        let dirty = Arc::new(AtomicBool::new(false));
        let (command_tx, command_rx) = std::sync::mpsc::channel::<Command>();

        let bridge = Arc::new(Bridge {
            commands: Mutex::new(Some(command_tx)),
            latest: latest.clone(),
            dirty: dirty.clone(),
        });

        let publisher = latest.clone();
        let flag = dirty.clone();
        context.spawn_background(move |token| {
            let session = Session::start();
            session.refresh();
            while !token.is_cancelled() {
                while let Ok(command) = command_rx.try_recv() {
                    session.send(command);
                }
                let _ = session.poll_events();
                if let Ok(mut slot) = publisher.lock() {
                    let snapshot = session.snapshot();
                    if snapshot.frames != slot.frames
                        || snapshot.connected != slot.connected
                        || snapshot.last_reply != slot.last_reply
                        || snapshot.state != slot.state
                    {
                        *slot = snapshot;
                        flag.store(true, Ordering::Relaxed);
                    }
                }
                std::thread::sleep(Duration::from_millis(120));
            }
            Message::Tick
        });

        // A repeating tick: re-arm after each delivery.
        let timer = context.set_timeout(UI_POLL, Message::Tick);

        Self {
            bridge,
            snapshot: Snapshot::default(),
            page: "console".to_string(),
            volume_drag: None,
            volume_at: None,
            brightness_sent: None,
            speed_sent: None,
            ambience_sent: None,
            pending_at: None,
            eq_sent: None,
            effect_sent: None,
            picker_color: None,
            color_draft: None,
            pending_volume: None,
            pending_brightness: false,
            pending_speed: false,
            pane_open: true,
            confirm: None,
            _timer: Some(timer),
        }
    }

    fn update(&mut self, message: Message, context: &ComponentContext<Self>) {
        let tick = matches!(message, Message::Tick);
        self.handle_message(message);
        if tick {
            self._timer = Some(context.set_timeout(UI_POLL, Message::Tick));
        }
    }

    fn view(&self, _input: &(), context: &mut ViewContext<Self>) -> View {
        context.window_title("EDIFIER Halo Soundbar");
        context.window_visuals(
            WindowVisuals::new()
                .theme(WindowTheme::System)
                .backdrop(WindowBackdrop::Mica)
                .client_size(1000.0, 700.0)
                // `icon()` wants a path, so the embedded .ico is materialised
                // once in the temp directory.
                .icon(app_icon_path().to_string_lossy().to_string()),
        );

        let content: View = match self.page.as_str() {
            "settings" => self.settings_view(context),
            _ => self.console_view(context),
        };

        let menu = vec![
            nav_item("console", "控制台", Symbol::Home, self.page == "console"),
            nav_item("settings", "设置", Symbol::Setting, self.page == "settings"),
        ];

        // `pane_display_mode` is deliberately left at `Left` and never changes.
        //
        // Switching `Left` <-> `LeftCompact` re-templates the control, and that
        // rebuild cannot be animated, which made closing the wide pane stutter
        // (opening looked fine because the pane is already laid out). Collapsing
        // is therefore done the way the gallery does it: keep the mode fixed and
        // only toggle `is_pane_open`, which WinUI animates as a width change.
        let navigation = NavigationView::new()
            .pane_display_mode(NavigationViewPaneDisplayMode::Left)
            // The title bar owns the only toggle, at the leading edge of the
            // window. Leaving the control's own button on would put a second,
            // duplicate hamburger at the top of the pane.
            .is_pane_toggle_button_visible(false)
            .is_back_button_visible(NavigationViewBackButtonVisible::Collapsed)
            .is_settings_visible(false)
            .always_show_header(false)
            // No `pane_title`: the app name is already in the title bar, and a
            // pane title is rendered as a second, redundant toggle target.
            .open_pane_length(PANE_WIDTH)
            .is_pane_open(self.pane_open)
            .on_is_pane_open_changed(context.callback(Message::PaneOpenChanged))
            .on_selected_tag_changed(context.callback(|tag: Option<std::rc::Rc<str>>| {
                Message::Navigate(tag.map(|t| t.to_string()).unwrap_or_default())
            }))
            .grid_row(1)
            .keyed_menu_items(menu)
            .content(content);

        // Connection and state are maintained automatically, so the title bar
        // only reports status — there is nothing for the user to refresh.
        //
        // The pane toggle belongs to the title bar, which places it at the
        // leading edge (top-left) the way WinUI expects. `Symbol::TitleBar` and
        // `windows_reactor::TitleBar` share a name, so the widget is imported
        // under an alias.
        let title_bar = TitleBarTrigger::new()
            .preferred_height(WindowTitleBarHeight::Tall)
            .title("EDIFIER Halo Soundbar")
            .subtitle(if self.page == "settings" {
                "设置"
            } else {
                "控制台"
            })
            .is_pane_toggle_button_visible(true)
            .on_pane_toggle_requested(context.message(Message::TogglePane))
            .right_header(TextBlock::new().text(self.connection_label()).opacity(0.8))
            .grid_row(0);

        Grid::new()
            .rows([GridLength::Auto, GridLength::Star(1.0)])
            .children((title_bar, navigation))
            // One dialog for the whole window. `content_dialog` stores a single
            // `Option`, so attaching a second one anywhere silently replaces the
            // first — which is why this lives here rather than per page.
            .content_dialog(self.confirm_dialog(context))
    }
}

fn nav_item(tag: &str, label: &str, icon: Symbol, selected: bool) -> KeyedView {
    KeyedView::new(
        tag.to_string(),
        NavigationViewItem::new()
            .tag(tag)
            .is_selected(selected)
            .selects_on_invoked(true)
            .icon(icon)
            .content(label),
    )
}

fn section(title: &str, body: View) -> View {
    StackPanel::new()
        .spacing(7.0)
        .children((TextBlock::new().text(title).font_size(15.0), body))
        .into()
}

fn row(children: impl IntoViews) -> View {
    StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(7.0)
        .children(children)
        .into()
}

impl App {
    fn console_view(&self, context: &mut ViewContext<Self>) -> View {
        let max = self.snapshot.state.max_volume.unwrap_or(16) as f64;
        let current = self
            .volume_drag
            .unwrap_or_else(|| self.snapshot.state.volume.unwrap_or(0) as f64);

        let volume = section(
            "音量",
            StackPanel::new()
                .spacing(5.0)
                .children((
                    TextBlock::new()
                        .text(format!("{} / {}", current as i64, max as i64))
                        .font_size(13.0),
                    Slider::new()
                        .minimum(0.0)
                        .maximum(max)
                        .step_frequency(1.0)
                        .value(current)
                        .is_enabled(self.snapshot.connected)
                        .on_value_changed(
                            context.callback(|value: f64| Message::VolumeChanged(value)),
                        ),
                ))
                .into(),
        );

        // `EqType.EQ_0E` scenes as cards.
        let selected_eq = self.selected_eq();
        let scenes: Vec<KeyedView> = ec_state::HALO_EQ_SCENES
            .iter()
            .map(|(index, name)| {
                let index = *index;
                let file = format!("big_eq_{}.png", eq_slug(index));
                grid_card(
                    name,
                    &file,
                    selected_eq == Some(index),
                    self.snapshot.connected,
                    context.callback(move |_: ()| Message::EqSelected(index)),
                )
            })
            .collect();

        let eq = section("音效模式", wrap_grid(scenes));

        // Everything below describes the effect the device is actually
        // driving, which `0x6A` reports in `payload[1]`.
        let active = self.active_effect();
        let record = self.effect_record(active);
        // The device reports the light table only while the effects are live;
        // a 1-byte `[00]` reply means it is holding no records, in which case
        // per-effect controls have nothing to describe.
        let table_live = ec_state::light_table_is_live(&self.snapshot.state);
        // The master switch gates everything else on this page.
        let switch_on = self.light_switch_on();
        let wire_speed = record.map(|r| r.speed).unwrap_or(255);
        let wire_brightness = record.map(|r| r.brightness).unwrap_or(255);
        let rate_on = table_live && switch_on && ec_state::halo_rate_settable(active, wire_speed);
        let bright_on =
            table_live && switch_on && ec_state::halo_brightness_settable(active, wire_brightness);
        // Slider positions come from the device record, so a change made on the
        // device itself or by another client moves them too.
        let brightness_display = self.brightness_display(active);
        let speed_display = self.speed_display(active);

        // `EA_08` effects as cards in a wrapping grid.
        let cards: Vec<KeyedView> = ec_state::HALO_EFFECTS
            .iter()
            .map(|(index, name, _, _)| {
                let index = *index;
                let file = format!("big_effect_{index}_{}.png", effect_slug(index));
                grid_card(
                    name,
                    &file,
                    index == active,
                    // With no live table the selection cannot be applied, so the
                    // card is disabled rather than silently swallowing the click.
                    table_live && switch_on && self.snapshot.connected,
                    context.callback(move |_: ()| Message::EffectSelected(index)),
                )
            })
            .collect();

        let brightness = section(
            "亮度",
            StackPanel::new()
                .spacing(5.0)
                .children((
                    TextBlock::new().text(if bright_on {
                        format!("亮度 {brightness_display}")
                    } else if !switch_on {
                        "氛围灯已关闭".to_string()
                    } else {
                        "该效果不支持调节亮度".to_string()
                    }),
                    Slider::new()
                        .minimum(ec_state::HALO_BRIGHTNESS_MIN as f64)
                        .maximum(ec_state::HALO_BRIGHTNESS_MAX as f64)
                        .step_frequency(1.0)
                        .value(brightness_display as f64)
                        .is_enabled(bright_on)
                        .on_value_changed(
                            context.callback(|value: f64| Message::BrightnessChanged(value as u8)),
                        ),
                ))
                .into(),
        );

        let speed = section(
            "速度",
            StackPanel::new()
                .spacing(5.0)
                .children((
                    TextBlock::new().text(if rate_on {
                        format!("速度 {speed_display}")
                    } else if !switch_on {
                        "氛围灯已关闭".to_string()
                    } else {
                        "该效果不支持调节速度".to_string()
                    }),
                    Slider::new()
                        .minimum(ec_state::HALO_BRIGHTNESS_MIN as f64)
                        .maximum(ec_state::HALO_BRIGHTNESS_MAX as f64)
                        .step_frequency(1.0)
                        .value(speed_display as f64)
                        .is_enabled(rate_on)
                        .on_value_changed(
                            context.callback(|value: f64| Message::SpeedChanged(value as u8)),
                        ),
                ))
                .into(),
        );

        // `EA_08` reserves effect index 8 for the official mood-light master
        // switch. `SoundBoxLightModel.set_device_light` puts the flag in the
        // record's `b` field: `[array_index, 8, 0, 0, on ? 1 : 0, 255, 255]`.
        //
        // Everything below depends on it, so the switch comes first and the
        // controls are disabled while it is off.
        // `EA_08` marks which effects take a custom colour: 1, 3, 4, 6 and 7 do,
        // while 2 (幻彩潮汐) and 5 (炫彩涟漪) run their own internal gradient.
        // `is_alpha_enabled(false)` because the device stores plain RGB — there
        // is no alpha channel on the wire.
        let rgb_on = table_live && switch_on && ec_state::halo_rgb_settable(active);
        let (pr, pg, pb) = self.color_display(active);
        let color = section(
            "颜色",
            StackPanel::new()
                .spacing(5.0)
                .children((
                    TextBlock::new().text(if rgb_on {
                        format!("#{pr:02X}{pg:02X}{pb:02X}")
                    } else if !switch_on {
                        "氛围灯已关闭".to_string()
                    } else {
                        "该效果使用自带配色，不支持自定义颜色".to_string()
                    }),
                    TextBlock::new()
                        .text("由于显示屏与音箱灯光的硬件差异，实际灯光颜色可能与屏幕显示存在差异，请以实际灯光效果为准。")
                        .text_wrapping(TextWrapping::Wrap)
                        .font_size(12.0)
                        .opacity(0.7),
                    ColorPicker::new()
                        .color(Color::rgb(pr, pg, pb))
                        .is_alpha_enabled(false)
                        .is_enabled(rgb_on)
                        .on_color_changed(
                            context.callback(|c: Color| Message::ColorChanged(c.r, c.g, c.b)),
                        ),
                    Button::new()
                        .content("应用此颜色")
                        .is_enabled(
                            rgb_on
                                && self.snapshot.connected
                                && self.color_draft.is_some_and(|(index, _)| index == active),
                        )
                        .on_click(context.callback(|_: ()| Message::ApplyColor)),
                ))
                .into(),
        );

        let master = section(
            "氛围灯",
            StackPanel::new()
                .spacing(5.0)
                .children((
                    ToggleSwitch::new()
                        .header("灯效开关")
                        .on_content("开")
                        .off_content("关")
                        .is_on(switch_on)
                        .is_enabled(self.snapshot.connected)
                        .on_toggled(context.callback(|on: bool| Message::AmbienceToggled(on))),
                    TextBlock::new()
                        .text(if switch_on {
                            "关闭后灯效与亮度、速度调节不可用"
                        } else {
                            "氛围灯已关闭，打开后可调节效果、亮度与速度"
                        })
                        .opacity(0.7),
                ))
                .into(),
        );

        page(
            "控制台",
            vec![
                volume,
                eq,
                master,
                section("灯光效果", wrap_grid(cards)),
                brightness,
                speed,
                color,
                section(
                    "重置",
                    row((Button::new()
                        .content("恢复默认灯效")
                        .is_enabled(switch_on)
                        .on_click(
                            context.message(Message::ConfirmRequested(ConfirmKind::ResetEffect)),
                        ),)),
                ),
            ],
        )
    }

    fn settings_view(&self, context: &mut ViewContext<Self>) -> View {
        // `product_ec_config.get_device_setting_by_device_product_id` puts Halo
        // in `ProductOptionPid.SOUND_BOX`, whose settings are `SI_01` (manual),
        // `SI_FF_FF` (authenticity) and `SI_07` (factory reset). Only the two
        // actions a user can actually perform are shown; the capability bitmap
        // and protocol details are diagnostics, not settings.
        // Disable device actions while disconnected; a dialog is also cancelled
        // if the connection disappears before it is confirmed.
        let can_power_off = self.snapshot.connected
            && self
                .snapshot
                .state
                .decoded_features
                .as_ref()
                .is_none_or(|f| f.device_off);
        let can_reset = self.snapshot.connected;

        let about = section(
            "版本",
            StackPanel::new()
                .spacing(4.0)
                .children((
                    TextBlock::new().text(format!("软件版本：{APP_VERSION}")),
                    TextBlock::new().text(match &self.snapshot.state.firmware {
                        Some(v) => format!("音箱固件：{v}"),
                        None => "音箱固件：—".to_string(),
                    }),
                ))
                .into(),
        );

        let reset = section(
            "恢复出厂设置",
            StackPanel::new()
                .spacing(7.0)
                .children((
                    TextBlock::new()
                        .text("设备会重启，所有设备端设置将被清除。此操作不可撤销。")
                        .opacity(0.7),
                    row((Button::new()
                        .content("恢复出厂设置")
                        .is_enabled(can_reset)
                        .on_click(
                            context.message(Message::ConfirmRequested(ConfirmKind::FactoryReset)),
                        ),)),
                ))
                .into(),
        );

        let mut sections = vec![about, reset];
        if can_power_off {
            sections.push(section(
                "关机",
                StackPanel::new()
                    .spacing(7.0)
                    .children((
                        TextBlock::new()
                            .text("关闭音箱，下次使用时按设备上的电源键开机。")
                            .opacity(0.7),
                        row((Button::new().content("关机").on_click(
                            context.message(Message::ConfirmRequested(ConfirmKind::Shutdown)),
                        ),)),
                    ))
                    .into(),
            ));
        }

        page("设置", sections)
    }
}

/// Card geometry for the wrapping selection grids.
const CARD_W: f64 = 112.0;
const CARD_H: f64 = 96.0;
const CARD_GAP: f64 = 8.0;

/// One selectable card: a large icon over its label, on a bordered tile.
///
/// The selected card is marked by the bold label, a brighter border and the
/// `_on` artwork — no accent fill, which washed out the line-art icons.
fn grid_card(
    label: &str,
    file: &str,
    selected: bool,
    enabled: bool,
    on_click: Callback<()>,
) -> KeyedView {
    // One artwork per card: the official `_on` variant is the dimmer of the two
    // (thin, low-contrast strokes), so the brighter `_off` drawing is used for
    // both states.
    let art: View = match icon_bytes(file) {
        Some(bytes) => Image::new()
            .source_data(EncodedImage::from_static(bytes))
            .width(40.0)
            .height(40.0)
            .stretch(Stretch::Uniform)
            .into(),
        // Only reachable if a name is mistyped; the label keeps the card usable.
        None => TextBlock::new().text(label).into(),
    };
    // Neither the artwork nor the text changes with selection — the accent ring
    // on the tile is the only state indicator.
    //
    // `horizontal_alignment` centres the glyphs *inside the element*, which is
    // not the same as centring the element: `TextBlock` defaults to
    // `TextAlignment::Left`, so a label narrower than its box sits against the
    // left edge. The sound-mode captions are two characters against a box sized
    // by the wider icons above them, which made them read as off-centre while
    // the four-character effect captions filled their box and looked fine.
    let caption = TextBlock::new()
        .text(label)
        .font_size(13.0)
        .horizontal_alignment(HorizontalAlignment::Center);
    let border = if selected {
        ThemeBrush::Accent
    } else {
        ThemeBrush::CardStroke
    };

    KeyedView::new(
        label,
        Button::new()
            .style(ButtonStyle::Subtle)
            .is_enabled(enabled)
            .width(CARD_W)
            .height(CARD_H)
            .margin(CARD_GAP / 2.0)
            .horizontal_content_alignment(HorizontalAlignment::Stretch)
            .vertical_content_alignment(VerticalAlignment::Stretch)
            .on_click(on_click)
            .content(
                Border::new()
                    .background(ThemeBrush::CardBackground)
                    .border_brush(border)
                    .border_thickness(if selected { 2.0 } else { 1.0 })
                    .corner_radius(8.0)
                    .padding(10.0)
                    .content(
                        StackPanel::new()
                            .spacing(5.0)
                            .horizontal_alignment(HorizontalAlignment::Center)
                            .vertical_alignment(VerticalAlignment::Center)
                            .children((art, caption)),
                    ),
            ),
    )
}

/// Lay cards out in rows that wrap to the available width.
fn wrap_grid(cards: Vec<KeyedView>) -> View {
    VariableSizedWrapGrid::new()
        .orientation(Orientation::Horizontal)
        .item_width(CARD_W + CARD_GAP)
        .item_height(CARD_H + CARD_GAP)
        .keyed_children(cards)
        .into()
}

/// Every card icon, compiled into the binary.
///
/// `cargo build` then produces a single self-contained executable — no
/// `assets/` directory to ship alongside it, and nothing to go missing when the
/// binary is moved. The whole set is ~38 KB.
const ICONS: [(&str, &[u8]); 10] = [
    (
        "big_effect_1_pure_static_light.png",
        include_bytes!("../assets/big_effect_1_pure_static_light.png"),
    ),
    (
        "big_effect_2_magic_tides.png",
        include_bytes!("../assets/big_effect_2_magic_tides.png"),
    ),
    (
        "big_effect_3_breath.png",
        include_bytes!("../assets/big_effect_3_breath.png"),
    ),
    (
        "big_effect_4_emotion_rhythm.png",
        include_bytes!("../assets/big_effect_4_emotion_rhythm.png"),
    ),
    (
        "big_effect_5_ripple.png",
        include_bytes!("../assets/big_effect_5_ripple.png"),
    ),
    (
        "big_effect_6_flowing_light.png",
        include_bytes!("../assets/big_effect_6_flowing_light.png"),
    ),
    (
        "big_effect_7_energy_stack.png",
        include_bytes!("../assets/big_effect_7_energy_stack.png"),
    ),
    (
        "big_eq_0_music.png",
        include_bytes!("../assets/big_eq_0_music.png"),
    ),
    (
        "big_eq_1_game.png",
        include_bytes!("../assets/big_eq_1_game.png"),
    ),
    (
        "big_eq_2_movie.png",
        include_bytes!("../assets/big_eq_2_movie.png"),
    ),
];

/// The independent soundbar icon, compiled in.
const APP_ICON: &[u8] = include_bytes!("../assets/app.ico");

/// Materialise the embedded window icon and return its path.
///
/// `WindowVisuals::icon` takes a path rather than image data, so the bytes are
/// written once into the temp directory.
fn app_icon_path() -> &'static std::path::Path {
    static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        APP_ICON.hash(&mut hash);
        // Content-specific names prevent an older build's cached icon from
        // taking precedence when the embedded artwork changes.
        let path = std::env::temp_dir().join(format!("halo-soundbar-{:016x}.ico", hash.finish()));
        if !path.exists() {
            let _ = std::fs::write(&path, APP_ICON);
        }
        path
    })
    .as_path()
}

/// Look up an embedded icon by file name.
fn icon_bytes(name: &str) -> Option<&'static [u8]> {
    ICONS
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, bytes)| *bytes)
}

/// Resource slug for effect `index`, matching the official drawable names.
fn effect_slug(index: u8) -> &'static str {
    match index {
        1 => "pure_static_light",
        2 => "magic_tides",
        3 => "breath",
        4 => "emotion_rhythm",
        5 => "ripple",
        6 => "flowing_light",
        7 => "energy_stack",
        _ => "pure_static_light",
    }
}

/// Resource slug for sound mode `index`, matching the official drawables
/// (`ic_newskin_scene_{music,game,movi}_{select,unselect}`).
fn eq_slug(index: u8) -> &'static str {
    match index {
        1 => "1_game",
        2 => "2_movie",
        _ => "0_music",
    }
}

/// Shared page chrome: a scrolled content column with just the page title.
fn page(title: &str, sections: Vec<View>) -> View {
    let mut children: Vec<View> = vec![TextBlock::new().text(title).font_size(21.0).into()];
    children.extend(sections);

    let body = StackPanel::new().spacing(18.0).children(children);

    ScrollViewer::new()
        .content(
            Border::new()
                .padding(Thickness::new(24.0, 16.0, 24.0, 24.0))
                .horizontal_alignment(HorizontalAlignment::Stretch)
                .content(body),
        )
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver};

    fn app() -> (App, Receiver<Command>) {
        let (tx, rx) = mpsc::channel();
        let mut snapshot = Snapshot {
            connected: true,
            ..Default::default()
        };
        snapshot.state.volume = Some(5);
        snapshot.state.max_volume = Some(16);
        snapshot.state.eq = Some(0);
        snapshot.state.light_group = Some(8);
        snapshot.state.light_index = Some(3);
        snapshot.state.light_groups = [1, 3]
            .into_iter()
            .map(|index| ec_state::LightGroup {
                light_array_index: 8,
                index,
                r: 20,
                g: 40,
                b: 60,
                brightness: 50,
                speed: if index == 3 { 5 } else { 255 },
            })
            .collect();
        let bridge = Arc::new(Bridge {
            commands: Mutex::new(Some(tx)),
            latest: Arc::new(Mutex::new(snapshot.clone())),
            dirty: Arc::new(AtomicBool::new(false)),
        });
        (
            App {
                bridge,
                snapshot,
                page: "console".into(),
                volume_drag: None,
                volume_at: None,
                brightness_sent: None,
                speed_sent: None,
                pane_open: true,
                confirm: None,
                pending_at: None,
                eq_sent: None,
                effect_sent: None,
                picker_color: None,
                color_draft: None,
                pending_volume: None,
                pending_brightness: false,
                pending_speed: false,
                ambience_sent: None,
                _timer: None,
            },
            rx,
        )
    }

    fn publish(app: &mut App, snapshot: Snapshot) {
        *app.bridge.latest.lock().unwrap() = snapshot;
        app.bridge.dirty.store(true, Ordering::Relaxed);
        app.handle_message(Message::Tick);
    }

    #[test]
    fn picking_color_survives_polling_without_writing_or_leaking_into_brightness() {
        let (mut app, rx) = app();
        app.handle_message(Message::ColorChanged(255, 0, 0));
        publish(&mut app, app_snapshot());
        assert!(rx.try_recv().is_err());
        assert_eq!(app.color_display(3), (255, 0, 0));
        app.handle_message(Message::BrightnessChanged(8));
        app.handle_message(Message::Tick);
        match rx.try_recv().unwrap() {
            Command::SetEffect {
                rgb, brightness, ..
            } => {
                assert_eq!(rgb, (20, 40, 60));
                assert_eq!(brightness, 80);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(app.color_display(3), (255, 0, 0));
    }

    fn app_snapshot() -> Snapshot {
        app().0.snapshot
    }

    #[test]
    fn applying_color_and_adjusting_sliders_preserves_all_recent_values() {
        let (mut app, rx) = app();
        app.handle_message(Message::BrightnessChanged(8));
        app.handle_message(Message::SpeedChanged(7));
        app.handle_message(Message::ColorChanged(255, 0, 0));
        app.handle_message(Message::ApplyColor);
        app.handle_message(Message::Tick);
        let writes: Vec<_> = rx.try_iter().collect();
        assert_eq!(writes.len(), 2);
        for command in writes {
            match command {
                Command::SetEffect {
                    index,
                    rgb,
                    brightness,
                    speed,
                    ..
                } => {
                    assert_eq!((index, rgb, brightness, speed), (3, (255, 0, 0), 80, 7));
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[test]
    fn two_sliders_flush_as_one_effect_write() {
        let (mut app, rx) = app();
        app.handle_message(Message::BrightnessChanged(9));
        app.handle_message(Message::SpeedChanged(2));
        app.handle_message(Message::Tick);
        assert!(matches!(
            rx.try_recv(),
            Ok(Command::SetEffect {
                brightness: 90,
                speed: 2,
                ..
            })
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn eq_waits_for_the_real_device_reading() {
        let (mut app, rx) = app();
        app.handle_message(Message::EqSelected(2));
        assert!(matches!(rx.try_recv(), Ok(Command::SetEq(2))));
        publish(&mut app, app_snapshot());
        assert_eq!(app.snapshot.state.eq, Some(0));
        assert_eq!(app.selected_eq(), Some(2));
        assert!(app.eq_sent.is_some());
        let mut agreed = app_snapshot();
        agreed.state.eq = Some(2);
        publish(&mut app, agreed);
        assert!(app.eq_sent.is_none());
    }

    #[test]
    fn optimistic_values_expire_even_without_a_new_snapshot() {
        let (mut app, _) = app();
        let old = Instant::now() - PENDING_TTL - Duration::from_secs(1);
        app.eq_sent = Some((2, old));
        app.volume_drag = Some(10.0);
        app.volume_at = Some(old);
        app.brightness_sent = Some(9);
        app.picker_color = Some((255, 0, 0));
        app.pending_at = Some(old);
        app.handle_message(Message::Tick);
        assert_eq!(app.selected_eq(), Some(0));
        assert_eq!(app.volume_drag, None);
        assert_eq!(app.brightness_display(3), 5);
        assert_eq!(app.color_display(3), (20, 40, 60));
    }

    #[test]
    fn changing_effect_discards_unflushed_edits_and_selects_immediately() {
        let (mut app, rx) = app();
        app.handle_message(Message::BrightnessChanged(9));
        app.handle_message(Message::ColorChanged(255, 0, 0));
        app.handle_message(Message::EffectSelected(1));
        app.handle_message(Message::Tick);
        assert_eq!(app.active_effect(), 1);
        assert_eq!(app.color_draft, None);
        assert!(matches!(
            rx.try_recv(),
            Ok(Command::SetEffect {
                index: 1,
                brightness: 50,
                ..
            })
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn externally_changing_effect_discards_the_previous_applied_color() {
        let (mut app, _) = app();
        app.picker_color = Some((255, 0, 0));
        let mut changed = app_snapshot();
        changed.state.light_index = Some(1);
        publish(&mut app, changed);
        assert_eq!(app.color_display(1), (20, 40, 60));
    }

    #[test]
    fn switching_off_cancels_unflushed_light_writes() {
        let (mut app, rx) = app();
        app.handle_message(Message::BrightnessChanged(9));
        app.handle_message(Message::AmbienceToggled(false));
        assert!(!app.light_switch_on());
        app.handle_message(Message::Tick);
        assert!(matches!(
            rx.try_recv(),
            Ok(Command::SetLightSwitch { on: false, .. })
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn disconnect_cancels_pending_commands_and_destructive_dialogs() {
        let (mut app, rx) = app();
        app.handle_message(Message::VolumeChanged(10.0));
        app.handle_message(Message::BrightnessChanged(9));
        app.handle_message(Message::ColorChanged(255, 0, 0));
        app.handle_message(Message::ConfirmRequested(ConfirmKind::FactoryReset));
        publish(&mut app, Snapshot::default());
        app.handle_message(Message::Confirmed(ContentDialogResult::Primary));
        app.handle_message(Message::VolumeChanged(6.0));
        app.handle_message(Message::AmbienceToggled(true));
        app.handle_message(Message::Tick);
        assert!(rx.try_recv().is_err());
        assert!(app.confirm.is_none());
        assert!(app.color_draft.is_none());
        assert!(app.volume_drag.is_none());
    }

    #[test]
    fn cancelled_reset_does_not_write() {
        let (mut app, rx) = app();
        app.handle_message(Message::ConfirmRequested(ConfirmKind::FactoryReset));
        app.handle_message(Message::Confirmed(ContentDialogResult::None));
        assert!(app.confirm.is_none());
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn navigating_before_the_next_tick_keeps_the_last_slider_value() {
        let (mut app, rx) = app();
        app.handle_message(Message::VolumeChanged(9.0));
        app.handle_message(Message::BrightnessChanged(8));
        app.handle_message(Message::Navigate("settings".into()));
        assert!(matches!(rx.try_recv(), Ok(Command::SetVolume(9))));
        assert!(matches!(
            rx.try_recv(),
            Ok(Command::SetEffect { brightness: 80, .. })
        ));
        app.handle_message(Message::Tick);
        assert!(rx.try_recv().is_err());
    }
}
