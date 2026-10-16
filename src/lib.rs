//! EDIFIER Halo Soundbar local control library.
//!
//! Everything here was recovered from the shipped EDIFIER TempoHub client:
//!
//! * the frame layout and command set come from `driver/edifier_driver.py`
//!   (`ECDriver`), which is the driver `DEVICE_MAP['EDIFIER']` selects,
//! * the payload decoders come from `interface/client/ec_parse.py` and the
//!   builders from `interface/client/ec_splice.py`,
//! * the device-selection rule (vendor `0x2D99`, usage page `0xFF14`, usage `1`)
//!   comes from `interface/client/__init__.py` (`_DeviceManager`).
//!
//! The soundbar is a `SOUND_BOX` (`HeadsetType == 5`) device, so it speaks the
//! raw-byte **EC** protocol rather than the chunked JSON "home" protocol.
//!
//! Only the local device features are implemented; nothing in this crate talks
//! to the network.

pub mod device;
pub mod ec;
pub mod ec_state;
pub mod hid;
pub mod protocol;
pub mod transport;
