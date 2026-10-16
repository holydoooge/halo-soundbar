//! Transport contract between the EC command layer and whatever carries it.
//!
//! The soundbar speaks one command set over more than one physical link: USB HID
//! report collections today, BLE GATT (with its XTEA layer) on the mobile side.
//! Everything above this trait — frame layout, checksum, command numbers,
//! payload builders, reply parsers — is transport-agnostic and lives in
//! [`crate::ec`] and [`crate::ec_state`].
//!
//! Adding a transport means implementing [`EcTransport`] and nothing else.
//! [`Link`] holds the request/response pairing, so a new transport does not
//! re-implement it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::protocol::Response;

/// One inbound report plus when it arrived.
#[derive(Debug, Clone)]
pub struct IncomingFrame {
    pub data: Vec<u8>,
    pub at: Instant,
}

/// What the command layer needs from a link.
pub trait EcTransport {
    /// Write one EC frame (already built by [`crate::ec::build_primary`]).
    fn send_frame(&mut self, frame: &[u8]) -> Result<(), String>;
    /// Take the next inbound report, waiting up to `timeout`.
    fn poll_frame(&mut self, timeout: Duration) -> Option<IncomingFrame>;
    /// Take everything already buffered.
    fn drain_frames(&mut self) -> Vec<IncomingFrame>;
    /// Short human-readable description, for logs and the UI.
    fn describe(&self) -> String;
}

/// A deferred frame plus the moment it stops being believable as "current".
#[derive(Debug, Clone)]
struct Deferred {
    frame: IncomingFrame,
    expires: Instant,
}

/// How long a set-aside frame may still answer a later query. The polled
/// commands run every 150–400 ms, so anything older than this is history, not
/// a current reading.
const DEFERRED_TTL: Duration = Duration::from_millis(500);

/// Hard cap on the set-aside queue. The device emits little, but the queue is
/// fed by *every* unparseable report, so it must not grow without bound.
const DEFERRED_MAX: usize = 32;

/// Request/response pairing over any [`EcTransport`].
///
/// The soundbar interleaves unsolicited traffic with replies, so a reply is
/// matched by the command byte in the frame header, and anything that does not
/// match is set aside for the caller's next read instead of being dropped —
/// but only briefly, and only if it is a structurally valid reply. See
/// [`Link::query`] for why the freshness matters.
pub struct Link<T: EcTransport> {
    transport: T,
    /// Frames that arrived while waiting for something else.
    deferred: VecDeque<Deferred>,
}

impl<T: EcTransport> Link<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            deferred: VecDeque::new(),
        }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub fn into_inner(self) -> T {
        self.transport
    }

    pub fn describe(&self) -> String {
        self.transport.describe()
    }

    /// Send a command and return the payload of the matching reply.
    ///
    /// The request is **always** written. A previously set-aside reply is only
    /// consulted if it is still fresh ([`DEFERRED_TTL`]); returning an old one
    /// instead of asking the device is what made a polled value freeze at
    /// whatever happened to be captured during an earlier, unrelated wait.
    ///
    /// A write failure is reported the same way as a timeout — both are
    /// `None` — because the two are indistinguishable to a caller that only
    /// wants a value. Callers that care use [`Link::send`] directly.
    pub fn query(&mut self, command: u8, payload: &[u8], wait: Duration) -> Option<Vec<u8>> {
        self.prune_deferred();
        if let Some(found) = self.take_deferred(command) {
            return Some(found);
        }
        let frame = crate::ec::build_primary(command, payload);
        self.transport.send_frame(&frame).ok()?;

        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            // Never wait past the deadline, even when the remaining time is
            // shorter than the slice: an immediate transport would otherwise
            // spin here for the whole timeout.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Some(item) = self
                .transport
                .poll_frame(remaining.min(Duration::from_millis(60)))
            else {
                continue;
            };
            match Response::parse(&item.data) {
                Some(response) if response.index == command && response.is_valid() => {
                    return Some(response.payload)
                }
                _ => self.defer(item),
            }
        }
        None
    }

    /// Fire a command without waiting for a reply.
    pub fn send(&mut self, command: u8, payload: &[u8]) -> Result<(), String> {
        let frame = crate::ec::build_primary(command, payload);
        self.transport.send_frame(&frame)
    }

    /// Write an already-built frame.
    pub fn send_frame(&mut self, frame: &[u8]) -> Result<(), String> {
        self.transport.send_frame(frame)
    }

    /// Every frame seen since the last call, including deferred ones.
    pub fn drain(&mut self) -> Vec<IncomingFrame> {
        let mut out: Vec<IncomingFrame> =
            self.deferred.drain(..).map(|entry| entry.frame).collect();
        out.extend(self.transport.drain_frames());
        out
    }

    /// Read one frame without command matching — for diagnostics that want to
    /// see every byte the device emits, including unsolicited traffic.
    pub fn poll_raw(&mut self, timeout: Duration) -> Option<IncomingFrame> {
        self.prune_deferred();
        if let Some(entry) = self.deferred.pop_front() {
            return Some(entry.frame);
        }
        self.transport.poll_frame(timeout)
    }

    /// Drop anything buffered.
    pub fn clear(&mut self) {
        self.deferred.clear();
        let _ = self.transport.drain_frames();
    }

    /// Set a frame aside for a possible later query.
    ///
    /// Only structurally valid replies are kept: the old code stored every
    /// unparseable report too, which let noise accumulate without bound.
    fn defer(&mut self, item: IncomingFrame) {
        if Response::parse(&item.data).is_none_or(|r| !r.is_valid()) {
            return;
        }
        if self.deferred.len() >= DEFERRED_MAX {
            self.deferred.pop_front();
        }
        self.deferred.push_back(Deferred {
            frame: item,
            expires: Instant::now() + DEFERRED_TTL,
        });
    }

    /// Forget set-aside frames that are too old to answer anything.
    fn prune_deferred(&mut self) {
        let now = Instant::now();
        while self
            .deferred
            .front()
            .is_some_and(|entry| entry.expires <= now)
        {
            self.deferred.pop_front();
        }
    }

    /// Number of frames currently set aside. Test-only.
    #[cfg(test)]
    pub fn deferred_len(&self) -> usize {
        self.deferred.len()
    }

    /// Age every set-aside frame past `DEFERRED_TTL`. Test-only.
    #[cfg(test)]
    pub fn expire_deferred_for_test(&mut self) {
        let past = Instant::now() - DEFERRED_TTL - Duration::from_millis(1);
        for entry in &mut self.deferred {
            entry.expires = past;
        }
    }

    fn take_deferred(&mut self, command: u8) -> Option<Vec<u8>> {
        let position = self.deferred.iter().position(|entry| {
            Response::parse(&entry.frame.data).is_some_and(|r| r.index == command && r.is_valid())
        })?;
        let entry = self.deferred.remove(position)?;
        Response::parse(&entry.frame.data).map(|r| r.payload)
    }
}

/// In-memory transport, so the command layer can be tested without a device.
#[cfg(test)]
pub mod testing {
    use super::*;

    /// Records everything written and replays a scripted set of replies.
    ///
    /// Replies are matched by the command byte of the frame that was just
    /// written, so a test only has to queue the answers it cares about and does
    /// not depend on the caller's command order.
    #[derive(Default)]
    pub struct FakeTransport {
        pub written: Vec<Vec<u8>>,
        replies: VecDeque<(u8, Vec<u8>)>,
        /// Replies to hand back regardless of what was asked, for unsolicited
        /// traffic.
        unsolicited: VecDeque<Vec<u8>>,
    }

    impl FakeTransport {
        pub fn new() -> Self {
            Self::default()
        }

        /// Queue a raw inbound frame, returned for the next poll whatever the
        /// command was.
        pub fn push_raw(&mut self, frame: Vec<u8>) {
            self.unsolicited.push_back(frame);
        }

        /// Queue the reply the device gives for `command`.
        pub fn push_response(&mut self, command: u8, payload: &[u8]) {
            self.replies
                .push_back((command, crate::protocol::build_response(command, payload)));
        }

        pub fn written_frames(&self) -> Vec<Vec<u8>> {
            self.written.clone()
        }
    }

    impl EcTransport for FakeTransport {
        fn send_frame(&mut self, frame: &[u8]) -> Result<(), String> {
            self.written.push(frame.to_vec());
            Ok(())
        }

        fn poll_frame(&mut self, _timeout: Duration) -> Option<IncomingFrame> {
            if let Some(data) = self.unsolicited.pop_front() {
                return Some(IncomingFrame {
                    data,
                    at: Instant::now(),
                });
            }
            // Answer the command that was written most recently. A written
            // frame is `2E AA EC <cmd> ...`, i.e. the command sits at index 3
            // (the reply layout differs by its leading marker byte).
            let command = *self.written.last()?.get(3)?;
            let position = self
                .replies
                .iter()
                .position(|(queued, _)| *queued == command)?;
            self.replies
                .remove(position)
                .map(|(_, data)| IncomingFrame {
                    data,
                    at: Instant::now(),
                })
        }

        fn drain_frames(&mut self) -> Vec<IncomingFrame> {
            // Only unsolicited traffic: the scripted replies are answers and
            // must survive a `Link::drain`, which callers do on entry.
            self.unsolicited
                .drain(..)
                .map(|data| IncomingFrame {
                    data,
                    at: Instant::now(),
                })
                .collect()
        }

        fn describe(&self) -> String {
            "fake".to_string()
        }
    }

    #[test]
    fn link_matches_replies_by_command() {
        let mut fake = FakeTransport::new();
        // Unsolicited traffic arrives first and must not be mistaken for the
        // reply to the command about to be sent.
        fake.push_raw(crate::protocol::build_response(0xC3, &[0x0D]));
        fake.push_response(0x66, &[0x10, 0x0A]);
        let mut link = Link::new(fake);

        let reply = link.query(0x66, &[], Duration::from_millis(200));
        assert_eq!(reply, Some(vec![0x10, 0x0A]));

        // The unrelated frame is still available afterwards.
        let drained = link.drain();
        assert!(drained
            .iter()
            .any(|f| crate::protocol::Response::parse(&f.data).is_some_and(|r| r.index == 0xC3)));
    }

    /// A set-aside reply older than `DEFERRED_TTL` must not satisfy a later
    /// query: returning it instead of asking the device is what froze polled
    /// values at whatever was captured during an earlier wait.
    #[test]
    fn stale_deferred_reply_is_not_reused() {
        let mut fake = FakeTransport::new();
        fake.push_raw(crate::protocol::build_response(0x66, &[16, 5]));
        let mut link = Link::new(fake);

        // Capture the 0x66 push while waiting for something else.
        assert_eq!(link.query(0x65, &[], Duration::from_millis(120)), None);
        assert_eq!(link.deferred_len(), 1, "the 0x66 push should be set aside");

        // Look as though a long time passed, then ask for 0x66: the request must
        // go out again rather than being answered from the stale entry.
        link.expire_deferred_for_test();
        let writes_before = link.transport().written_frames().len();
        assert_eq!(link.query(0x66, &[], Duration::from_millis(80)), None);
        assert_eq!(
            link.transport().written_frames().len(),
            writes_before + 1,
            "the 0x66 request must reach the device"
        );
    }

    /// A frame with an unknown device type is noise, not an answer.
    #[test]
    fn foreign_frame_is_rejected() {
        let mut fake = FakeTransport::new();
        let mut frame = crate::protocol::build_response(crate::ec::cmd::VOLUME_QUERY, &[16, 10]);
        frame[2] = 0x00; // neither 0xEC nor 0xED
        fake.push_raw(frame);
        let mut link = Link::new(fake);
        assert_eq!(
            link.query(crate::ec::cmd::VOLUME_QUERY, &[], Duration::from_millis(80)),
            None
        );
    }

    #[test]
    fn link_builds_the_client_frame() {
        let mut link = Link::new(FakeTransport::new());
        link.send(0x67, &[9]).unwrap();
        let written = link.transport().written_frames();
        assert_eq!(written.len(), 1);
        // 2E AA EC 67 00 01 09 <crc>
        assert_eq!(&written[0][..3], &[0x2E, 0xAA, 0xEC]);
        assert_eq!(written[0][3], 0x67);
        assert_eq!(&written[0][4..7], &[0x00, 0x01, 0x09]);
    }

    #[test]
    fn deferred_reply_is_reused_without_resending() {
        let mut fake = FakeTransport::new();
        fake.push_response(0x66, &[0x10, 0x05]);
        let mut link = Link::new(fake);

        // First query consumes it.
        assert_eq!(
            link.query(0x66, &[], Duration::from_millis(50)),
            Some(vec![0x10, 0x05])
        );
        let writes_after_first = link.transport().written_frames().len();

        // Second query must actually go out on the wire (no stale reuse).
        let _ = link.query(0x66, &[], Duration::from_millis(50));
        assert!(link.transport().written_frames().len() > writes_after_first);
    }
}
