//! Set the device volume from a *separate process*.
//!
//! Used to verify that the running application notices a change it did not make
//! — the GUI polls `0x66` every 150 ms and should follow within about a second.
//!
//! ```text
//! cargo run --bin devset        # set 9
//! cargo run --bin devset 4      # set 4
//! ```
//!
//! The app must be running and connected; if it is not, this still changes the
//! device and the app will pick the value up when it reconnects.

use std::time::Duration;

use halo_soundbar::ec::{self, cmd};
use halo_soundbar::ec_state;
use halo_soundbar::hid::{find_control_collection, HidDevice, WriteStrategy};
use halo_soundbar::transport::Link;

fn main() {
    let level: u8 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(9);

    let collection = match find_control_collection() {
        Ok(c) => c,
        Err(e) => {
            println!("no collection: {e}");
            return;
        }
    };
    let Ok(device) = HidDevice::open(collection) else {
        println!("open failed");
        return;
    };
    if device.strategy() == WriteStrategy::Unknown {
        println!("no output mechanism");
        return;
    }
    let mut link = Link::new(device);
    let _ = link.drain();

    let before = ec_state::query(
        &mut link,
        cmd::VOLUME_QUERY,
        &[],
        Duration::from_millis(300),
    );
    println!("device volume before: {before:?}");

    let _ = link.send_frame(&ec::build_primary(cmd::VOLUME_SET, &[level]));
    std::thread::sleep(Duration::from_millis(700));

    let after = ec_state::query(
        &mut link,
        cmd::VOLUME_QUERY,
        &[],
        Duration::from_millis(300),
    );
    println!("set to {level}, device now reports: {after:?}");
}
