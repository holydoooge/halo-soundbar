//! EC device state: run the soundbar's byte-protocol queries and decode them.
//!
//! Every decoder mirrors an `*_parse` function in
//! `interface/client/ec_parse.py`; the command bytes and request shapes mirror
//! `driver/edifier_driver.py` (`ECDriver`).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ec::{self, cmd};
use crate::transport::{EcTransport, Link};

/// One light record decoded from a `0x6A` reply.
///
/// `interface/client/ec_parse.py::mood_lighting_parse` reads the two header
/// bytes `[light_array_index, current_light_index]` and then walks the rest in
/// fixed **6-byte records**:
///
/// ```text
/// [index, R, G, B, Brightness, Speed]
/// ```
///
/// The walk starts at offset 2 for `0x6A`/`0x72` and at 1 for other commands.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LightGroup {
    /// `payload[0]` — the effect array this device uses (Halo: `8`).
    pub light_array_index: u8,
    /// The effect id, e.g. 1..7 for `EffectArrayType.EA_08`.
    pub index: u8,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    /// Wire brightness. Halo uses 1..10 in the UI, multiplied by 10 on the wire;
    /// 255 is the model's "not adjustable" sentinel.
    pub brightness: u8,
    /// Wire speed. 255 is the "not adjustable" sentinel.
    pub speed: u8,
}

/// Decode a mood-lighting reply using the client's own walk.
pub fn parse_light_table(command: u8, payload: &[u8]) -> Vec<LightGroup> {
    let mut out: Vec<LightGroup> = Vec::new();
    if payload.len() < 2 {
        return out;
    }
    let array_index = payload[0];
    let start = if command == cmd::MOOD_LIGHT_QUERY || command == cmd::MOOD_LIGHT_RESET {
        2
    } else {
        1
    };
    if payload.len() <= start {
        return out;
    }
    for chunk in payload[start..].chunks(6) {
        if chunk.len() < 6 {
            break;
        }
        out.push(LightGroup {
            light_array_index: array_index,
            index: chunk[0],
            r: chunk[1],
            g: chunk[2],
            b: chunk[3],
            brightness: chunk[4],
            speed: chunk[5],
        });
    }
    out
}

/// `payload[1]` of a `0x6A` reply: the effect the device is currently driving.
pub fn current_light_index(payload: &[u8]) -> Option<u8> {
    payload.get(1).copied()
}

/// Apply a complete Halo table or its explicit off reply. A malformed or
/// truncated response must not turn the lights off in the UI.
pub fn update_light_state(state: &mut EcState, payload: Vec<u8>) -> bool {
    if payload == [0] {
        state.light_groups.clear();
        state.light_raw = Some(payload);
        return true;
    }
    if payload.len() < 8
        || !(payload.len() - 2).is_multiple_of(6)
        || payload[0] != 8
        || !(1..=8).contains(&payload[1])
    {
        return false;
    }
    let groups = parse_light_table(cmd::MOOD_LIGHT_QUERY, &payload);
    if groups.iter().any(|g| !(1..=8).contains(&g.index)) {
        return false;
    }
    state.light_group = Some(payload[0] as i64);
    state.light_index = Some(payload[1] as i64);
    state.light_groups = groups;
    state.light_raw = Some(payload);
    true
}

/// A decoded device snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EcState {
    /// `volume_query_parse`: payload[0] = max, payload[1] = current.
    pub volume: Option<i64>,
    pub max_volume: Option<i64>,
    /// `query_eq_parse`: payload[0].
    pub eq: Option<i64>,
    /// `battery_query_parse`: payload[0].
    pub battery: Option<i64>,
    /// `device_status_query_parse`: Peer/LB/RB/DB/DC/ED.
    pub status: Option<[i64; 6]>,
    /// `mood_lighting_query_parse`: payload[0] group, payload[1] light index.
    pub light_group: Option<i64>,
    pub light_index: Option<i64>,
    /// `0x6A` light table, decoded.
    pub light_groups: Vec<LightGroup>,
    /// Raw payload of the last `0x6A` answer.
    pub light_raw: Option<Vec<u8>>,
    /// `bass_query_parse`: payload[0].
    pub bass: Option<i64>,
    /// Firmware version, dotted (`1.0.2`).
    pub firmware: Option<String>,
    /// Raw payload of the device-function bitmap query (`0xD8`).
    pub features: Option<Vec<u8>>,
    /// `0xD8` decoded with `D8Bean.set_payload`'s rules.
    pub decoded_features: Option<DeviceFeatures>,
    /// Base-function handshake payload.
    pub base: Option<Vec<u8>>,
}

impl EcState {
    pub fn is_empty(&self) -> bool {
        self.volume.is_none()
            && self.eq.is_none()
            && self.battery.is_none()
            && self.status.is_none()
            && self.light_index.is_none()
            && self.features.is_none()
    }

    pub fn feature_count(&self) -> usize {
        self.features
            .as_ref()
            .map(|f| f.iter().filter(|b| **b != 0).count())
            .unwrap_or(0)
    }
}

/// Send one EC request and wait for the reply carrying `command`.
pub fn query<T: EcTransport>(
    link: &mut Link<T>,
    command: u8,
    payload: &[u8],
    wait: Duration,
) -> Option<Vec<u8>> {
    // Request/response matching lives in Link, so every transport shares it.
    link.query(command, payload, wait)
}

/// Read everything the three tabs need.
pub fn read_state<T: EcTransport>(link: &mut Link<T>) -> EcState {
    read_state_into(link, EcState::default())
}

/// Refresh `state` in place, keeping whatever the device did not answer.
///
/// `read_state` starts from a blank state, so pushing its result straight into
/// the snapshot wiped every field whenever the device went quiet for one round
/// — a real occurrence, because the command buffer is small. Starting from the
/// previous state means a silent round preserves the last known values instead
/// of blanking the UI.
pub fn read_state_into<T: EcTransport>(link: &mut Link<T>, mut state: EcState) -> EcState {
    let wait = Duration::from_millis(700);

    let _ = link.drain();

    // Base function handshake.
    if let Some(payload) = query(link, cmd::BASE_FUNCTION, &[], wait) {
        state.base = Some(payload);
    }
    // Firmware version (`0xC6`).
    if let Some(payload) = query(link, cmd::VERSION, &[], wait) {
        if let Some(version) = parse_version(&payload) {
            state.firmware = Some(version);
        }
    }
    // Supported-feature bitmap (`0xD8`), decoded with `D8Bean`'s rules.
    if let Some(payload) = query(link, cmd::DEVICE_FUNCTION, &[], wait) {
        state.decoded_features = Some(DeviceFeatures::parse(&payload));
        state.features = Some(payload);
    }
    // Volume: payload[0] = max, payload[1] = current.
    if let Some(payload) = query(link, cmd::VOLUME_QUERY, &[], wait) {
        if payload.len() >= 2 {
            state.max_volume = Some(payload[0] as i64);
            state.volume = Some(payload[1] as i64);
        } else if payload.len() == 1 {
            state.volume = Some(payload[0] as i64);
        }
    }
    // Equaliser scene: `0xD5` is the query; `payload[0]` is the scene index.
    if let Some(payload) = query(link, cmd::EQ_QUERY, &[], wait) {
        if let Some(scene) = payload.first() {
            state.eq = Some(*scene as i64);
        }
    }
    if let Some(payload) = query(link, cmd::DEVICE_STATUS, &[], wait) {
        if payload.len() >= 6 {
            state.status = Some([
                payload[0] as i64,
                payload[1] as i64,
                payload[2] as i64,
                payload[3] as i64,
                payload[4] as i64,
                payload[5] as i64,
            ]);
        }
    }
    // Mood lighting: `0x6A` answers `[light_array_index, current_light_index]`
    // followed by 6-byte records.
    if let Some(payload) = query(link, cmd::MOOD_LIGHT_QUERY, &[], wait) {
        update_light_state(&mut state, payload);
    }

    state
}

/// Set the volume (`volume_set`).
pub fn set_volume<T: EcTransport>(link: &mut Link<T>, level: u8) -> Result<(), String> {
    let frame = ec::build_primary(cmd::VOLUME_SET, &[level]);
    link.send_frame(&frame)
}

/// Select an equaliser scene (`ECDriver.eq_set` -> `CM_C4` with `[index]`).
///
/// The query side is `CM_D5` (`query_eq_parse` reads `payload[0]`). Halo reports
/// `EqType.EQ_0E` in byte 13 of its `0xD8` bitmap, which exposes exactly three
/// scenes: 音乐 / 游戏 / 电影.
pub fn set_eq<T: EcTransport>(link: &mut Link<T>, index: u8) -> Result<(), String> {
    link.send_frame(&ec::build_primary(cmd::EQ_SET, &[index]))
}

/// The scenes Halo exposes, from `EqType.EQ_0E` in the current install's
/// `product_ec_config`.
pub const HALO_EQ_SCENES: [(u8, &str); 3] = [(0, "音乐"), (1, "游戏"), (2, "电影")];

/// First `EA_08` effect index, used when the device reports nothing usable.
pub const HALO_FIRST_EFFECT: u8 = 1;

/// Whether the device's `0xD8` bitmap selects the three-scene EQ model.
///
/// Byte 13 is `0x0E` for `EqType.EQ_0E` on the observed Halo unit.
pub fn halo_uses_three_scene_eq(features: &[u8]) -> bool {
    features.get(13) == Some(&0x0E)
}

/// Build a mood-lighting payload exactly like `mood_lighting_splice` in
/// `interface/client/ec_splice.py`.
///
/// The dictionary path emits **seven** bytes:
///
/// ```text
/// [light_array_index, index, R, G, B, Brightness, Speed]
/// ```
///
/// The eighth `mode` byte is only appended when the caller passes it.
pub fn mood_lighting_splice(
    array_index: u8,
    index: u8,
    r: u8,
    g: u8,
    b: u8,
    brightness: u8,
    speed: u8,
) -> Vec<u8> {
    vec![array_index, index, r, g, b, brightness, speed]
}

/// The seven-field payload plus the optional `mode` byte.
#[allow(clippy::too_many_arguments)]
pub fn mood_lighting_splice_mode(
    array_index: u8,
    index: u8,
    r: u8,
    g: u8,
    b: u8,
    brightness: u8,
    speed: u8,
    mode: u8,
) -> Vec<u8> {
    let mut payload = mood_lighting_splice(array_index, index, r, g, b, brightness, speed);
    payload.push(mode);
    payload
}

/// Send a mood-lighting payload (`mood_lighting_set` -> `CM_6B`).
pub fn set_mood_light<T: EcTransport>(
    link: &mut Link<T>,
    array_index: u8,
    index: u8,
    rgb: (u8, u8, u8),
    brightness: u8,
    speed: u8,
) -> Result<(), String> {
    let payload = mood_lighting_splice(array_index, index, rgb.0, rgb.1, rgb.2, brightness, speed);
    link.send_frame(&ec::build_primary(cmd::MOOD_LIGHT, &payload))
}

/// Restore one effect to its default (`SoundBoxLightModel.reset_light`):
/// `CM_72` with `[array_index, current_index]`.
pub fn reset_mood_light<T: EcTransport>(
    link: &mut Link<T>,
    array_index: u8,
    index: u8,
) -> Result<(), String> {
    link.send_frame(&ec::build_primary(
        cmd::MOOD_LIGHT_RESET,
        &[array_index, index],
    ))
}

/// `EA_08` reserves effect index 8 for the mood-light master switch.
///
/// The on/off state lives in the record's **`b`** field: `0x6A` reports the
/// switch as `[8, 8, 0, 0, b, 255, 255]` with `b == 1` meaning on, and the
/// `0x6B` write puts the flag in the same slot. An earlier revision put it in
/// `speed`, which turns the light **off** (because `b` becomes 0) but can never
/// turn it back on.
pub const HALO_LIGHT_SWITCH_INDEX: u8 = 8;

/// Build the master-switch payload:
/// `[array_index, 8, 0, 0, on ? 1 : 0, 255, 255]`.
pub fn light_switch_payload(array_index: u8, on: bool) -> Vec<u8> {
    mood_lighting_splice(
        array_index,
        HALO_LIGHT_SWITCH_INDEX,
        0,
        0,
        u8::from(on),
        255,
        255,
    )
}

/// Send the mood-light master switch (`0x6B`).
///
/// The payload is correct and the device does apply it, but confirmation is
/// unreliable: measured on hardware, roughly half of a 10-write alternating run
/// read back the *previous* switch state even after a 1.5 s quiet wait, and
/// rewriting the same value is what makes it stick. Callers should therefore
/// verify and retry — see `device::apply_light_switch`.
///
/// `0x72` is **not** a usable recovery here: it is a toggle, so issuing it
/// alongside a switch-on inverted the switch on the second round trip.
pub fn set_light_switch<T: EcTransport>(
    link: &mut Link<T>,
    array_index: u8,
    on: bool,
) -> Result<(), String> {
    let payload = light_switch_payload(array_index, on);
    link.send_frame(&ec::build_primary(cmd::MOOD_LIGHT, &payload))
}

/// Read the master switch position. `[00]` explicitly means off; `None` means
/// the query timed out or did not contain a switch reading.
pub fn read_light_switch<T: EcTransport>(link: &mut Link<T>) -> Option<bool> {
    let payload = query(link, cmd::MOOD_LIGHT_QUERY, &[], Duration::from_millis(700))?;
    if payload == [0] {
        return Some(false);
    }
    let groups = parse_light_table(cmd::MOOD_LIGHT_QUERY, &payload);
    groups
        .iter()
        .find(|g| g.index == HALO_LIGHT_SWITCH_INDEX)
        .map(|g| g.b == 1)
}

/// `0xD8` feature bitmap, decoded the way `interface/client/ec_brean.py`'s
/// `D8Bean.set_payload` does.
///
/// The payload is a byte table, not a bitfield: most entries are a single flag
/// read with `b0[i]`, where `b0` comes from `format(byte, '08b')` — so index 0
/// is the **most** significant bit and 7 the least. A few entries carry a value
/// instead (byte 13 is the `EqType`, byte 3 the name length limit, byte 9 the
/// auto-off time).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceFeatures {
    pub raw: Vec<u8>,
    /// byte 1 — `bat`, also gates the battery readout.
    pub battery: u8,
    pub show_battery: bool,
    pub query_name: bool,
    /// byte 3 — `num_support_set_name`, the longest settable name.
    pub name_limit: u8,
    pub query_mac: bool,
    pub query_version: bool,
    pub disconnect_a2dp: bool,
    pub repair: bool,
    pub auto_shutdown_without_audio: bool,
    /// byte 9 — `time_off`.
    pub time_off: u8,
    pub device_off: bool,
    pub show_manual: bool,
    /// byte 13 — the `EqType` value. Halo reports `0x0E` (`EQ_0E`).
    pub eq_type: u8,
    pub skin: bool,
    pub tap: bool,
}

/// Whether the device is currently holding a live light table.
///
/// `0x6A` answers a bare `[00]` when the master switch is off, which is how the
/// switch position is actually observable: there is no separate on/off query.
pub fn light_table_is_live(state: &EcState) -> bool {
    !state.light_groups.is_empty()
}

/// Render a version payload the way `version_query_parse` does: every byte is a
/// component, joined with `.`.
pub fn parse_version(payload: &[u8]) -> Option<String> {
    if payload.is_empty() {
        return None;
    }
    Some(
        payload
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join("."),
    )
}

/// Read bit `index` the way `D8Bean.byte_to_bitarray` orders bits:
/// `format(byte, '08b')[index]`, i.e. MSB first.
pub fn bitmap_bit(byte: u8, index: u8) -> bool {
    index < 8 && (byte >> (7 - index)) & 1 == 1
}

impl DeviceFeatures {
    pub fn parse(payload: &[u8]) -> Self {
        let byte = |i: usize| payload.get(i).copied().unwrap_or(0);
        Self {
            raw: payload.to_vec(),
            battery: byte(1),
            show_battery: byte(1) != 0,
            query_name: bitmap_bit(byte(2), 7),
            name_limit: byte(3),
            query_mac: bitmap_bit(byte(4), 7),
            query_version: bitmap_bit(byte(5), 7),
            disconnect_a2dp: bitmap_bit(byte(6), 7),
            repair: bitmap_bit(byte(7), 0),
            auto_shutdown_without_audio: bitmap_bit(byte(8), 0),
            time_off: byte(9),
            device_off: bitmap_bit(byte(10), 7),
            show_manual: bitmap_bit(byte(11), 7),
            eq_type: byte(13),
            skin: bitmap_bit(byte(14), 4),
            tap: bitmap_bit(byte(16), 0),
        }
    }

    /// `EqType.EQ_0E` — the three-scene model (音乐 / 游戏 / 电影).
    pub const EQ_TYPE_0E: u8 = 0x0E;
}

/// Halo's `EffectArrayType.EA_08` brightness scale.
pub const HALO_BRIGHTNESS_MIN: u8 = 1;
pub const HALO_BRIGHTNESS_MAX: u8 = 10;

/// UI 1..10 -> wire value (×10).
pub fn brightness_wire_from_display(value: u8) -> u8 {
    value.clamp(HALO_BRIGHTNESS_MIN, HALO_BRIGHTNESS_MAX) * 10
}

/// Wire value -> UI 1..10.
pub fn brightness_display_from_wire(value: u8) -> u8 {
    ((value as u16 + 5) / 10).clamp(HALO_BRIGHTNESS_MIN as u16, HALO_BRIGHTNESS_MAX as u16) as u8
}

/// Speed is **not** scaled the way brightness is.
///
/// The device stores brightness as UI × 10 (0..100 — the `brightness_magnification`
/// of `EA_08`) but speed as a plain 1..10 value. Applying the brightness formula
/// to speed renders every rate as `1`, which is exactly the bug that made the
/// speed slider look frozen.
pub fn speed_display_from_wire(value: u8) -> u8 {
    if value == 255 || value == 0 {
        // 255 is the "not adjustable" sentinel; 0 means the effect does not run.
        return HALO_BRIGHTNESS_MIN;
    }
    value.clamp(HALO_BRIGHTNESS_MIN, HALO_BRIGHTNESS_MAX)
}

/// UI 1..10 -> wire speed (identity).
pub fn speed_wire_from_display(value: u8) -> u8 {
    value.clamp(HALO_BRIGHTNESS_MIN, HALO_BRIGHTNESS_MAX)
}

/// `EA_08` offers the rate control only for effects 2 and 3; 255 is the
/// "not adjustable" sentinel.
pub fn halo_rate_settable(effect_index: u8, wire_speed: u8) -> bool {
    matches!(effect_index, 2 | 3) && wire_speed != 255
}

/// `EA_08` disables brightness for effect 4; 255 is the sentinel.
pub fn halo_brightness_settable(effect_index: u8, wire_brightness: u8) -> bool {
    matches!(effect_index, 1 | 2 | 3 | 5 | 6 | 7) && wire_brightness != 255
}

/// Whether the effect accepts a custom RGB colour.
///
/// Read straight from `product_ec_config.get_light_effect_by_device_index`'s
/// `EA_08` block: `rgb_settable` is `True` for effects 1, 3, 4, 6, 7 and
/// `False` for 2 (幻彩潮汐) and 5 (炫彩涟漪) — those two run their own internal
/// gradient, so the colour control must be disabled for them.
pub fn halo_rgb_settable(effect_index: u8) -> bool {
    matches!(effect_index, 1 | 3 | 4 | 6 | 7)
}

/// The seven `EA_08` effects: `(index, name, rate_settable, brightness_settable)`.
///
/// Names and flags come from the `EA_08` block of
/// `app/bean/product_ec_config.py` in the shipped client.
pub const HALO_EFFECTS: [(u8, &str, bool, bool); 7] = [
    (1, "纯色静光", false, true),
    (2, "幻彩潮汐", true, true),
    (3, "氛围呼吸", true, true),
    (4, "声光律动", false, false),
    (5, "炫彩涟漪", false, true),
    (6, "纯光回旋", false, true),
    (7, "辉光渐展", false, true),
];

/// Factory reset (`device_reset`).
pub fn factory_reset<T: EcTransport>(link: &mut Link<T>) -> Result<(), String> {
    let frame = ec::build_primary(cmd::FACTORY_RESET, &[]);
    link.send_frame(&frame)
}

/// Power off (`shut_down`).
pub fn shutdown<T: EcTransport>(link: &mut Link<T>) -> Result<(), String> {
    let frame = ec::build_primary(cmd::SHUTDOWN, &[]);
    link.send_frame(&frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The volume *parser* is what needs guarding, so drive a real transport
    /// through `read_state` instead of re-asserting the byte layout.
    #[test]
    fn volume_layout_matches_the_parser() {
        use crate::transport::testing::FakeTransport;
        let mut link = Link::new(FakeTransport::new());
        // `read_state` clears the link first, so the reply is queued after it
        // exists. 0x66 answers [max, current].
        link.transport_mut()
            .push_response(cmd::VOLUME_QUERY, &[0x10, 0x0A]);
        let state = read_state(&mut link);
        assert_eq!(state.max_volume, Some(16));
        assert_eq!(state.volume, Some(10));
    }

    #[test]
    fn volume_layout_documented_offsets() {
        // volume_query_parse reads payload[0] as max and payload[1] as current.
        let payload = [0x10u8, 0x0A];
        assert_eq!(payload[0] as i64, 16);
        assert_eq!(payload[1] as i64, 10);
    }

    #[test]
    fn mood_lighting_splice_matches_the_client_order() {
        // The dictionary path emits seven bytes; `mode` is appended only when
        // the caller supplies it.
        let payload = mood_lighting_splice(0, 2, 255, 0, 0, 128, 64);
        assert_eq!(payload, vec![0, 2, 255, 0, 0, 128, 64]);
        let with_mode = mood_lighting_splice_mode(0, 2, 255, 0, 0, 128, 64, 1);
        assert_eq!(with_mode, vec![0, 2, 255, 0, 0, 128, 64, 1]);
    }

    #[test]
    fn light_table_uses_the_clients_walk() {
        // Hardware sample from CM_6A (0x6A): two header bytes, then 6-byte
        // records of [index, R, G, B, Brightness, Speed].
        let payload = [
            0x08, 0x01, //
            0x01, 0xFF, 0xE6, 0xFE, 0x64, 0xFF, //
            0x02, 0x01, 0x00, 0x00, 0x64, 0x06,
        ];
        let groups = parse_light_table(cmd::MOOD_LIGHT_QUERY, &payload);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].light_array_index, 0x08);
        assert_eq!(groups[0].index, 0x01);
        assert_eq!(groups[0].r, 0xFF);
        assert_eq!(groups[0].g, 0xE6);
        assert_eq!(groups[0].b, 0xFE);
        assert_eq!(groups[0].brightness, 0x64);
        assert_eq!(groups[0].speed, 0xFF);
        assert_eq!(groups[1].index, 0x02);
        assert_eq!(groups[1].r, 0x01);
        assert_eq!(groups[1].brightness, 0x64);
        assert_eq!(current_light_index(&payload), Some(0x01));
    }

    #[test]
    fn halo_brightness_scale_round_trips() {
        assert_eq!(brightness_wire_from_display(7), 70);
        assert_eq!(brightness_display_from_wire(70), 7);
        assert_eq!(brightness_wire_from_display(0), 10);
        assert_eq!(brightness_wire_from_display(99), 100);
    }

    #[test]
    fn speed_is_not_scaled_like_brightness() {
        // Brightness: UI x10 on the wire. Speed: plain 1..10.
        assert_eq!(brightness_wire_from_display(7), 70);
        assert_eq!(brightness_display_from_wire(70), 7);

        assert_eq!(speed_display_from_wire(8), 8, "wire 8 is speed 8, not 1");
        assert_eq!(speed_display_from_wire(10), 10);
        assert_eq!(speed_display_from_wire(1), 1);
        // 255 is the "not adjustable" sentinel.
        assert_eq!(speed_display_from_wire(255), HALO_BRIGHTNESS_MIN);
        assert_eq!(speed_display_from_wire(0), HALO_BRIGHTNESS_MIN);
        assert_eq!(speed_wire_from_display(8), 8);
        assert_eq!(speed_wire_from_display(0), HALO_BRIGHTNESS_MIN);
    }

    #[test]
    fn halo_availability_rules_match_ea08() {
        assert!(!halo_rate_settable(1, 10));
        assert!(halo_rate_settable(2, 10));
        assert!(halo_rate_settable(3, 10));
        assert!(!halo_rate_settable(4, 10));
        assert!(!halo_rate_settable(3, 255));
        assert!(halo_brightness_settable(1, 100));
        assert!(!halo_brightness_settable(4, 100));
        assert!(!halo_brightness_settable(3, 255));
    }

    /// Flags taken verbatim from the `EA_08` block of
    /// `product_ec_config.get_light_effect_by_device_index`.
    #[test]
    fn halo_rgb_settable_matches_ea08() {
        // True for 1, 3, 4, 6, 7.
        assert!(halo_rgb_settable(1));
        assert!(!halo_rgb_settable(2)); // 幻彩潮汐
        assert!(halo_rgb_settable(3));
        assert!(halo_rgb_settable(4));
        assert!(!halo_rgb_settable(5)); // 炫彩涟漪
        assert!(halo_rgb_settable(6));
        assert!(halo_rgb_settable(7));
        // The switch record is not an effect.
        assert!(!halo_rgb_settable(HALO_LIGHT_SWITCH_INDEX));
    }

    #[test]
    fn halo_effect_table_matches_ea08() {
        let flags: Vec<(u8, bool, bool)> = HALO_EFFECTS
            .iter()
            .map(|(i, _, rate, bright)| (*i, *rate, *bright))
            .collect();
        assert_eq!(
            flags,
            vec![
                (1, false, true),
                (2, true, true),
                (3, true, true),
                (4, false, false),
                (5, false, true),
                (6, false, true),
                (7, false, true),
            ]
        );
    }

    #[test]
    fn bitmap_bits_are_msb_first() {
        // byte_to_bitarray uses format(x, '08b'), so index 0 is the top bit.
        assert!(bitmap_bit(0b1000_0000, 0));
        assert!(!bitmap_bit(0b1000_0000, 7));
        assert!(bitmap_bit(0b0000_0001, 7));
        assert!(!bitmap_bit(0b0000_0000, 0));
    }

    /// Byte table of the connected Halo, captured from `0xD8`.
    const HALO_D8: [u8; 22] = [
        0x00, 0x00, 0x01, 0x02, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01, 0x0A, 0x0E, 0x0A,
        0x00, 0x00, 0x20, 0x00, 0x83, 0x00, 0x20,
    ];

    #[test]
    fn version_parses_like_the_client() {
        // version_query_parse joins one byte per component with '.'.
        assert_eq!(parse_version(&[1, 0, 2]).as_deref(), Some("1.0.2"));
        assert_eq!(parse_version(&[2, 5]).as_deref(), Some("2.5"));
        assert_eq!(parse_version(&[]), None);
    }

    #[test]
    fn halo_feature_bitmap_decodes() {
        let f = DeviceFeatures::parse(&HALO_D8);
        assert!(!f.show_battery, "battery readout is off (bat == 0)");
        assert!(f.query_name);
        assert_eq!(f.name_limit, 2, "byte 3 carries the limit, not a flag");
        assert!(f.query_mac);
        assert!(f.query_version);
        assert!(f.disconnect_a2dp);
        assert!(!f.repair);
        assert!(!f.auto_shutdown_without_audio);
        assert_eq!(f.time_off, 0x00, "byte 9 is the auto-off time, here 0");
        assert!(f.device_off);
        assert!(f.show_manual);
        assert_eq!(f.eq_type, DeviceFeatures::EQ_TYPE_0E);
        assert!(f.skin);
        assert!(!f.tap);
    }

    #[test]
    fn halo_selects_the_three_scene_eq() {
        // Byte 13 of the observed 0xD8 reply is 0x0E (EqType.EQ_0E).
        let features = [
            0x00, 0x00, 0x01, 0x02, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01, 0x0A, 0x0E,
            0x0A,
        ];
        assert!(halo_uses_three_scene_eq(&features));
        assert_eq!(HALO_EQ_SCENES.len(), 3);
    }

    #[test]
    fn empty_state_is_detected() {
        assert!(EcState::default().is_empty());
        let state = EcState {
            volume: Some(5),
            ..Default::default()
        };
        assert!(!state.is_empty());
    }
    #[test]
    fn explicit_off_reply_is_distinct_from_a_query_timeout() {
        use crate::transport::testing::FakeTransport;
        let mut link = Link::new(FakeTransport::new());
        link.transport_mut()
            .push_response(cmd::MOOD_LIGHT_QUERY, &[0]);
        assert_eq!(read_light_switch(&mut link), Some(false));
        assert_eq!(read_light_switch(&mut link), None);
    }

    #[test]
    fn full_refresh_accepts_off_and_preserves_fields_for_empty_replies() {
        use crate::transport::testing::FakeTransport;
        let mut link = Link::new(FakeTransport::new());
        for command in [
            cmd::BASE_FUNCTION,
            cmd::VERSION,
            cmd::DEVICE_FUNCTION,
            cmd::VOLUME_QUERY,
            cmd::EQ_QUERY,
            cmd::DEVICE_STATUS,
        ] {
            link.transport_mut().push_response(command, &[]);
        }
        link.transport_mut()
            .push_response(cmd::MOOD_LIGHT_QUERY, &[0]);
        let mut state = EcState {
            firmware: Some("1.0.2".into()),
            eq: Some(1),
            ..Default::default()
        };
        update_light_state(&mut state, vec![8, 1, 1, 20, 40, 60, 50, 255]);
        let result = read_state_into(&mut link, state);
        assert_eq!(result.firmware.as_deref(), Some("1.0.2"));
        assert_eq!(result.eq, Some(1));
        assert_eq!(result.light_group, Some(8));
        assert!(!light_table_is_live(&result));
    }
}
