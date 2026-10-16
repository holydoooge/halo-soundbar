//! Wire protocol for the EDIFIER "home" device family.
//!
//! Recovered from `driver/edifier_home_driver.py`:
//!
//! ```text
//! COMMON      = 0x2E
//! HEADER      = 0xAA
//! DEVICE_TYPE = 0xEC
//! MAX_READ_LENGTH = MAX_SEND_LENGTH = 64
//!
//! frame = [COMMON, HEADER, DEVICE_TYPE] + [index] + add_len(len(payload)) + payload
//! crc   = sum(frame[1:]) & 0xFF
//! ```
//!
//! `add_len` is big endian, so byte 4 is the high byte and byte 5 the low byte.
//! The reply is parsed by `CmdReport`: byte 0 is the start marker, byte 1 the
//! header, byte 2 the code, byte 3 the command index, bytes 4..5 the payload
//! length and the last payload byte position holds the CRC.

use serde::{Deserialize, Serialize};

/// `COMMON` from `driver/edifier_home_driver.py`.
pub const COMMON: u8 = 0x2E;
/// `HEADER` from `driver/edifier_home_driver.py`.
pub const HEADER: u8 = 0xAA;
/// `DEVICE_TYPE` from `driver/edifier_home_driver.py`.
pub const DEVICE_TYPE: u8 = 0xEC;

/// `CommandIndexType.CM_65` - query the device state.
pub const CMD_QUERY: u8 = 0x65;
/// `CommandIndexType.CM_EB` - apply a state change.
pub const CMD_SET: u8 = 0xEB;

/// `MAX_SEND_LENGTH` / `MAX_READ_LENGTH`.
pub const REPORT_LEN: usize = 64;

/// Build a request frame. The returned vector is exactly as long as the client
/// builds it before padding to `REPORT_LEN`.
pub fn build_frame(index: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(7 + payload.len());
    frame.push(COMMON);
    frame.push(HEADER);
    frame.push(DEVICE_TYPE);
    frame.push(index);
    frame.push(((payload.len() >> 8) & 0xFF) as u8);
    frame.push((payload.len() & 0xFF) as u8);
    frame.extend_from_slice(payload);
    let crc = checksum(&frame);
    frame.push(crc);
    frame
}

/// `add_crc`: sum of every byte from `HEADER` up to (not including) the CRC,
/// truncated to one byte.
pub fn checksum(frame_without_crc: &[u8]) -> u8 {
    frame_without_crc
        .iter()
        .skip(1)
        .fold(0u32, |acc, b| acc + *b as u32) as u8
}

/// Pad a frame out to a full HID report, which is what the HID driver expects
/// on Windows (`OutputReportByteLength`). The CRC stays where the frame put it.
pub fn padded(frame: &[u8]) -> Vec<u8> {
    // Pad to the report length. A frame that does not fit cannot be padded
    // without cutting off its checksum while the length field still claims the
    // full payload, so fall back to the frame as-is: the write then fails
    // loudly instead of sending a silently corrupt command.
    if frame.len() > REPORT_LEN {
        return frame.to_vec();
    }
    let mut out = vec![0u8; REPORT_LEN];
    out[..frame.len()].copy_from_slice(frame);
    out
}

/// Build the reply the device would send for `index`, in the same layout
/// [`Response::parse`] accepts. Used by tests and by the in-memory transport.
pub fn build_response(index: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(7 + payload.len());
    frame.push(COMMON + 1); // 0x2F, the reply start marker
    frame.push(HEADER);
    frame.push(DEVICE_TYPE);
    frame.push(index);
    frame.push(((payload.len() >> 8) & 0xFF) as u8);
    frame.push((payload.len() & 0xFF) as u8);
    frame.extend_from_slice(payload);
    let crc = checksum(&frame);
    frame.push(crc);
    frame
}

/// A decoded reply.
#[derive(Debug, Clone)]
pub struct Response {
    pub code: u8,
    pub index: u8,
    pub payload: Vec<u8>,
    pub crc: u8,
    pub raw: Vec<u8>,
}

impl Response {
    /// Parse a reply using the same field offsets as `CmdReport.__init__`.
    ///
    /// Only the EC report marker (`0x2F`) is accepted. The `0x55` "home" marker
    /// belongs to the EHM protocol, a different family this device does not use;
    /// accepting it here let arbitrary noise be read as a valid reply.
    pub fn parse(data: &[u8]) -> Option<Response> {
        if data.len() < 7 {
            return None;
        }
        let begin = data[0];
        if begin != COMMON + 1 {
            return None;
        }
        let _header = data[1];
        let code = data[2];
        let index = data[3];
        let length = ((data[4] as usize) << 8) | data[5] as usize;
        // The frame is `2E AA EC cmd len_hi len_lo <payload> crc`. When
        // `length <= 57` the CRC byte follows the payload, so the buffer must
        // hold one more byte than the payload needs — checking only
        // `6 + length` let a frame ending exactly at the payload through and
        // then indexed past the end for the CRC.
        let needs = if length > 57 {
            6 + length
        } else {
            6 + length + 1
        };
        if needs > data.len() {
            return None;
        }
        let payload = data[6..6 + length].to_vec();
        // `CmdReport` zeroes the CRC when length > 57.
        let crc = if length > 57 { 0 } else { data[6 + length] };
        Some(Response {
            code,
            index,
            payload,
            crc,
            raw: data.to_vec(),
        })
    }

    /// Whether this reply is plausibly an EC reply rather than noise.
    ///
    /// Deliberately checks only what has been established against the hardware:
    /// the EC report marker and a known device type. It does **not** check
    /// `data[1]` or the trailing checksum:
    ///
    /// * The device answers with `2F BB EC …` in normal operation, while a
    ///   freshly opened handle also produced `2F AA EC …`, so `data[1]` is not a
    ///   constant to test against.
    /// * The trailing byte does not equal [`checksum`] over the reply bytes, so
    ///   replies use a scheme this crate cannot recompute. Requiring a match
    ///   rejected every genuine reply — all seven `read_state` commands came
    ///   back empty while the raw frames were plainly present and correct.
    ///
    /// Matching still requires the command byte, so this narrows the false-accept
    /// risk rather than removing it; see docs/CODE_REVIEW.md item H6.
    pub fn is_valid(&self) -> bool {
        self.raw.len() >= 7 && crate::ec::DEVICE_TYPE_TRANS_MAP.contains(&self.raw[2])
    }

    /// Decode the payload as UTF-8 JSON.
    pub fn json<T: for<'de> Deserialize<'de>>(&self) -> Option<T> {
        let end = self
            .payload
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(self.payload.len());
        serde_json::from_slice(&self.payload[..end]).ok()
    }
}

// ---------------------------------------------------------------------------
// JSON payload model
//
// Field names and shapes are taken from `interface/client/ehm_model.py`
// (`EHMDeviceInfoModel` and friends).
// ---------------------------------------------------------------------------

/// One entry of `lightEffect` (the "lighting" page).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct LightEffect {
    pub light_index: Option<i64>,
    pub selected_index: Option<i64>,
    pub light_switch: Option<i64>,
    pub brightness: Option<i64>,
    pub speed: Option<i64>,
    pub color_enable: Option<i64>,
    pub color: Option<Color>,
    pub reset: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Color {
    pub r: Option<i64>,
    pub g: Option<i64>,
    pub b: Option<i64>,
}

/// `smartLight` - sound-reactive lighting.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SmartLight {
    pub index: Option<i64>,
    pub r#type: Option<i64>,
}

/// `player` - transport state and volume.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Player {
    pub player_status: Option<i64>,
    pub previous_enable: Option<i64>,
    pub next_enable: Option<i64>,
    pub play_enable: Option<i64>,
    pub previous: Option<i64>,
    pub next: Option<i64>,
    pub play_mode: Option<i64>,
    pub volume: Option<i64>,
    pub max_volume: Option<i64>,
    pub minl_voume: Option<i64>,
    pub song: Option<String>,
    pub singer: Option<String>,
    pub lyric: Option<String>,
    pub album: Option<String>,
}

/// `deviceInfo`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct DeviceInfo {
    pub bluetooth_name: Option<String>,
    pub bluetooth_mac: Option<String>,
    pub airplay_name: Option<String>,
    pub wifi_name: Option<String>,
    pub wifi_mac: Option<String>,
    pub set_bluetooth_name_enable: Option<i64>,
    pub set_airplay_name_enable: Option<i64>,
    pub firmware_version: Option<String>,
    pub airplay_name_max_length: Option<i64>,
    pub bluetooth_name_max_length: Option<i64>,
    pub bt_code: Option<String>,
}

/// `soundEffect` - the equaliser / scene presets.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SoundEffect {
    pub sound_index: Option<i64>,
    pub selected_index: Option<i64>,
    pub sound_effect_diy: Option<serde_json::Value>,
}

/// `inputSource`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct InputSource {
    pub input_index: Option<i64>,
    pub selected_index: Option<i64>,
    pub input_error_code: Option<i64>,
}

/// `battery`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Battery {
    pub peer: Option<i64>,
    pub left: Option<i64>,
    pub right: Option<i64>,
    /// The vendor calls this `box` (charging case); `box` is a Rust keyword.
    #[serde(rename = "box")]
    pub case: Option<i64>,
    pub status: Option<i64>,
}

/// `idleShutdown` / `timerShutdown` / `deviceShutdown`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct IdleShutdown {
    pub enable: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct TimerShutdown {
    pub timer_index: Option<i64>,
    pub time_shutdown: Option<i64>,
    pub time_remaining: Option<i64>,
}

/// `hdAudio` - LDAC / LHDC selection.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct HdAudio {
    pub hd_index: Option<i64>,
    pub ldac_selected_index: Option<i64>,
    pub lhdc_selected_index: Option<i64>,
}

/// `screenSetting`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ScreenSetting {
    pub index: Option<i64>,
    pub status: Option<i64>,
    pub mode: Option<i64>,
    pub menu: Option<serde_json::Value>,
    pub fun: Option<serde_json::Value>,
    pub url: Option<String>,
    pub src: Option<serde_json::Value>,
}

/// `ctrlSetting`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CtrlSetting {
    pub index: Option<i64>,
    pub group: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HdAudioLogo {
    pub logo: Option<i64>,
    pub enable: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BluetoothPairingRecords {
    pub max_connect_count: Option<i64>,
    pub records: Option<serde_json::Value>,
}

/// The full device-state document (`EHMDeviceInfoModel`). Everything is
/// optional because the device reports only the branches it supports.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct DeviceState {
    pub supported_features: Option<serde_json::Value>,
    pub only_bluetooth_enable: Option<i64>,
    pub exclusion_functions: Option<serde_json::Value>,
    pub device_info: Option<DeviceInfo>,
    pub player: Option<Player>,
    pub sound_effect: Option<SoundEffect>,
    pub input_source: Option<InputSource>,
    pub hd_audio_logo: Option<HdAudioLogo>,
    pub notification_sound: Option<serde_json::Value>,
    pub bluetooth_pairing_record: Option<BluetoothPairingRecords>,
    pub hd_audio: Option<HdAudio>,
    pub multipoint_connection: Option<serde_json::Value>,
    pub idle_shutdown: Option<IdleShutdown>,
    pub timer_shutdown: Option<TimerShutdown>,
    pub device_shutdown: Option<IdleShutdown>,
    pub disconnect_bluetooth: Option<IdleShutdown>,
    pub reset_to_factory: Option<IdleShutdown>,
    pub firmware_upgrade: Option<serde_json::Value>,
    pub battery: Option<Battery>,
    pub light_effect: Option<LightEffect>,
    pub smart_light: Option<SmartLight>,
    pub ctrl_setting: Option<CtrlSetting>,
    pub screen_setting: Option<ScreenSetting>,
    pub file_load: Option<serde_json::Value>,
    pub time_sync: Option<serde_json::Value>,
}

impl DeviceState {
    /// Feature flags live under `supportedFeatures`; keys are checked
    /// case-insensitively because the device mixes conventions.
    pub fn supports(&self, feature: &str) -> bool {
        let Some(value) = &self.supported_features else {
            return false;
        };
        match value {
            serde_json::Value::Object(map) => map.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case(feature)
                    && !matches!(v, serde_json::Value::Null)
                    && *v != serde_json::Value::Bool(false)
                    && v.as_i64() != Some(0)
            }),
            serde_json::Value::Array(items) => items.iter().any(|item| {
                item.as_str()
                    .map(|s| s.eq_ignore_ascii_case(feature))
                    .unwrap_or(false)
            }),
            _ => false,
        }
    }
}

impl LightEffect {
    /// Convenience: the values the UI manipulates, with sane fallbacks.
    pub fn on(&self) -> bool {
        self.light_switch.unwrap_or(0) != 0
    }

    pub fn brightness_value(&self) -> i64 {
        self.brightness.unwrap_or(0)
    }

    pub fn speed_value(&self) -> i64 {
        self.speed.unwrap_or(0)
    }
}

/// The 4-byte capability header the device puts at the front of a `0x65` reply.
///
/// `get_device_base_function_parse` in `interface/client/ehm_parse.py` reads it
/// as `version = payload[0]`, `encryption = payload[1]`,
/// `max_length = payload[2] << 8 | payload[3]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceHeader {
    pub version: u8,
    pub encryption: u8,
    pub max_length: u16,
}

impl DeviceHeader {
    pub fn parse(payload: &[u8]) -> Option<DeviceHeader> {
        if payload.len() < 4 {
            return None;
        }
        Some(DeviceHeader {
            version: payload[0],
            encryption: payload[1],
            max_length: ((payload[2] as u16) << 8) | payload[3] as u16,
        })
    }
}

impl std::fmt::Display for DeviceHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "version={} encryption={} maxLength={}",
            self.version, self.encryption, self.max_length
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the bounds check used to omit the CRC byte, so a frame
    /// ending exactly at the payload indexed past the end and panicked — on the
    /// session thread, which then died silently.
    #[test]
    fn parse_rejects_a_frame_that_ends_at_the_payload() {
        // 2F AA EC 66 00 01 00 — declares one payload byte, supplies exactly
        // that and no CRC.
        let truncated = [0x2F, 0xAA, 0xEC, 0x66, 0x00, 0x01, 0x00];
        assert!(Response::parse(&truncated).is_none());
        // One byte longer (the CRC) parses fine.
        let mut complete = truncated.to_vec();
        complete.push(checksum(&complete));
        assert!(Response::parse(&complete).is_some());
    }

    /// Only the marker byte and the device type are checkable; `data[1]` and the
    /// trailing checksum are not (the device varies the former and does not
    /// match [`checksum`] for the latter).
    #[test]
    fn validity_checks_marker_and_device_type() {
        let good = build_response(crate::ec::cmd::VOLUME_QUERY, &[16, 10]);
        assert!(Response::parse(&good).is_some_and(|r| r.is_valid()));

        // Wrong marker byte: the EHM "home" report is not an EC reply.
        let mut wrong_marker = good.clone();
        wrong_marker[0] = 0x55;
        assert!(Response::parse(&wrong_marker).is_none());

        // Wrong device type (neither 0xEC nor 0xED) is rejected.
        let mut wrong_type = good.clone();
        wrong_type[2] = 0x00;
        assert!(Response::parse(&wrong_type).is_some_and(|r| !r.is_valid()));

        // The device really does send `2F BB EC …`, which must stay accepted.
        let mut real_world = good.clone();
        real_world[1] = 0xBB;
        assert!(Response::parse(&real_world).is_some_and(|r| r.is_valid()));

        // The alternate device type is accepted too.
        let mut alt = good.clone();
        alt[2] = 0xED;
        assert!(Response::parse(&alt).is_some_and(|r| r.is_valid()));
    }

    /// A reply built by this crate must survive a round trip through parse.
    #[test]
    fn build_response_round_trips() {
        let frame = build_response(crate::ec::cmd::EQ_QUERY, &[2]);
        let parsed = Response::parse(&frame).expect("should parse");
        assert_eq!(parsed.index, crate::ec::cmd::EQ_QUERY);
        assert_eq!(parsed.payload, vec![2]);
        assert!(parsed.is_valid());
    }
}
