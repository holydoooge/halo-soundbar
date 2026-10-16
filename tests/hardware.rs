//! Opt-in round-trip test for a physically connected Halo Soundbar.
//! Close other controllers, then run:
//! cargo test --test hardware -- --ignored --nocapture
//! Temporarily changes volume, EQ and lighting, then restores the original values.

use std::time::{Duration, Instant};

use halo_soundbar::device::{Command, Session, Snapshot};
use halo_soundbar::ec_state::{self, LightGroup};

fn wait_for(session: &Session, label: &str, predicate: impl Fn(&Snapshot) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        let snapshot = session.snapshot();
        if snapshot.connected && predicate(&snapshot) {
            println!("PASS: {label}");
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out: {label}; state: {:?}", session.snapshot().state);
}

fn effect(record: &LightGroup) -> Command {
    Command::SetEffect {
        array_index: record.light_array_index,
        index: record.index,
        rgb: (record.r, record.g, record.b),
        brightness: record.brightness,
        speed: record.speed,
    }
}

/// Restore even if an assertion fails, without power-off or factory-reset writes.
struct Restore {
    session: Session,
    original: Snapshot,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let state = &self.original.state;
        if let Some(volume) = state.volume {
            self.session.send(Command::SetVolume(volume as u8));
        }
        if let Some(eq) = state.eq {
            self.session.send(Command::SetEq(eq as u8));
        }
        self.session.send(Command::SetLightSwitch {
            array_index: 8,
            on: true,
        });
        // Only effect 3 and the initial active effect are touched by this test.
        if let Some(record) = state.light_groups.iter().find(|g| g.index == 3) {
            self.session.send(effect(record));
        }
        if let Some(record) = state
            .light_groups
            .iter()
            .find(|g| Some(g.index as i64) == state.light_index)
        {
            self.session.send(effect(record));
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let current = self.session.snapshot();
            if current.connected
                && current.state.volume == state.volume
                && current.state.eq == state.eq
                && current.state.light_index == state.light_index
                && current.state.light_groups == state.light_groups
            {
                println!("PASS: all original settings restored");
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        eprintln!(
            "RESTORE FAILED: original={state:?}; current={:?}",
            self.session.snapshot().state
        );
        if !std::thread::panicking() {
            panic!("original device settings were not restored");
        }
    }
}

#[test]
#[ignore = "requires Halo Soundbar; temporarily changes settings and restores them"]
fn halo_settings_round_trip() {
    let session = Session::start();
    wait_for(&session, "initial volume/EQ/light state", |s| {
        s.state.volume.is_some() && s.state.eq.is_some() && s.state.light_groups.len() == 8
    });
    let original = session.snapshot();
    assert!(
        original
            .state
            .light_groups
            .iter()
            .any(|g| g.index == 8 && g.b == 1),
        "start with the mood light on so its original effect records can be restored"
    );
    let restore = Restore { session, original };
    let session = &restore.session;
    let volume = restore.original.state.volume.unwrap() as u8;
    let target_volume = volume.saturating_sub(1);
    session.send(Command::SetVolume(target_volume));
    wait_for(session, "volume readback", |s| {
        s.state.volume == Some(target_volume as i64)
    });

    for scene in 0..3 {
        session.send(Command::SetEq(scene));
        wait_for(session, &format!("EQ scene {scene} readback"), |s| {
            s.state.eq == Some(scene as i64)
        });
    }

    session.send(Command::SetEffect {
        array_index: 8,
        index: 3,
        rgb: (32, 96, 192),
        brightness: 60,
        speed: 6,
    });
    wait_for(session, "effect/color/brightness/speed readback", |s| {
        s.state.light_index == Some(3)
            && s.state.light_groups.iter().any(|g| {
                g.index == 3 && (g.r, g.g, g.b, g.brightness, g.speed) == (32, 96, 192, 60, 6)
            })
    });

    session.send(Command::SetLightSwitch {
        array_index: 8,
        on: false,
    });
    wait_for(session, "light off", |s| {
        s.state.light_raw.as_deref() == Some(&[0])
            || s.state
                .light_groups
                .iter()
                .any(|g| g.index == 8 && g.b == 0)
    });
    session.send(Command::SetLightSwitch {
        array_index: 8,
        on: true,
    });
    wait_for(session, "light back on", |s| {
        ec_state::light_table_is_live(&s.state)
            && s.state
                .light_groups
                .iter()
                .any(|g| g.index == 8 && g.b == 1)
    });

    // A burst must converge to the last requested value, without a long backlog.
    for brightness in 1..=10 {
        session.send(Command::SetEffect {
            array_index: 8,
            index: 3,
            rgb: (32, 96, 192),
            brightness: brightness * 10,
            speed: 6,
        });
    }
    wait_for(session, "rapid consecutive light edits converge", |s| {
        s.state
            .light_groups
            .iter()
            .any(|g| g.index == 3 && g.brightness == 100 && g.speed == 6)
    });
    drop(restore);
}
