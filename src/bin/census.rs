//! Systematic command census: which EC commands does the soundbar accept as
//! *writes*? A real write is acknowledged by the device with `0xD5 [slot]`.
//!
//! Run with a fresh process per batch; the device has a small command buffer.

use std::time::{Duration, Instant};

use halo_soundbar::ec::{self};
use halo_soundbar::hid::{find_control_collection, HidDevice, WriteStrategy};
use halo_soundbar::protocol::Response;
use halo_soundbar::transport::Link;

fn hex(d: &[u8]) -> String {
    d.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Send one frame and collect all distinct replies.
fn probe(
    device: &mut Link<HidDevice>,
    command: u8,
    payload: &[u8],
    window: Duration,
) -> Vec<String> {
    let frame = ec::build_primary(command, payload);
    let _ = device.send_frame(&frame);
    let deadline = Instant::now() + window;
    let mut seen: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        if let Some(item) = device.poll_raw(Duration::from_millis(40)) {
            if let Some(r) = Response::parse(&item.data) {
                let line = format!("{:#04X}[{}]", r.index, hex(&r.payload));
                if !seen.contains(&line) {
                    seen.push(line);
                }
            }
        }
    }
    std::thread::sleep(Duration::from_millis(260));
    seen
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let group = args.get(1).map(|s| s.as_str()).unwrap_or("light");

    let collection = match find_control_collection() {
        Ok(c) => c,
        Err(e) => {
            println!("no collection: {e}");
            return;
        }
    };
    let device = match HidDevice::open(collection) {
        Ok(d) => d,
        Err(e) => {
            println!("open failed: {e}");
            return;
        }
    };
    if device.strategy() == WriteStrategy::Unknown {
        println!("no output mechanism");
        return;
    }
    let strategy = device.strategy();
    let mut device = Link::new(device);
    let _ = device.drain();
    println!("strategy={} group={group}\n", strategy.label());

    // (label, command, payload) — lighting family first, then audio.
    let cases: Vec<(&str, u8, Vec<u8>)> = match group {
        "light" => vec![
            ("0x6A read mood", 0x6A, vec![]),
            ("0x6A read mood [0]", 0x6A, vec![0]),
            ("0x6B set mood empty", 0x6B, vec![]),
            ("0x6B set mood 7B all0", 0x6B, vec![0, 0, 0, 0, 0, 0, 0]),
            ("0x6B set mood 7B red", 0x6B, vec![0, 3, 255, 0, 0, 0, 0]),
            ("0x6B set mood 2B [0,1]", 0x6B, vec![0, 1]),
            ("0x70 shutdn set empty", 0x70, vec![]),
            ("0x70 shutdn set [1]", 0x70, vec![1]),
            ("0x72 reset empty", 0x72, vec![]),
            ("0x72 reset [0]", 0x72, vec![0]),
            ("0x84 smart read", 0x84, vec![]),
            ("0x85 smart set empty", 0x85, vec![]),
            ("0x85 smart set [1]", 0x85, vec![1]),
            ("0x85 smart set 7B", 0x85, vec![0, 1, 255, 0, 0, 255, 255]),
            ("0x86 smart cfg", 0x86, vec![0]),
            ("0x87 smart cfg", 0x87, vec![0]),
            ("0x47", 0x47, vec![]),
            ("0x43", 0x43, vec![]),
            ("0x69", 0x69, vec![]),
            ("0xC6", 0xC6, vec![]),
            ("0xC7", 0xC7, vec![]),
            ("0xC8", 0xC8, vec![]),
            ("0xC9", 0xC9, vec![]),
            ("0xCA", 0xCA, vec![]),
            ("0xF0", 0xF0, vec![]),
            ("0xF1", 0xF1, vec![]),
            ("0x8D", 0x8D, vec![]),
            ("0x8E", 0x8E, vec![]),
        ],
        _ => vec![
            ("0x66 vol read", 0x66, vec![]),
            ("0x67 vol set", 0x67, vec![9]),
            ("0xC4 eq read", 0xC4, vec![]),
            ("0xC4 eq [1,3]", 0xC4, vec![1, 3]),
            ("0x44 eq empty", 0x44, vec![]),
            ("0x46 eq empty", 0x46, vec![]),
            ("0x05", 0x05, vec![]),
            ("0x06", 0x06, vec![]),
            ("0x08", 0x08, vec![]),
            ("0x09", 0x09, vec![]),
            ("0x16", 0x16, vec![]),
            ("0x98", 0x98, vec![]),
            ("0x99", 0x99, vec![]),
            ("0x9A", 0x9A, vec![]),
            ("0x9B", 0x9B, vec![]),
            ("0x9C", 0x9C, vec![]),
            ("0xCC", 0xCC, vec![]),
            ("0xC1", 0xC1, vec![]),
            ("0xBF", 0xBF, vec![]),
            ("0x07 reset", 0x07, vec![]),
        ],
    };

    for (label, command, payload) in cases {
        let replies = probe(&mut device, command, &payload, Duration::from_millis(600));
        let acked = replies.iter().any(|r| r.starts_with("0xD5"));
        println!(
            "  {label:<24} cmd={command:#04X} pay=[{}]  ->  {}{}",
            hex(&payload),
            if replies.is_empty() {
                "(silent)".to_string()
            } else {
                replies.join("  ")
            },
            if acked { "   <== WRITE ACK" } else { "" }
        );
    }
    println!("\ndone");
}
