//! Proven motor scaling reads; no guessed play-once command.
//! UI 92281/middleware chunk 2620 module 98773 define 0c/86 get scaling:
//! [profile, controlId, scalingPercent]. Middleware 47442 reads control 5 and
//! writes 5..8 with Synapse levels [0,17,33,50,67,83,100]. Enum 15072/83207 names
//! those controls vibration right/left and impulse right/left. These records
//! are not a physical motor inventory.
//!
//! The same SDK defines a four-byte 0c/07 play-once packet
//! [0, argument1, highByte(argument2), lowByte(argument2)] but neither retained
//! build names the arguments or supplies a caller proving units/bounds. Uint8
//! and Uint16 storage widths are not duration or intensity bounds. Exposing
//! that packet as a timed rumble action would therefore invent a contract.

use super::Session;
use crate::{Group, Row, usb};
use anyhow::{Result, ensure};

const MOTORS: [(u8, &str); 4] = [
    (5, "Right vibration scaling"),
    (6, "Left vibration scaling"),
    (7, "Right impulse scaling"),
    (8, "Left impulse scaling"),
];

pub fn group(device: &usb::Device, session: &mut Session) -> Result<Group> {
    let reply = session.command(5, 0x84, &[0])?;
    ensure!(
        reply.payload().len() == 1,
        "Invalid active-profile response for motor query"
    );
    let profile = reply.payload()[0];
    let mut group = Group::new(
        format!("wolverine-motors:{}", device.id),
        "Native controller haptics",
        "Native HID motor scaling readback is independent of Linux evdev force feedback. Reading these records does not play a motor or prove physical rumble.",
    );
    for (id, name) in MOTORS {
        let reply = session.command(12, 0x86, &[profile, id, 0])?;
        let data = reply.payload();
        ensure!(
            data.len() == 3 && data[..2] == [profile, id] && data[2] <= 100,
            "Invalid motor scaling response for profile {profile}, control {id}"
        );
        group.rows.push(Row::info(
            format!("motor-{id}"),
            format!("{name}: {}%", data[2]),
            format!("Onboard profile {profile}, vendor control {id}, exact scaling byte {}. This is stored haptic scaling, not a live playback intensity or proof that a physical motor was exercised.", data[2]),
        ));
    }
    group.rows.push(Row::info(
        "scaling-range",
        "Vendor intensity scale",
        "Synapse levels 0–6 map to stored scaling percentages 0, 17, 33, 50, 67, 83, 100. Scaling records do not establish play-once duration units or playback intensity bounds.",
    ));
    group.rows.push(Row::info(
        "playback",
        "Native rumble playback not safely available",
        "Synapse's 0c/07 play-once method has no retained caller or named argument contract proving the selector, duration units and bounds. No guessed playback packet is sent. Linux evdev exposes no force-feedback capability in this HID mode, so an evdev test cannot substitute for native playback. A successful command alone would mean acceptance, not felt rumble.",
    ));
    let reply = session.command(5, 0x84, &[0])?;
    ensure!(
        reply.payload() == [profile],
        "Onboard profile changed during motor queries; refresh to read a consistent profile"
    );
    Ok(group)
}
