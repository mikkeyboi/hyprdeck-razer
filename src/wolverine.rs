//! Wolverine V4 Pro PC HID (1532:0a79), without detaching the input driver.
//!
//! Synapse `ui-main.js` module 54849 builds 90-byte Razer packets with a 1..31
//! transaction ID and XOR of packet bytes 2..88. The observed interface 1 HID
//! descriptor declares feature report 10 with 90 data bytes (91 with report ID).
//! Modules 11880/38287 implement per-profile high-speed polling 00/c0 and 00/40;
//! module 14160 offers 250/500/1000/2000/4000/8000 Hz for controllers. Modules
//! 59737/50488 implement battery 07/80 (level / 255, floored) and charging 07/84.

#[path = "wolverine_firmware.rs"]
mod firmware_status;
#[path = "wolverine_keyboard.rs"]
mod keyboard;
#[path = "wolverine_lighting.rs"]
mod lighting;
#[path = "wolverine_mapping.rs"]
mod mapping;
#[path = "wolverine_motor.rs"]
mod motor;

use crate::{Battery, Group, Row, string_arg, usb};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const REPORT_ID: u8 = 10;
const REPORT_SIZE: usize = 91;
const MAX_PAYLOAD: usize = 80;
const HIDIOCSFEATURE: libc::c_ulong = 0xc05b4806;
const HIDIOCGFEATURE: libc::c_ulong = 0xc05b4807;
const HIDIOCGRAWINFO: libc::c_ulong = 0x80084803;
const POLLING_RATES: [u16; 6] = [250, 500, 1000, 2000, 4000, 8000];
const COMMAND_TIMEOUT: Duration = Duration::from_millis(750);
const SESSION_TIMEOUT: Duration = Duration::from_secs(4);

#[repr(C)]
#[derive(Default)]
struct RawInfo {
    bustype: u32,
    vendor: i16,
    product: i16,
}

pub struct Reply {
    report: [u8; REPORT_SIZE],
}
impl Reply {
    pub fn payload(&self) -> &[u8] {
        &self.report[9..9 + usize::from(self.report[6])]
    }
}

pub struct Session {
    file: File,
    transaction: u8,
    expires: Instant,
}
impl Session {
    fn open(device: &usb::Device) -> Result<Self> {
        ensure!(
            device.product_id == 0x0a79,
            "Native controls require Wolverine V4 Pro PC HID mode (Function + Menu + Y; 1532:0a79)"
        );
        let root = Path::new("/sys/class/hidraw");
        for entry in fs::read_dir(root).context("Reading HID raw interfaces")? {
            let entry = entry?;
            let Ok(hid) = fs::canonicalize(entry.path().join("device")) else {
                continue;
            };
            if !hid.starts_with(&device.path) {
                continue;
            }
            let Some(interface) = hid.parent() else {
                continue;
            };
            if usb::text(interface.join("bInterfaceNumber")).as_deref() != Some("01") {
                continue;
            }
            let node = Path::new("/dev").join(entry.file_name());
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(&node)
                .with_context(|| format!("Opening {}: install data/70-hyprdeck-wolverine.rules for active-seat access to this control interface, reload udev rules and reconnect. Do not run the GUI as root or grant access to all input devices", node.display()))?;
            let mut info = RawInfo::default();
            // SAFETY: RawInfo has the Linux hidraw_devinfo layout and a live FD.
            let rc = unsafe { libc::ioctl(file.as_raw_fd(), HIDIOCGRAWINFO, &mut info) };
            if rc < 0 {
                return Err(std::io::Error::last_os_error()).context("Reading HID identity");
            }
            ensure!(
                info.bustype == 3 && info.vendor == 0x1532 && info.product == 0x0a79,
                "HID identity changed while opening the controller; refresh devices"
            );
            let deadline = Instant::now() + COMMAND_TIMEOUT;
            loop {
                // Advisory exclusion across plugin state/action subprocesses. It
                // never grabs evdev or detaches hid-generic from any interface.
                let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if rc == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(error).context("Locking controller configuration interface");
                }
                ensure!(
                    Instant::now() < deadline,
                    "Controller configuration is busy with another request; refresh before applying again"
                );
                thread::sleep(Duration::from_millis(10));
            }
            return Ok(Self {
                file,
                transaction: 0,
                expires: Instant::now() + SESSION_TIMEOUT,
            });
        }
        bail!(
            "Wolverine PC HID control interface 1 is unavailable; keep Function + Menu + Y mode and refresh"
        )
    }

    pub fn command(&mut self, class: u8, id: u8, data: &[u8]) -> Result<Reply> {
        ensure!(
            data.len() <= MAX_PAYLOAD,
            "HID command exceeds the 80-byte payload"
        );
        ensure!(
            Instant::now() < self.expires,
            "Native HID session deadline reached; refresh before applying again"
        );
        self.transaction = self.transaction % 31 + 1;
        let mut request = request(self.transaction, class, id, data);
        // SAFETY: The ioctl encodes the exact size of this writable report.
        let rc =
            unsafe { libc::ioctl(self.file.as_raw_fd(), HIDIOCSFEATURE, request.as_mut_ptr()) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("Sending HID command {class:02x}/{id:02x}"));
        }
        ensure!(
            rc as usize == REPORT_SIZE,
            "Short HID command write: {rc} bytes"
        );
        let deadline = (Instant::now() + COMMAND_TIMEOUT).min(self.expires);
        loop {
            thread::sleep(Duration::from_millis(10));
            let mut reply = [0; REPORT_SIZE];
            reply[0] = REPORT_ID;
            // SAFETY: reply has the 91 writable bytes declared by the ioctl.
            let rc =
                unsafe { libc::ioctl(self.file.as_raw_fd(), HIDIOCGFEATURE, reply.as_mut_ptr()) };
            if rc < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("Reading HID command {class:02x}/{id:02x}"));
            }
            ensure!(
                rc as usize == REPORT_SIZE,
                "Short HID feature report: {rc} bytes"
            );
            if let Some(reply) = completed_reply(reply, self.transaction, class, id)? {
                return Ok(reply);
            }
            ensure!(
                Instant::now() < deadline,
                "HID command {class:02x}/{id:02x} did not complete; no request was resent"
            );
        }
    }
}

fn checksum(report: &[u8; REPORT_SIZE]) -> u8 {
    report[3..89].iter().fold(0, |crc, byte| crc ^ byte)
}
fn request(transaction: u8, class: u8, id: u8, data: &[u8]) -> [u8; REPORT_SIZE] {
    let mut report = [0; REPORT_SIZE];
    report[0] = REPORT_ID;
    report[2] = transaction;
    report[6] = data.len() as u8;
    report[7] = class;
    report[8] = id;
    report[9..9 + data.len()].copy_from_slice(data);
    report[89] = checksum(&report);
    report
}
fn completed_reply(
    report: [u8; REPORT_SIZE],
    transaction: u8,
    class: u8,
    id: u8,
) -> Result<Option<Reply>> {
    // A previous command's report can remain available while this one is busy.
    if report[0] != REPORT_ID || report[2] != transaction || report[7..9] != [class, id] {
        return Ok(None);
    }
    ensure!(
        usize::from(report[6]) <= MAX_PAYLOAD,
        "Invalid HID response payload size {}",
        report[6]
    );
    ensure!(
        checksum(&report) == report[89],
        "HID response checksum mismatch for {class:02x}/{id:02x}"
    );
    match report[1] {
        0 | 1 => Ok(None),
        2 => Ok(Some(Reply { report })),
        status => {
            bail!("Controller rejected HID command {class:02x}/{id:02x} with status {status:#04x}")
        }
    }
}

fn active_profile(session: &mut Session) -> Result<u8> {
    let reply = session.command(5, 0x84, &[0])?;
    ensure!(
        reply.payload().len() == 1,
        "Invalid active-profile response"
    );
    Ok(reply.payload()[0])
}
fn polling_code(rate: u16) -> Result<u8> {
    ensure!(
        POLLING_RATES.contains(&rate),
        "Choose 250, 500, 1000, 2000, 4000 or 8000 Hz"
    );
    Ok((8000 / rate) as u8)
}
fn polling_rate(session: &mut Session, profile: u8) -> Result<u16> {
    let reply = session.command(0, 0xc0, &[profile, 0])?;
    let data = reply.payload();
    ensure!(
        data.len() == 2 && data[0] == profile,
        "Polling response does not match the requested onboard profile"
    );
    let rate = match data[1] {
        1 => 8000,
        2 => 4000,
        4 => 2000,
        8 => 1000,
        16 => 500,
        32 => 250,
        other => bail!(
            "Unrecognized controller polling period {other:#04x}; no default rate substituted"
        ),
    };
    Ok(rate)
}
fn polling_group(device: &usb::Device, session: &mut Session) -> Result<Group> {
    let profile = active_profile(session)?;
    let rate = polling_rate(session, profile)?;
    let mut group = Group::new(
        format!("wolverine-polling:{}", device.id),
        "Controller polling rate",
        "Native per-profile configuration read from the controller. This is the configured report rate, not a measured input/sensor cadence.",
    );
    group.rows.push(Row::info(
        "profile",
        format!("Onboard profile {profile}"),
        "Select profiles on the controller; applying never switches the active profile.",
    ));
    group.rows.push(Row::info("polling", format!("Configured: {rate} Hz"), "Each explicit change is confirmed by a new hardware readback.").controlled(json!({
        "kind":"form", "action":"wolverine-configure", "args":{"device":device.id,"operation":"polling"}, "label":"Apply polling rate",
        "fields":[
            {"id":"profile","label":"Onboard profile","kind":"choice","value":profile.to_string(),"options":[{"value":profile.to_string(),"label":format!("Profile {profile} (currently active)")}]},
            {"id":"rate","label":"Polling rate","kind":"choice","value":rate.to_string(),"options":POLLING_RATES.iter().map(|rate| json!({"value":rate.to_string(),"label":format!("{rate} Hz")})).collect::<Vec<_>>()}
        ]
    })));
    Ok(group)
}
fn set_polling(session: &mut Session, value: &Value) -> Result<()> {
    let profile = string_arg(value, "profile")?
        .parse::<u8>()
        .context("Invalid onboard profile")?;
    let rate = string_arg(value, "rate")?
        .parse::<u16>()
        .context("Invalid polling rate")?;
    let code = polling_code(rate)?;
    ensure!(
        active_profile(session)? == profile,
        "Active onboard profile changed; refresh before applying polling rate"
    );
    if polling_rate(session, profile)? != rate {
        session.command(0, 0x40, &[profile, code])?;
    }
    ensure!(
        polling_rate(session, profile)? == rate,
        "Polling command was accepted but readback differs; refresh before applying again"
    );
    ensure!(
        active_profile(session)? == profile,
        "Active profile changed during polling configuration; refresh to see the current profile"
    );
    Ok(())
}
fn battery_state(device: &usb::Device, session: &mut Session) -> Result<(Battery, Group)> {
    let level = session.command(7, 0x80, &[0, 0])?;
    let data = level.payload();
    ensure!(
        data.len() == 2 && data[0] == 0,
        "Invalid controller battery response"
    );
    let percentage = (u16::from(data[1]) * 100 / 255) as f64;
    let charging = session.command(7, 0x84, &[0, 0]);
    let (is_charging, status) = match charging {
        Ok(reply) if reply.payload().len() == 2 && reply.payload()[0] == 0 => {
            match reply.payload()[1] {
                0 => (Some(false), "Not charging".into()),
                1 => (Some(true), "Charging".into()),
                4 => (Some(true), "Fast charging".into()),
                code => (
                    None,
                    format!("Vendor charging state {code}; active charging not inferred"),
                ),
            }
        }
        Ok(_) => (None, "Charging status unavailable: invalid response".into()),
        Err(error) => (None, format!("Charging status unavailable: {error:#}")),
    };
    let battery = Battery {
        percentage: Some(percentage),
        charging: is_charging,
        source: "Wolverine native HID battery query (07/80)".into(),
    };
    let mut group = Group::new(
        format!("wolverine-battery:{}", device.id),
        "Controller battery",
        "Live battery and charging queries; receiver presence is not used as a battery reading.",
    );
    group.rows.push(Row::info(
        "battery",
        format!("Battery: {percentage:.0}%"),
        status,
    ));
    Ok((battery, group))
}

pub struct Snapshot {
    pub groups: Vec<(&'static str, Group)>,
    pub battery: Option<Battery>,
}
fn failed_group(device: &usb::Device, section: &str, title: &str, error: anyhow::Error) -> Group {
    let mut group = Group::new(
        format!("wolverine-{section}-error:{}", device.id),
        title,
        "Native HID read failed; no hardware configuration was changed",
    );
    group.rows.push(Row::info(
        "error",
        "Controller response unavailable",
        format!("{error:#}"),
    ));
    group
}
pub fn snapshot(device: &usb::Device) -> Result<Snapshot> {
    let mut session = Session::open(device)?;
    let polling = polling_group(device, &mut session).unwrap_or_else(|error| {
        failed_group(device, "sensitivity", "Polling rate unavailable", error)
    });
    let mappings = mapping::groups(device, &mut session);
    let (battery, power) = match battery_state(device, &mut session) {
        Ok((battery, group)) => (Some(battery), group),
        Err(error) => (
            None,
            failed_group(device, "power", "Battery unavailable", error),
        ),
    };
    let mut groups = vec![("sensitivity", polling), ("power", power)];
    match mappings {
        Ok([buttons, shortcuts]) => {
            groups.push(("controls", buttons));
            groups.push(("macros", shortcuts));
        }
        Err(error) => groups.push((
            "controls",
            failed_group(device, "controls", "Onboard assignments unavailable", error),
        )),
    }
    match firmware_status::groups(device, &mut session) {
        Ok([status, details]) => {
            groups.push(("firmware", status));
            groups.push(("diagnostics", details));
        }
        Err(error) => groups.push((
            "firmware",
            failed_group(device, "firmware", "Installed firmware unavailable", error),
        )),
    }
    groups.push(("tools", lighting::group(device)));
    match motor::group(device, &mut session) {
        Ok(group) => groups.push(("diagnostics", group.collapsed())),
        Err(error) => groups.push((
            "diagnostics",
            failed_group(device, "motor", "Native haptic details unavailable", error).collapsed(),
        )),
    }
    Ok(Snapshot { groups, battery })
}
pub fn action(args: &Value) -> Result<Group> {
    let device = usb::find(string_arg(args, "device")?)?;
    let operation = string_arg(args, "operation")?;
    ensure!(
        matches!(operation, "polling" | "mapping" | "keyboard"),
        "Unknown native controller operation"
    );
    let mut session = Session::open(&device)?;
    match operation {
        "polling" => set_polling(
            &mut session,
            args.get("value").context("Missing polling form value")?,
        )?,
        "mapping" => mapping::action(&mut session, args)?,
        "keyboard" => mapping::keyboard_action(&mut session, args)?,
        _ => unreachable!(),
    }
    let mut group = Group::new(
        format!("wolverine-result:{}", device.id),
        "Controller configuration verified",
        "Selected settings were confirmed by controller readback; unrelated settings were not rewritten.",
    );
    group.rows.push(Row::info("result", "Onboard readback confirmed", "Refresh the selected product to see its current native settings. Configured polling is not a measurement of actual input cadence."));
    Ok(group)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(transaction: u8, status: u8, payload: &[u8]) -> [u8; REPORT_SIZE] {
        let mut report = request(transaction, 0, 0xc0, payload);
        report[1] = status;
        report
    }
    #[test]
    fn rejects_corrupt_oversized_and_failed_responses() {
        let mut corrupt = reply(7, 2, &[1, 8]);
        corrupt[10] ^= 1;
        assert!(completed_reply(corrupt, 7, 0, 0xc0).is_err());
        let mut oversized = reply(7, 2, &[1, 8]);
        oversized[6] = 81;
        assert!(completed_reply(oversized, 7, 0, 0xc0).is_err());
        assert!(completed_reply(reply(7, 5, &[1, 8]), 7, 0, 0xc0).is_err());
    }
    #[test]
    fn ignores_pending_and_stale_reports_instead_of_fabricating_settings() {
        assert!(
            completed_reply(reply(6, 2, &[1, 8]), 7, 0, 0xc0)
                .unwrap()
                .is_none()
        );
        assert!(
            completed_reply(reply(7, 1, &[1, 8]), 7, 0, 0xc0)
                .unwrap()
                .is_none()
        );
        assert!(
            completed_reply(reply(7, 2, &[1, 8]), 7, 0, 0x81)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn rejects_rates_outside_controller_domain() {
        for rate in [0, 125, 333, 1001, 16000] {
            assert!(polling_code(rate).is_err());
        }
    }
}
