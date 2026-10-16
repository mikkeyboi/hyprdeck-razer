//! V4 PC lighting capability diagnostics, not a universal rzDevice25 fallback.
//! AvailableDevices.json explicitly marks V4 wired 2676/2677 and dongle
//! 2680/2681 (0a78/0a79) as isNotChromaDevice:true. UI model 14160's generic
//! quick-effect defaults and profile-slot colors do not override that contract.
//! Headers 436/codec 88187 define the read-only diagnostics below; all returned
//! Command Failure (status 3) on the attached 0a79. No RGB write is exposed.

use crate::{Group, Row, usb};

pub fn group(device: &usb::Device) -> Group {
    let mut group = Group::new(format!("wolverine-lighting:{}", device.id), "Status LEDs, not Chroma RGB", "Verified model capabilities; unsupported lighting queries are not repeated during refresh.").collapsed();
    group.rows.push(Row::info("capability", "Wolverine V4 Pro is a non-Chroma device", "Synapse's AvailableDevices explicitly marks this model, including 0a79, isNotChromaDevice=true. Its status indicator is not an addressable RGB lighting zone; no RGB setter is offered."));
    group.rows.push(Row::info("status-indicator", "What the indicator means", "PC HID: purple · PC XInput: green · Xbox: white · pairing: blinking white · charging: amber · critical battery: blinking red. Source: the official Wolverine V4 Pro master guide, pages 3 and 6–7."));
    group
}
