//! EDIFIER Halo Soundbar control panel.
//!
//! A local (offline) re-implementation of the official TempoHub device page,
//! driven by the EC byte protocol the soundbar actually speaks.
//!
//! ```text
//! cargo run --release              # the GUI
//! cargo run --bin halo-diag read   # protocol census
//! cargo run --bin halo-diag write  # write-path checks
//! ```

#![windows_subsystem = "windows"]

mod settings;
mod ui;

fn main() {
    windows_reactor::App::run_component::<ui::App>(()).unwrap();
}
