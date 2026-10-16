//! The EC command protocol — the family the Halo Soundbar actually speaks.
//!
//! Everything here comes from `driver/edifier_driver.py` (`ECDriver`), which is
//! the driver behind `DEVICE_MAP['EDIFIER'] = ECDeviceFactory`.
//!
//! Why this and not the JSON "home" protocol: `interface/client/__init__.py`
//! classifies a device as `'EDIFIER'` when its product id is in
//! `DEVICE_TYPE_MAP['HEADPHONE']` or `DEVICE_TYPE_MAP['SPEAKERBOX']`, and
//! `DEVICE_TYPE_MAP['SPEAKERBOX']` is exactly the set of products whose
//! `HeadsetType == 5`. The Halo Soundbar (pid 41059) has `HeadsetType == 5`, so
//! it runs on `ECDeviceFactory`, speaks raw byte payloads, and does **not** use
//! the chunked JSON layer (`ENABLE_PACKET_BUFFER = False`).
//!
//! Frame layout (identical envelope to the home protocol):
//!
//! ```text
//! COMMON = 0x2E   HEADER = 0xAA
//! DEVICE_TYPE_TRANS_MAP = (0, 1)  ->  (0xEC, 0xED)
//! frame = [COMMON, HEADER, DEVICE_TYPE, index & 0xFF] + add_len(len) + payload + crc
//! device_index = index // 256      ->  picks DEVICE_TYPE
//! command      = index &  0xFF
//! ```

use crate::protocol::{checksum, CMD_QUERY, COMMON, HEADER, REPORT_LEN};

/// `DEVICE_TYPE_TRANS_MAP` from `driver/edifier_driver.py`.
pub const DEVICE_TYPE_TRANS_MAP: [u8; 2] = [0xEC, 0xED];

/// `DeviceIndexType.ED` — the second device slot (`index // 256 == 1`).
pub const DEVICE_INDEX_ED: u16 = 1;

/// `CommandIndexType` members used by the soundbar features, from
/// `driver/edifier_driver.py`.
pub mod cmd {
    /// `CM_65` — base function / capability handshake.
    pub const BASE_FUNCTION: u8 = 0x65;
    /// `CM_D8` — enumerate the device's supported functions.
    pub const DEVICE_FUNCTION: u8 = 0xD8;
    /// `CM_F2` — device status.
    pub const DEVICE_STATUS: u8 = 0xF2;
    /// `CM_66` — volume query.
    pub const VOLUME_QUERY: u8 = 0x66;
    /// `CM_67` — volume set.
    pub const VOLUME_SET: u8 = 0x67;
    /// `CM_C1` — noise cancellation.
    pub const ANC: u8 = 0xC1;
    /// `CM_D5` — equaliser **query** (`query_eq_parse` reads `payload[0]`).
    pub const EQ_QUERY: u8 = 0xD5;
    /// `CM_C4` — equaliser **set**: payload is `[index]`.
    pub const EQ_SET: u8 = 0xC4;
    /// `CM_43` — custom-EQ query (returns the band table).
    pub const CUSTOM_EQ_QUERY: u8 = 0x43;
    /// `CM_44` — custom-EQ set.
    pub const CUSTOM_EQ_SET: u8 = 0x44;
    /// `CM_45` — EQ reset.
    pub const EQ_RESET: u8 = 0x45;
    /// `CM_46` — EQ switch.
    pub const EQ_SWITCH: u8 = 0x46;
    /// `CM_6A` — mood-lighting **read**. Reply is
    /// `[light_array_index, current_light_index]` followed by 6-byte records
    /// `[index, R, G, B, Brightness, Speed]`.
    pub const MOOD_LIGHT_QUERY: u8 = 0x6A;
    /// `CM_6B` — mood-lighting **set**.
    pub const MOOD_LIGHT: u8 = 0x6B;
    /// `CM_70` — mood-lighting shut-down behaviour.
    pub const MOOD_LIGHT_SHUTDOWN: u8 = 0x70;
    /// `CM_72` — mood lighting reset.
    pub const MOOD_LIGHT_RESET: u8 = 0x72;
    /// `CM_84` — smart-lights query.
    pub const SMART_LIGHT_QUERY: u8 = 0x84;
    /// `CM_85` — smart lighting set.
    pub const SMART_LIGHT: u8 = 0x85;
    /// `CM_86` / `CM_87` — smart-light configuration.
    pub const SMART_LIGHT_CFG: u8 = 0x86;
    pub const SMART_LIGHT_CFG2: u8 = 0x87;
    /// `CM_F8` — LED.
    pub const LED: u8 = 0xF8;
    /// `CM_FA` — prompt tone.
    pub const PROMPT_TONE: u8 = 0xFA;
    /// `CM_07` — factory reset.
    pub const FACTORY_RESET: u8 = 0x07;
    /// `CM_CE` — power off.
    pub const SHUTDOWN: u8 = 0xCE;
    /// `CM_9D` — firmware / version query.
    /// Firmware version query. The response payload is the version, one byte per
    /// component: `[01 00 02]` is `1.0.2`.
    ///
    /// `0x9D` also exists in the command table but the device does not answer it;
    /// the official client's `driver.version_query` uses `CM_C6`.
    pub const VERSION: u8 = 0xC6;
    /// `CM_BB` — bluetooth name.
    pub const BLUETOOTH_NAME: u8 = 0xBB;
    /// `CM_BC` — bluetooth address.
    pub const BLUETOOTH_MAC: u8 = 0xBC;
    /// `CM_6F` — built-in prompt tone settings.
    pub const BUILT_IN_PROMPT_TONE: u8 = 0x6F;
    /// `CM_62` — input source.
    pub const INPUT_SOURCE: u8 = 0x62;
}

/// Build an EC frame for `command` on `device_index` (0 or 1).
pub fn build(command: u8, payload: &[u8], device_index: u16) -> Vec<u8> {
    let device_type = DEVICE_TYPE_TRANS_MAP
        .get(device_index as usize)
        .copied()
        .unwrap_or(DEVICE_TYPE_TRANS_MAP[0]);
    let mut frame = Vec::with_capacity(7 + payload.len());
    frame.push(COMMON);
    frame.push(HEADER);
    frame.push(device_type);
    frame.push(command);
    frame.push(((payload.len() >> 8) & 0xFF) as u8);
    frame.push((payload.len() & 0xFF) as u8);
    frame.extend_from_slice(payload);
    let crc = checksum(&frame);
    frame.push(crc);
    frame
}

/// Frame for the primary device slot.
pub fn build_primary(command: u8, payload: &[u8]) -> Vec<u8> {
    build(command, payload, 0)
}

/// Pad to a full HID output report (what the driver requires on Windows).
pub fn padded(frame: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; REPORT_LEN];
    let n = frame.len().min(REPORT_LEN);
    out[..n].copy_from_slice(&frame[..n]);
    out
}

/// The capability handshake `ECDriver.get_device_base_function` performs.
pub fn base_function_request(payload: &[u8]) -> Vec<u8> {
    build_primary(cmd::BASE_FUNCTION, payload)
}

/// `ECDriver.get_device_function` — ask which features the device supports.
pub fn device_function_request(payload: &[u8]) -> Vec<u8> {
    build_primary(cmd::DEVICE_FUNCTION, payload)
}

/// `ECDriver.device_status_query`.
pub fn device_status_request(payload: &[u8]) -> Vec<u8> {
    build_primary(cmd::DEVICE_STATUS, payload)
}

/// Convenience: the bare handshake with an empty payload.
pub fn bare_query(command: u8) -> Vec<u8> {
    build(command, &[], 0)
}

/// The index value `ECDriver` uses when the caller passes a bare command byte.
pub fn flat_index(command: u8) -> u16 {
    command as u16
}

/// Re-export for callers that only have the old query constant.
pub const LEGACY_QUERY: u8 = CMD_QUERY;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_shape_matches_the_driver() {
        let frame = build_primary(cmd::BASE_FUNCTION, &[]);
        // COMMON HEADER DTYPE INDEX LEN_HI LEN_LO CRC
        assert_eq!(frame.len(), 7);
        assert_eq!(frame[0], 0x2E);
        assert_eq!(frame[1], 0xAA);
        assert_eq!(frame[2], 0xEC);
        assert_eq!(frame[3], 0x65);
        assert_eq!(frame[4], 0);
        assert_eq!(frame[5], 0);
        assert_eq!(frame[6], 0xFB); // sum(0xAA,0xEC,0x65) & 0xFF
    }

    #[test]
    fn device_index_selects_the_device_type() {
        assert_eq!(build(cmd::BASE_FUNCTION, &[], 0)[2], 0xEC);
        assert_eq!(build(cmd::BASE_FUNCTION, &[], 1)[2], 0xED);
    }

    #[test]
    fn crc_matches_the_reference_implementation() {
        // add_crc sums bytes[1..] and keeps the low byte.
        let frame = build_primary(cmd::DEVICE_FUNCTION, &[0x01, 0x02]);
        let sum: u32 = frame[1..frame.len() - 1].iter().map(|b| *b as u32).sum();
        assert_eq!(*frame.last().unwrap(), (sum & 0xFF) as u8);
    }
}
