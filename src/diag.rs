//! Protocol verification tool.
//!
//! ```text
//! cargo run --bin halo-diag            # state read + command census
//! cargo run --bin halo-diag write      # write tests (fresh handle)
//! ```
//!
//! The read census and the write tests deliberately run as separate
//! invocations: the soundbar has a small command buffer, and a dense burst of
//! queries leaves it silent until the process reopens the handle.

use std::time::{Duration, Instant};

use halo_soundbar::ec::{self, cmd};
use halo_soundbar::ec_state::{self, query};
use halo_soundbar::hid::{find_control_collection, HidDevice, WriteStrategy};
use halo_soundbar::protocol::Response;
use halo_soundbar::transport::Link;

fn hex(d: &[u8]) -> String {
    d.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Send `frame`, collect every distinct reply within `window`, then pause so
/// the device can settle.
fn exchange(device: &mut Link<HidDevice>, frame: &[u8], window: Duration) -> Vec<String> {
    let _ = device.send_frame(frame);
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
    std::thread::sleep(Duration::from_millis(220));
    seen
}

fn open() -> Option<HidDevice> {
    let collection = find_control_collection().ok()?;
    let device = HidDevice::open(collection).ok()?;
    if device.strategy() == WriteStrategy::Unknown {
        println!("no output mechanism");
        return None;
    }
    println!("write strategy : {}", device.strategy().label());
    Some(device)
}

fn main() {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "read".to_string());
    if !matches!(mode.as_str(), "read" | "state" | "write" | "census") {
        eprintln!("usage: halo-diag [read|state|write|census]");
        return;
    }
    let Some(device) = open() else {
        return;
    };
    println!("mode           : {mode}\n");
    let mut device = Link::new(device);
    let _ = device.drain();

    if mode == "write" {
        write_criterion(&mut device);
        volume_round_trip(&mut device);
    } else if mode == "state" || mode == "read" {
        // Read-only smoke test; the historical census also probes write commands.
        read_state(&mut device);
    } else {
        read_state(&mut device);
        census(&mut device);
    }
}

/// Everything the three tabs display.
fn read_state(device: &mut Link<HidDevice>) {
    let state = ec_state::read_state(device);
    println!("=== 1. device state ===");
    if let Some(b) = &state.base {
        println!("  0x65 base function  : [{}]", hex(b));
    }
    if let Some(f) = &state.features {
        println!("  0xD8 feature bitmap : [{}]", hex(f));
    }
    println!(
        "  0x66 volume         : {:?} / {:?}",
        state.volume, state.max_volume
    );
    println!("  0xC4 eq scene       : {:?}", state.eq);
    println!("  0xF2 device status  : {:?}", state.status);
    if let Some(raw) = &state.light_raw {
        println!("  0x6B mood lighting  : [{}]", hex(raw));
    }
    // 0x43 reads the band table. 0x45 is `EQ_RESET`, a *write*, so using it
    // here reset the equaliser instead of reporting it.
    if let Some(bands) = query(
        device,
        cmd::CUSTOM_EQ_QUERY,
        &[],
        Duration::from_millis(800),
    ) {
        println!("  0x43 eq band table  : [{}]", hex(&bands));
    }
}

/// Which commands does this firmware answer at all?
fn census(device: &mut Link<HidDevice>) {
    println!("\n=== 2. command census (empty payload) ===");
    let probes: [(&str, u8); 14] = [
        ("base function", cmd::BASE_FUNCTION),
        ("device status", cmd::DEVICE_STATUS),
        ("volume query", cmd::VOLUME_QUERY),
        ("eq query", cmd::EQ_QUERY),
        ("eq set", cmd::EQ_SET),
        ("eq band table", cmd::CUSTOM_EQ_QUERY),
        ("eq switch", cmd::EQ_SWITCH),
        ("mood light", cmd::MOOD_LIGHT),
        ("mood light reset", cmd::MOOD_LIGHT_RESET),
        ("smart light", cmd::SMART_LIGHT),
        ("led", cmd::LED),
        ("prompt tone", cmd::PROMPT_TONE),
        ("input source", cmd::INPUT_SOURCE),
        ("version", cmd::VERSION),
    ];
    for (name, command) in probes {
        let frame = ec::build_primary(command, &[]);
        let replies = exchange(device, &frame, Duration::from_millis(500));
        println!(
            "  {name:<18} {command:#04X}  ->  {}",
            if replies.is_empty() {
                "(no answer)".to_string()
            } else {
                replies.join("  ")
            }
        );
    }
    println!(
        "\n  note: commands after the first few show '(no answer)' because the dense\n\
         \x20       burst exhausts the device's buffer. Re-run with a narrower probe set\n\
         \x20       (or a fresh process) to test any individual command."
    );
}

/// The write path: EQ scene selection and the mood-lighting payload.
fn write_criterion(device: &mut Link<HidDevice>) {
    println!("=== 3. write path ===");
    println!("  EQ scene select is CM_C4 (0xC4) with [index]");
    for index in 0..3u8 {
        let frame = ec::build_primary(cmd::EQ_SET, &[index]);
        let replies = exchange(device, &frame, Duration::from_millis(800));
        let after = query(device, cmd::EQ_QUERY, &[], Duration::from_millis(800));
        println!(
            "    0xC4 [{index}] -> {:?}   then 0xD5 -> {after:?}",
            replies
        );
    }

    println!("  mood lighting is CM_6B (0x6B) with the 7-byte splice payload");
    for (label, payload) in [
        (
            "all zero",
            ec_state::mood_lighting_splice(0, 0, 0, 0, 0, 0, 0),
        ),
        (
            "effect 1 br=100",
            ec_state::mood_lighting_splice(8, 1, 255, 255, 255, 100, 0),
        ),
        (
            "effect 2 br=70 sp=70",
            ec_state::mood_lighting_splice(8, 2, 0, 255, 0, 70, 70),
        ),
    ] {
        let frame = ec::build_primary(cmd::MOOD_LIGHT, &payload);
        println!(
            "    0x6B {label:<20} -> {:?}",
            exchange(device, &frame, Duration::from_millis(900))
        );
    }

    println!("  read the light table back with CM_6A (0x6A)");
    if let Some(payload) = query(
        device,
        cmd::MOOD_LIGHT_QUERY,
        &[],
        Duration::from_millis(900),
    ) {
        let groups = ec_state::parse_light_table(cmd::MOOD_LIGHT_QUERY, &payload);
        println!(
            "    0x6A -> array={:?} current={:?} {} records",
            payload.first(),
            ec_state::current_light_index(&payload),
            groups.len()
        );
        for g in groups.iter().take(8) {
            println!(
                "      index {:>3}  #{:02X}{:02X}{:02X}  brightness {:>3}  speed {:>3}",
                g.index, g.r, g.g, g.b, g.brightness, g.speed
            );
        }
    }
}

/// Prove the write path with a volume round-trip.
fn volume_round_trip(device: &mut Link<HidDevice>) {
    println!("\n=== 4. volume write round-trip ===");
    let Some(current) = query(device, cmd::VOLUME_QUERY, &[], Duration::from_millis(800))
        .and_then(|p| p.get(1).map(|b| *b as i64))
    else {
        println!("  skipped: device did not report volume");
        return;
    };
    let max = query(device, cmd::VOLUME_QUERY, &[], Duration::from_millis(800))
        .and_then(|p| p.first().map(|b| *b as i64))
        .unwrap_or(16);
    let target = if current >= max {
        (max - 1).max(0)
    } else {
        current + 1
    };

    let frame = ec::build_primary(cmd::VOLUME_SET, &[target as u8]);
    println!("  {current} -> {target}   frame=[{}]", hex(&frame));
    let _ = device.send_frame(&frame);
    std::thread::sleep(Duration::from_millis(700));

    let readback = query(device, cmd::VOLUME_QUERY, &[], Duration::from_millis(800));
    println!("  readback: {readback:?}");
    let confirmed = readback.as_ref().and_then(|p| p.get(1)).map(|b| *b as i64) == Some(target);
    println!(
        "  RESULT : {}",
        if confirmed {
            "volume change CONFIRMED on hardware"
        } else {
            "not confirmed"
        }
    );

    let _ = device.send_frame(&ec::build_primary(cmd::VOLUME_SET, &[current as u8]));
    std::thread::sleep(Duration::from_millis(500));
    println!(
        "  restored: {:?}",
        query(device, cmd::VOLUME_QUERY, &[], Duration::from_millis(800))
    );
}
