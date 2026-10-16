//! Read-only installed firmware, using Synapse UI 92281 -> 38287.R and
//! middleware 98773 -> 12844.R. Headers 11880/14397 define 00/87, four bytes;
//! both codecs name those bytes major, minor, internal and reserved. Additional
//! returned bytes have no component decoder in these sources and stay raw.

use super::Session;
use crate::{Group, Row, usb};
use anyhow::{Result, ensure};
use serde_json::json;
use std::fmt::Write;

const SUPPORT_PAGE: &str = "https://mysupport.razer.com/app/answers/detail/a_id/21268";
const PRODUCT_PAGE: &str = "https://www.razer.com/console-controllers/razer-wolverine-v4-pro";

pub fn groups(device: &usb::Device, session: &mut Session) -> Result<[Group; 2]> {
    let reply = session.command(0, 0x87, &[0; 4])?;
    let data = reply.payload();
    ensure!(
        data.len() >= 4,
        "Truncated extended firmware version response"
    );
    let mut group = Group::new(
        format!("wolverine-firmware:{}", device.id),
        "Installed controller firmware",
        "Read directly from the PC HID control interface; USB bcdDevice is not a firmware version.",
    );
    let mut details = Group::new(
        format!("wolverine-firmware-raw:{}", device.id),
        "Native firmware protocol details",
        "Reserved and trailing bytes are not assigned invented component identities.",
    )
    .collapsed();
    group.rows.push(Row::info(
        "installed",
        format!("Installed: {}.{}.{}", data[0], data[1], data[2]),
        format!(
            "Vendor fields: major {}, minor {}, internal {}, reserved {}. The reserved field is not another version component.",
            data[0], data[1], data[2], data[3]
        ),
    ));
    let mut raw = String::with_capacity(data.len() * 3 - 1);
    for (index, byte) in data.iter().enumerate() {
        if index != 0 {
            raw.push(' ');
        }
        write!(&mut raw, "{byte:02x}")?;
    }
    details.rows.push(Row::info(
        "raw",
        "Extended firmware reply · 00/87",
        format!("{raw}. Additional bytes are retained without assigning unverified component identities or versions."),
    ));
    details.rows.push(Row::info(
        "update-support",
        "No verified V4 native update image or transport",
        "The retained V4 Synapse model has no latestFirmwareInfo/update image. No native flashing or V3 updater is offered. An official image and documented V4 transport, or an exact supported fwupd/LVFS device and release, are prerequisites for Linux updating.",
    ));
    group.rows.push(Row::info(
        "official-downloads",
        "V4 support downloads: pairing utility, not firmware",
        "Razer support article 21268 (RZ06-06060), inspected 16 October 2026, lists Pairing Utility v1.00.00_r2 dated 6 October 2026, but no firmware updater. A pairing utility version is not an available firmware version. Recheck official support for future releases; no image is downloaded or executed here.",
    ));
    group.rows.push(Row::info(
        "official-support",
        "Official V4 support and safe update path",
        format!("Model support: {SUPPORT_PAGE}. Product information: {PRODUCT_PAGE}. Use only an updater explicitly supplied for Wolverine V4 Pro by Razer, following its Windows/controller/dongle and power instructions; do not run a V3 updater or assume Wine/VM USB flashing is safe."),
    ).controlled(json!({"kind":"button","label":"Open official V4 support","action":"vendor-page","args":{"device":device.id},"destructive":false})));
    Ok([group, details])
}
