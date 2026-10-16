//! USB-HID transport for the EDIFIER soundbar control collection.
//!
//! The official client uses the `hidapi` binding with
//!
//! ```text
//! VID = 0x2D99 (11673)   usage == 1   usage_page == 0xFF14 (65300)
//! ```
//!
//! which is exactly what `_DeviceManager` in `interface/client/__init__.py`
//! matches on. Two vendor collections exist on interface MI_04:
//!
//! ```text
//! Col01  usage page 0xFF13  62-byte reports
//! Col02  usage page 0xFF14  64-byte reports   <- the one TempoHub talks to
//! ```
//!
//! Because `HidD_SetOutputReport` and a bare `WriteFile` behave differently
//! across HID stacks, the transport tries every sane variant and remembers the
//! first one that the driver accepts.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetManufacturerString, HidD_GetPreparsedData,
    HidD_GetProductString, HidD_GetSerialNumberString, HidD_SetFeature, HidD_SetOutputReport,
    HidP_GetCaps, HIDD_ATTRIBUTES, HIDP_CAPS, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// EDIFIER's USB vendor id, as hard coded in `interface/devices/edifier_home_device.py`.
pub const EDIFIER_VID: u16 = 0x2D99;
/// The Halo Soundbar's product id.
pub const HALO_PID: u16 = 0xA063;
/// Usage page of the control collection TempoHub uses.
pub const CONTROL_USAGE_PAGE: u16 = 0xFF14;
/// Usage page of the secondary vendor collection (present on this hardware).
pub const SECONDARY_USAGE_PAGE: u16 = 0xFF13;
/// HID usage value TempoHub requires.
pub const CONTROL_USAGE: u16 = 0x0001;

/// `{4d1e55b2-f16f-11cf-88cb-001111000030}` (HID device interface class).
const HID_GUID: GUID = GUID::from_u128(0x4d1e55b2_f16f_11cf_88cb_001111000030);

/// Which Windows call ended up working for output reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStrategy {
    /// `WriteFile` with a frame padded to the full report length.
    WriteFilePadded,
    /// `WriteFile` with the frame at its natural length.
    WriteFileExact,
    /// `HidD_SetOutputReport` with the frame padded to the full report length.
    SetOutputReportPadded,
    /// `HidD_SetFeature` with the frame padded to the full report length.
    SetFeaturePadded,
    /// Not probed yet.
    Unknown,
}

impl WriteStrategy {
    pub fn label(self) -> &'static str {
        match self {
            WriteStrategy::WriteFilePadded => "WriteFile(padded)",
            WriteStrategy::WriteFileExact => "WriteFile(exact)",
            WriteStrategy::SetOutputReportPadded => "HidD_SetOutputReport",
            WriteStrategy::SetFeaturePadded => "HidD_SetFeature",
            WriteStrategy::Unknown => "unknown",
        }
    }
}

/// One discovered HID collection.
#[derive(Debug, Clone)]
pub struct Collection {
    pub path: String,
    pub usage_page: u16,
    pub usage: u16,
    pub vendor_id: u16,
    pub product_id: u16,
    pub input_len: u16,
    pub output_len: u16,
    pub feature_len: u16,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
}

impl Collection {
    /// The collection TempoHub selects.
    pub fn is_control(&self) -> bool {
        self.usage_page == CONTROL_USAGE_PAGE && self.usage == CONTROL_USAGE
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Enumerate every HID device interface on the machine.
pub fn enumerate() -> Result<Vec<Collection>, String> {
    let mut out = Vec::new();
    unsafe {
        let set = SetupDiGetClassDevsW(
            Some(&HID_GUID),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .map_err(|e| format!("SetupDiGetClassDevs failed: {e}"))?;

        let mut index = 0u32;
        loop {
            let mut data = SP_DEVICE_INTERFACE_DATA {
                cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInterfaces(set, None, &HID_GUID, index, &mut data).is_err() {
                break;
            }
            index += 1;

            let mut needed = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailW(set, &data, None, 0, Some(&mut needed), None);
            if needed == 0 {
                continue;
            }
            let mut buf = vec![0u8; needed as usize];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(
                set,
                &data,
                Some(detail),
                needed,
                Some(&mut needed),
                None,
            )
            .is_err()
            {
                continue;
            }
            let p = (*detail).DevicePath.as_ptr();
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            let path = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));

            // Open read-only just to read the caps.
            let w = wide(&path);
            let handle = match CreateFileW(
                PCWSTR(w.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            ) {
                Ok(h) if h != INVALID_HANDLE_VALUE => h,
                _ => continue,
            };

            let mut attrs = HIDD_ATTRIBUTES {
                Size: size_of::<HIDD_ATTRIBUTES>() as u32,
                ..Default::default()
            };
            let _ = HidD_GetAttributes(handle, &mut attrs);

            let mut pp = PHIDP_PREPARSED_DATA::default();
            let mut caps = HIDP_CAPS::default();
            if HidD_GetPreparsedData(handle, &mut pp) {
                let _ = HidP_GetCaps(pp, &mut caps);
                let _ = HidD_FreePreparsedData(pp);
            }

            let mut mfg = [0u16; 128];
            let mut prod = [0u16; 128];
            let mut serial = [0u16; 128];
            let _ = HidD_GetManufacturerString(handle, mfg.as_mut_ptr().cast(), 256);
            let _ = HidD_GetProductString(handle, prod.as_mut_ptr().cast(), 256);
            let _ = HidD_GetSerialNumberString(handle, serial.as_mut_ptr().cast(), 256);
            let _ = CloseHandle(handle);

            out.push(Collection {
                path,
                usage_page: caps.UsagePage,
                usage: caps.Usage,
                vendor_id: attrs.VendorID,
                product_id: attrs.ProductID,
                input_len: caps.InputReportByteLength,
                output_len: caps.OutputReportByteLength,
                feature_len: caps.FeatureReportByteLength,
                manufacturer: wide_string(&mfg),
                product: wide_string(&prod),
                serial: wide_string(&serial),
            });
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    Ok(out)
}

/// Pick only the Halo Soundbar control collection.
pub fn find_control_collection() -> Result<Collection, String> {
    select_control_collection(enumerate()?)
}

fn select_control_collection(all: Vec<Collection>) -> Result<Collection, String> {
    all.into_iter()
        .find(|c| {
            c.vendor_id == EDIFIER_VID
                && c.product_id == HALO_PID
                && c.is_control()
                && c.input_len == 64
                && c.output_len == 64
        })
        .ok_or_else(|| "未找到 Halo Soundbar 控制接口（VID 2D99 / PID A063）".to_string())
}

/// A frame received from the device.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub data: Vec<u8>,
    pub at: Instant,
}

/// An open connection to the control collection.
///
/// Two handles are used: a synchronous one for output reports (the mechanism
/// the Windows HID driver accepts) and an overlapped one for input reports, so
/// reads can be abandoned on a timeout instead of parking a thread.
pub struct HidDevice {
    handle: HANDLE,
    pub collection: Collection,
    strategy: WriteStrategy,
    frames: Receiver<Incoming>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    log: Vec<String>,
    /// Frames the device sent unprompted before the reader thread started.
    greeting: Vec<Vec<u8>>,
    /// Reports received on this handle so far (liveness signal).
    seen: Arc<AtomicU64>,
}

impl HidDevice {
    /// Open the control collection and start the reader thread.
    pub fn open(collection: Collection) -> Result<Self, String> {
        let w = wide(&collection.path);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(|e| format!("CreateFileW({}) failed: {e}", collection.path))?;
        if handle == INVALID_HANDLE_VALUE {
            return Err(format!(
                "CreateFileW({}) returned an invalid handle",
                collection.path
            ));
        }

        let mut device = Self {
            handle,
            collection,
            strategy: WriteStrategy::Unknown,
            frames: mpsc::channel().1,
            stop: Arc::new(AtomicBool::new(false)),
            reader: None,
            log: Vec::new(),
            greeting: Vec::new(),
            seen: Arc::new(AtomicU64::new(0)),
        };
        device.strategy = device.probe_write_strategy();
        // The device pushes its greeting (index 0x65) right after the handle is
        // created, so it must be collected before the reader thread starts —
        // otherwise the greeting is already in the driver queue and the reader
        // only ever sees subsequent traffic.
        let greeting = device.collect_greeting();
        let lines: Vec<String> = greeting
            .iter()
            .map(|frame| format!("greeting: {}", crate::device::hex(frame)))
            .collect();
        for line in lines {
            device.note(line);
        }
        device.greeting = greeting;
        device.start_reader();
        Ok(device)
    }

    /// Poll the handle briefly for frames the device sends unprompted.
    ///
    /// Uses overlapped I/O with a deadline so this never parks a thread inside
    /// `ReadFile` — a parked reader would steal frames from the session reader.
    fn collect_greeting(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(350);
        while Instant::now() < deadline {
            match self.read_overlapped(Duration::from_millis(60)) {
                Some(frame) => {
                    out.push(frame);
                    if out.len() >= 8 {
                        break;
                    }
                }
                None => continue,
            }
        }
        out
    }

    /// One overlapped read with a hard timeout.
    fn read_overlapped(&self, timeout: Duration) -> Option<Vec<u8>> {
        unsafe {
            let read_handle = self.handle;
            let event = CreateEventW(None, true, false, None).ok()?;
            let mut overlap = OVERLAPPED {
                hEvent: event,
                ..Default::default()
            };
            let mut buf = vec![0u8; self.collection.input_len.max(64) as usize];
            let mut read = 0u32;
            // A synchronous failure (unplugged device, stale handle) must not
            // be swallowed: without this the caller blocks for the whole
            // timeout and then reports a healthy connection.
            if let Err(e) = ReadFile(
                read_handle,
                Some(&mut buf),
                Some(&mut read),
                Some(&mut overlap),
            ) {
                if e.code().0 as u32 != 0x800703E5 {
                    let _ = CloseHandle(event);
                    return None;
                }
            }
            let start = Instant::now();
            let result = loop {
                if WaitForSingleObject(event, 10).0 == 0 {
                    let mut done = 0u32;
                    if GetOverlappedResult(read_handle, &overlap, &mut done, false).is_ok()
                        && done > 0
                    {
                        buf.truncate(done as usize);
                        break Some(buf.clone());
                    }
                    break None;
                }
                if start.elapsed() >= timeout {
                    let _ = CancelIoEx(read_handle, Some(&overlap));
                    // Cancelling can still complete with data, so harvest it
                    // rather than reporting a timeout for a reply that arrived.
                    let mut done = 0u32;
                    if GetOverlappedResult(read_handle, &overlap, &mut done, true).is_ok()
                        && done > 0
                    {
                        buf.truncate(done as usize);
                        break Some(buf.clone());
                    }
                    break None;
                }
            };
            let _ = CloseHandle(event);
            result
        }
    }

    /// Frames the device sent before the reader thread existed.
    pub fn greeting(&self) -> &[Vec<u8>] {
        &self.greeting
    }

    /// Total reports received on this handle, used as a liveness signal.
    pub fn frames_seen(&self) -> u64 {
        self.seen.load(Ordering::Relaxed)
    }

    pub fn strategy(&self) -> WriteStrategy {
        self.strategy
    }

    pub fn log(&self) -> &[String] {
        &self.log
    }

    fn note(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
    }

    /// Try each output mechanism with a harmless query frame and keep the first
    /// that the driver accepts.
    fn probe_write_strategy(&mut self) -> WriteStrategy {
        let probe = crate::protocol::build_frame(crate::protocol::CMD_QUERY, &[]);
        let padded = crate::protocol::padded(&probe);
        let attempts: [(WriteStrategy, Vec<u8>); 4] = [
            (WriteStrategy::WriteFilePadded, padded.clone()),
            (WriteStrategy::SetOutputReportPadded, padded.clone()),
            (WriteStrategy::WriteFileExact, probe.clone()),
            (WriteStrategy::SetFeaturePadded, padded.clone()),
        ];
        for (strategy, data) in attempts {
            match self.raw_write(strategy, &data) {
                Ok(()) => {
                    self.note(format!("output reports accepted via {}", strategy.label()));
                    return strategy;
                }
                Err(e) => self.note(format!("{} rejected: {e}", strategy.label())),
            }
        }
        self.note("no output mechanism was accepted; commands will fail".to_string());
        WriteStrategy::Unknown
    }

    fn raw_write(&self, strategy: WriteStrategy, data: &[u8]) -> Result<(), String> {
        unsafe {
            match strategy {
                WriteStrategy::WriteFilePadded | WriteStrategy::WriteFileExact => {
                    // The handle is overlapped, so the write must be issued with
                    // an OVERLAPPED structure and then awaited.
                    let event = CreateEventW(None, true, false, None)
                        .map_err(|e| format!("CreateEventW: {e}"))?;
                    let mut overlap = OVERLAPPED {
                        hEvent: event,
                        ..Default::default()
                    };
                    let mut written = 0u32;
                    let issued = WriteFile(
                        self.handle,
                        Some(data),
                        Some(&mut written),
                        Some(&mut overlap),
                    );
                    // ERROR_IO_PENDING (0x800703E5) is the normal result.
                    if let Err(e) = &issued {
                        if e.code().0 as u32 != 0x800703E5 {
                            let _ = CloseHandle(event);
                            return Err(format!("WriteFile(len={}): {e}", data.len()));
                        }
                    }
                    let start = Instant::now();
                    let mut written_total = 0u32;
                    loop {
                        if WaitForSingleObject(event, 10).0 == 0 {
                            let mut done = 0u32;
                            if GetOverlappedResult(self.handle, &overlap, &mut done, false).is_ok()
                            {
                                written_total = done;
                            }
                            break;
                        }
                        if start.elapsed() > Duration::from_millis(1500) {
                            let _ = CancelIoEx(self.handle, Some(&overlap));
                            let mut drained = 0u32;
                            let _ = GetOverlappedResult(self.handle, &overlap, &mut drained, true);
                            break;
                        }
                    }
                    let _ = CloseHandle(event);
                    // A short report is a failed write: Windows rejects an
                    // output report whose length differs from
                    // `OutputReportByteLength`, and reporting that as success
                    // hid the whole class of "wrong HID collection" failures.
                    if written_total as usize == data.len() {
                        Ok(())
                    } else {
                        Err(format!(
                            "WriteFile(len={}) transferred {} bytes",
                            data.len(),
                            written_total
                        ))
                    }
                }
                WriteStrategy::SetOutputReportPadded => {
                    let ok =
                        HidD_SetOutputReport(self.handle, data.as_ptr().cast(), data.len() as u32);
                    if ok {
                        Ok(())
                    } else {
                        Err(format!(
                            "HidD_SetOutputReport(len={}): error {}",
                            data.len(),
                            std::io::Error::last_os_error()
                        ))
                    }
                }
                WriteStrategy::SetFeaturePadded => {
                    let ok = HidD_SetFeature(self.handle, data.as_ptr().cast(), data.len() as u32);
                    if ok {
                        Ok(())
                    } else {
                        Err(format!(
                            "HidD_SetFeature(len={}): error {}",
                            data.len(),
                            std::io::Error::last_os_error()
                        ))
                    }
                }
                WriteStrategy::Unknown => Err("no working output mechanism".to_string()),
            }
        }
    }

    /// Send one frame, using the strategy that probed successfully.
    pub fn send(&mut self, frame: &[u8]) -> Result<(), String> {
        let data = match self.strategy {
            WriteStrategy::WriteFileExact => frame.to_vec(),
            _ => crate::protocol::padded(frame),
        };
        match self.strategy {
            WriteStrategy::Unknown => {
                // Retry the probe in case the device came back.
                self.strategy = self.probe_write_strategy();
                if self.strategy == WriteStrategy::Unknown {
                    return Err("the device does not accept output reports".to_string());
                }
                self.send(frame)
            }
            s => self.raw_write(s, &data),
        }
    }

    fn start_reader(&mut self) {
        let handle_raw = self.handle.0 as isize;
        let stop = self.stop.clone();
        let counter = self.seen.clone();
        let (tx, rx): (Sender<Incoming>, Receiver<Incoming>) = mpsc::channel();
        let read_len = self.collection.input_len.max(64) as usize;

        let join = std::thread::Builder::new()
            .name("halo-hid-reader".into())
            .spawn(move || {
                let handle = HANDLE(handle_raw as *mut c_void);
                let mut buf = vec![0u8; read_len];
                while !stop.load(Ordering::Relaxed) {
                    // Issue an overlapped read and wait at most 120 ms for it.
                    let Ok(event) = (unsafe { CreateEventW(None, true, false, None) }) else {
                        break;
                    };
                    let mut overlap = OVERLAPPED {
                        hEvent: event,
                        ..Default::default()
                    };
                    let mut read = 0u32;
                    let started = unsafe {
                        ReadFile(handle, Some(&mut buf), Some(&mut read), Some(&mut overlap))
                    };
                    // ERROR_IO_PENDING is expected for an overlapped handle.
                    if let Err(e) = &started {
                        if e.code().0 as u32 != 0x800703E5 {
                            unsafe {
                                let _ = CloseHandle(event);
                            }
                            std::thread::sleep(Duration::from_millis(50));
                            continue;
                        }
                    }
                    let begin = Instant::now();
                    let mut got: Option<u32> = None;
                    loop {
                        if unsafe { WaitForSingleObject(event, 10) }.0 == 0 {
                            let mut done = 0u32;
                            if unsafe { GetOverlappedResult(handle, &overlap, &mut done, false) }
                                .is_ok()
                                && done > 0
                            {
                                got = Some(done);
                            }
                            break;
                        }
                        if begin.elapsed() > Duration::from_millis(120) {
                            unsafe {
                                let _ = CancelIoEx(handle, Some(&overlap));
                                let mut drained = 0u32;
                                // A report can complete just as cancellation is
                                // requested; keep it instead of losing a valid reply.
                                if GetOverlappedResult(handle, &overlap, &mut drained, true).is_ok()
                                    && drained > 0
                                {
                                    got = Some(drained);
                                }
                            }
                            break;
                        }
                    }
                    unsafe {
                        let _ = CloseHandle(event);
                    }
                    if let Some(n) = got {
                        let data = buf[..n as usize].to_vec();
                        counter.fetch_add(1, Ordering::Relaxed);
                        if tx
                            .send(Incoming {
                                data,
                                at: Instant::now(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            })
            .ok();

        self.reader = join;
        self.frames = rx;
    }

    /// Non-blocking poll for the next report.
    pub fn poll(&self, timeout: Duration) -> Option<Incoming> {
        self.frames.recv_timeout(timeout).ok()
    }

    /// Drain anything already queued.
    pub fn drain(&self) -> Vec<Incoming> {
        let mut out = Vec::new();
        while let Ok(item) = self.frames.try_recv() {
            out.push(item);
        }
        out
    }
}

/// The HID report collection is one possible carrier for the EC command set.
///
/// Everything the command layer needs from it is these three methods, which is
/// what lets [`crate::ec_state`] stay free of HID types. A BLE GATT transport
/// would implement this same trait and reuse every command and parser above it.
impl crate::transport::EcTransport for HidDevice {
    fn send_frame(&mut self, frame: &[u8]) -> Result<(), String> {
        self.send(frame)
    }

    fn poll_frame(&mut self, timeout: Duration) -> Option<crate::transport::IncomingFrame> {
        self.poll(timeout)
            .map(|item| crate::transport::IncomingFrame {
                data: item.data,
                at: item.at,
            })
    }

    fn drain_frames(&mut self) -> Vec<crate::transport::IncomingFrame> {
        self.drain()
            .into_iter()
            .map(|item| crate::transport::IncomingFrame {
                data: item.data,
                at: item.at,
            })
            .collect()
    }

    fn describe(&self) -> String {
        format!(
            "USB HID usage page {:#06X} · {} · {}",
            self.collection.usage_page,
            self.collection.product,
            self.strategy.label()
        )
    }
}

impl Drop for HidDevice {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.reader.take() {
            // The reader may be parked inside ReadFile; give it a moment, then
            // let it go rather than blocking the UI thread forever.
            let _ = join.join();
        }
        if self.handle != INVALID_HANDLE_VALUE {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn collection(pid: u16, page: u16) -> Collection {
        Collection {
            path: format!("test-{pid}-{page}"),
            usage_page: page,
            usage: CONTROL_USAGE,
            vendor_id: EDIFIER_VID,
            product_id: pid,
            input_len: 64,
            output_len: 64,
            feature_len: 0,
            manufacturer: String::new(),
            product: String::new(),
            serial: String::new(),
        }
    }
    #[test]
    fn only_halo_control_interface_is_selected() {
        let other = collection(0xA064, CONTROL_USAGE_PAGE);
        let secondary = collection(HALO_PID, SECONDARY_USAGE_PAGE);
        assert!(select_control_collection(vec![other.clone(), secondary.clone()]).is_err());
        let halo = collection(HALO_PID, CONTROL_USAGE_PAGE);
        let selected = select_control_collection(vec![other, halo, secondary]).unwrap();
        assert_eq!(
            (selected.product_id, selected.usage_page),
            (HALO_PID, CONTROL_USAGE_PAGE)
        );
    }
}
