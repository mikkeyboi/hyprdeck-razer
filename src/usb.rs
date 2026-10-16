use crate::{ControllerInput, Group, Row, Visualization};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
#[derive(Debug)]
pub struct Device {
    pub id: String,
    pub port: String,
    pub path: PathBuf,
    pub product_id: u16,
    pub name: String,
    pub speed: f64,
}
pub fn text(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
fn hex(path: impl AsRef<Path>) -> Option<u16> {
    u16::from_str_radix(&text(path)?, 16).ok()
}

pub fn devices() -> Result<Vec<Device>> {
    discover(Path::new("/sys/bus/usb/devices"))
}
fn discover(root: &Path) -> Result<Vec<Device>> {
    let mut devices = Vec::new();
    for entry in fs::read_dir(root).context("Reading USB sysfs; Linux sysfs must be mounted")? {
        let entry = entry?;
        let path = entry.path();
        if hex(path.join("idVendor")) != Some(0x1532) {
            continue;
        }
        let Some(product_id) = hex(path.join("idProduct")) else {
            continue;
        };
        let port = entry.file_name().to_string_lossy().into_owned();
        let name =
            text(path.join("product")).unwrap_or_else(|| format!("Razer USB {product_id:04x}"));
        let speed = text(path.join("speed"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        devices.push(Device {
            id: format!("usb:{port}"),
            port,
            path: fs::canonicalize(path)?,
            product_id,
            name,
            speed,
        });
    }
    devices.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(devices)
}
pub fn find(id: &str) -> Result<Device> {
    devices()?
        .into_iter()
        .find(|device| device.id == id)
        .with_context(|| format!("Device {id:?} is disconnected or no longer enumerated"))
}

// USB 2.0 §9.6.6: full/low-speed interrupt bInterval counts 1 ms frames;
// high/super-speed interrupt bInterval is an exponent of 125 us microframes.
fn interval_us(speed: f64, interval: u16) -> Option<u64> {
    if speed >= 480.0 {
        (1..=16)
            .contains(&interval)
            .then(|| 125_u64 << (interval - 1))
    } else if speed > 0.0 {
        (1..=255)
            .contains(&interval)
            .then_some(u64::from(interval) * 1000)
    } else {
        None
    }
}
impl Device {
    pub fn is_controller(&self) -> bool {
        matches!(
            self.product_id,
            0x0a00
                | 0x0a03
                | 0x0a14
                | 0x0a15
                | 0x0a29
                | 0x0a3f
                | 0x0a57
                | 0x0a59
                | 0x0a76
                | 0x0a77
                | 0x0a78
                | 0x0a79
        ) || self.has_xpad_interface()
    }

    pub fn is_wolverine_v4(&self) -> bool {
        matches!(self.product_id, 0x0a76 | 0x0a78 | 0x0a79)
    }

    pub fn has_rear_buttons(&self) -> bool {
        matches!(self.product_id, 0x0a3f | 0x0a57 | 0x0a59 | 0x0a76..=0x0a79)
            || self.name.contains("Wolverine")
    }

    fn has_xpad_interface(&self) -> bool {
        let Ok(entries) = fs::read_dir(&self.path) else {
            return false;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().contains(':') {
                continue;
            }
            let iface = entry.path();
            if let Ok(target) = fs::read_link(iface.join("driver"))
                && target.file_name().is_some_and(|name| name == "xpad")
            {
                return true;
            }
            let class = hex(iface.join("bInterfaceClass"));
            let subclass = hex(iface.join("bInterfaceSubClass"));
            let protocol = hex(iface.join("bInterfaceProtocol"));
            if class == Some(0xff)
                && ((subclass == Some(0x47) && protocol == Some(0xd0))
                    || (subclass == Some(0x5d) && matches!(protocol, Some(0x01 | 0x81))))
            {
                return true;
            }
        }
        false
    }

    pub fn inferred_kind(&self) -> &'static str {
        if self.is_controller() {
            return "controller";
        }
        if matches!(self.product_id, 0x007e | 0x00a4 | 0x0f21) || self.name.contains("Dock") {
            return "dock";
        }
        if matches!(self.product_id, 0x0068 | 0x0c00..=0x0c0f)
            || self.name.contains("Firefly")
            || self.name.contains("Goliathus")
            || self.name.contains("Strider")
        {
            return "mousemat";
        }
        if (0x0001..=0x00ff).contains(&self.product_id) {
            return "mouse";
        }
        if matches!(
            self.product_id,
            0x0111 | 0x0201 | 0x0207 | 0x0208 | 0x0214 | 0x022b | 0x0244
        ) || self.name.contains("Tartarus")
            || self.name.contains("Orbweaver")
            || self.name.contains("Nostromo")
        {
            return "keypad";
        }
        if (0x0100..=0x02ff).contains(&self.product_id) || self.product_id == 0x0a24 {
            return "keyboard";
        }
        if (0x0500..=0x05ff).contains(&self.product_id)
            || self.product_id == 0x0f19
            || self.name.contains("Kraken")
            || self.name.contains("BlackShark")
            || self.name.contains("Barracuda")
        {
            return "headset";
        }
        if (0x0f00..=0x0fff).contains(&self.product_id) {
            return "accessory";
        }
        "peripheral"
    }

    pub fn controller_display_name(&self) -> &str {
        match self.product_id {
            0x0a3f => "Wolverine V3 Pro",
            0x0a57 | 0x0a59 => "Wolverine V3 Pro 8K PC",
            0x0a76 => "Wolverine V4 Pro",
            0x0a78 => "Wolverine V4 Pro (PC XInput)",
            0x0a79 => "Wolverine V4 Pro (PC HID)",
            _ => self
                .name
                .strip_prefix("Razer ")
                .unwrap_or(self.name.as_str()),
        }
    }

    pub fn controller_connection_label(&self) -> &'static str {
        match self.product_id {
            0x0a3f | 0x0a59 => "2.4 GHz USB receiver",
            0x0a57 => "Wired USB",
            0x0a76 => "2.4 GHz receiver / USB (Xbox 250 Hz mode)",
            0x0a78 => "High-Speed USB (PC XInput)",
            0x0a79 => "High-Speed USB (PC HID)",
            _ if self.speed >= 480.0 => "High-Speed USB",
            _ => "USB connected",
        }
    }

    pub fn devnode(&self) -> Option<PathBuf> {
        let bus: u16 = text(self.path.join("busnum"))?.parse().ok()?;
        let dev: u16 = text(self.path.join("devnum"))?.parse().ok()?;
        Some(PathBuf::from(format!("/dev/bus/usb/{bus:03}/{dev:03}")))
    }

    pub fn controller_group(&self, inputs: Option<Vec<ControllerInput>>, status: &str) -> Group {
        let can_stream = inputs.is_some();
        let mut group = Group::new(
            format!("controller:{}", self.id),
            "Live controller",
            "Live input is read independently of device settings; hardware-mapped extra buttons are not invented as independent gamepad inputs.",
        );
        if self.has_rear_buttons() || inputs.is_some() {
            group.visualization = Some(Visualization::Controller {
                name: self.controller_display_name().into(),
                connection: self.controller_connection_label().into(),
                status: status.into(),
                inputs: inputs.unwrap_or_else(crate::input::unknown_controller_inputs),
                stream: can_stream.then(|| json!({"args":["--input-stream",self.id]})),
            });
        }
        group
    }

    pub fn recovery_row(&self) -> Option<Row> {
        if !matches!(self.product_id, 0x0a3f | 0x0a59 | 0x0a76) {
            return None;
        }
        let node = self.devnode()?;
        let _access = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(node)
            .ok()?;
        Some(Row::info("usb-reset", "Reset an unresponsive receiver", "Use only if input is stuck. This disconnects all controller interfaces; it does not prove wireless recovery. Offered only for receiver modes with USB write access.").controlled(json!({"kind":"button","label":"Reset USB receiver","action":"usb-reset","args":{"device":self.id},"destructive":true})))
    }
    pub fn group(&self) -> Group {
        let mut group = Group::new(
            &self.id,
            format!("USB details · {}", self.name),
            format!(
                "Connected USB 1532:{:04x} at {} ({:.0} Mbit/s)",
                self.product_id, self.port, self.speed
            ),
        )
        .collapsed();
        group.rows.push(Row::info("usb-id", "USB identity", format!("1532:{:04x}; {}. IDs identify observed USB personality, not proprietary capabilities.", self.product_id, self.port)));
        group.rows.push(Row::info(
            "serial",
            "USB serial",
            text(self.path.join("serial"))
                .unwrap_or_else(|| "Not supplied by USB descriptor".into()),
        ));
        group.rows.push(Row::info(
            "descriptor-revision",
            "USB descriptor revision (bcdDevice)",
            text(self.path.join("bcdDevice"))
                .map(|v| format!("{v} — NOT a measured or queried installed firmware version"))
                .unwrap_or_else(|| "Unavailable".into()),
        ));
        let mut drivers = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.path) {
            for entry in entries.flatten() {
                let interface = entry.path();
                if !entry.file_name().to_string_lossy().contains(':') {
                    continue;
                }
                if let Ok(target) = fs::read_link(interface.join("driver"))
                    && let Some(name) = target.file_name()
                {
                    drivers.push(name.to_string_lossy().into_owned());
                }
                if let Ok(endpoints) = fs::read_dir(&interface) {
                    for endpoint in endpoints.flatten() {
                        let path = endpoint.path();
                        if !endpoint.file_name().to_string_lossy().starts_with("ep_") {
                            continue;
                        }
                        let address = hex(path.join("bEndpointAddress")).unwrap_or(0);
                        let attributes = hex(path.join("bmAttributes")).unwrap_or(0);
                        if address & 0x80 == 0 || attributes & 3 != 3 {
                            continue;
                        }
                        let raw = hex(path.join("bInterval"));
                        let schedule = raw.and_then(|raw| interval_us(self.speed, raw));
                        let detail = match schedule {
                            Some(us) => format!(
                                "{:.3} ms ({:.1} scheduled opportunities/s); bInterval={} at {:.0} Mbit/s. Endpoint scheduling only, NOT measured reports or physical sensor polling.",
                                us as f64 / 1000.0,
                                1_000_000.0 / us as f64,
                                raw.unwrap(),
                                self.speed
                            ),
                            None => format!(
                                "bInterval={raw:?}; speed {:.0} Mbit/s unknown/invalid for decoding. Not a physical polling measurement.",
                                self.speed
                            ),
                        };
                        group.rows.push(Row::info(
                            format!("{}-ep-{address:02x}", entry.file_name().to_string_lossy()),
                            format!("USB interrupt IN endpoint 0x{address:02x}"),
                            detail,
                        ));
                    }
                }
            }
        }
        drivers.sort();
        drivers.dedup();
        group.rows.push(Row::info(
            "drivers",
            "Bound kernel drivers",
            if drivers.is_empty() {
                "No interface driver currently bound".into()
            } else {
                drivers.join(", ")
            },
        ));
        group.rows.push(Row::info("runtime-power", "USB runtime power", format!("power/control={}; runtime_status={}. Observed sysfs power management only; no power setting is changed.", text(self.path.join("power/control")).unwrap_or_else(|| "unavailable".into()), text(self.path.join("power/runtime_status")).unwrap_or_else(|| "unavailable".into()))));
        if self.is_controller() {
            group.rows.push(Row::info("controller-state", "Controller proprietary configuration", if self.product_id == 0x0a79 { "Native PC HID polling, onboard button mapping and battery getters are queried in their settings sections. Standard evdev reports only observable mapped button/axis effects; physical trigger locks and installed firmware are not inferred. Reading state does not change onboard settings." } else { "Unavailable through standard xpad/evdev: onboard profiles, independent paddle assignments, physical trigger-lock positions, proprietary battery/configuration and firmware. Mapped button/axis effects are observable below; no guessed USB packets, grabs, driver changes or firmware comparisons are performed." }));
            group.rows.push(Row::info("controller-limits", "Physical polling limits", match self.product_id {
                0x0a3f => "Original Wolverine V3 Pro: vendor documents up to 250 Hz wireless and 1000 Hz wired PC Tournament Mode (Function + Menu + A). A 4 ms wireless USB endpoint does not become 1000 Hz via a Linux interval override. The 8K PC model is different hardware.",
                0x0a76 | 0x0a78 | 0x0a79 => "Wolverine V4 Pro (RZ06-0606): Xbox (1532:0a76), PC XInput (1532:0a78) and PC HID (1532:0a79) are distinct USB personalities. Advertised PC support is up to 8000 Hz; only a successful native getter reports the current onboard polling setting, not endpoint scheduling or USB speed.",
                _ => "1532:0a57 wired and 0a59 wireless are Wolverine V3 Pro 8K PC identities documented in OpenRazer issue #2569 and upstream xpad support. Advertised physical polling is not inferred from endpoint scheduling or UI sampling.",
            }));
        }
        if self.product_id == 0x0a3f {
            group.rows.push(Row::info("firmware-check", "Original Wolverine vendor updater", "Check the official download redirect for the currently offered Windows updater. Installed firmware is unavailable; no newer-than-installed claim is possible.").controlled(json!({"kind":"button","label":"Check vendor updater","action":"firmware-check","args":{"device":self.id},"destructive":false})));
            group.rows.push(Row::info("vendor-page", "Official Windows firmware instructions", crate::firmware::VENDOR_PAGE).controlled(json!({"kind":"button","label":"Open official support page","action":"vendor-page","args":{"device":self.id},"destructive":false})));
        } else if self.is_wolverine_v4() {
            group.rows.push(Row::info("vendor-page", "Official Wolverine V4 Pro page", crate::firmware::V4_VENDOR_PAGE).controlled(json!({"kind":"button","label":"Open official product page","action":"vendor-page","args":{"device":self.id},"destructive":false})));
        }
        group
    }
}

pub fn require_original(id: &str) -> Result<Device> {
    let device = find(id)?;
    if device.product_id != 0x0a3f {
        bail!(
            "This verified updater is only for the original 1532:0a3f Wolverine V3 Pro, not another model/USB identity"
        );
    }
    Ok(device)
}

pub fn reset_device(id: &str) -> Result<Group> {
    let device = find(id)?;
    if !device.is_controller() {
        bail!("USB receiver re-initialization is restricted to connected controller devices");
    }
    let devnode = device
        .devnode()
        .context("USB bus/device numbers unavailable in sysfs")?;
    // USBDEVFS_RESET = _IO('U', 20) = 0x5514
    const USBDEVFS_RESET: libc::c_ulong = 0x5514;
    let file = fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(&devnode)
        .or_else(|_| {
            fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(&devnode)
        })
        .with_context(|| {
            format!(
                "Opening {} for USB reset: grant uaccess on 1532:{:04x} via udev (e.g. SUBSYSTEM==\"usb\", ATTR{{idVendor}}==\"1532\", ATTR{{idProduct}}==\"{:04x}\", TAG+=\"uaccess\") or unplug and replug the receiver once",
                devnode.display(),
                device.product_id,
                device.product_id
            )
        })?;
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), USBDEVFS_RESET, 0) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "USBDEVFS_RESET on {} failed: ensure your user has write/uaccess permission on the USB device node",
                devnode.display()
            )
        });
    }
    // Give the kernel a brief moment to re-bind xpad and recreate /dev/input/event*
    std::thread::sleep(std::time::Duration::from_millis(400));
    let mut group = Group::new(
        format!("usb-reset:{}", device.id),
        format!("USB receiver re-initialized · {}", device.name),
        "USBDEVFS_RESET completed; verify controller input after the kernel reset",
    );
    group.rows.push(Row::info(
        "status",
        "USB port reset succeeded",
        format!(
            "Reset {} ({}) at {}. A successful USB reset does not establish that the wireless link or input streaming recovered.",
            device.name,
            devnode.display(),
            device.port
        ),
    ));
    Ok(group)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scheduling_respects_usb_speed() {
        assert_eq!(interval_us(12.0, 4), Some(4000));
        assert_eq!(interval_us(480.0, 4), Some(1000));
        assert_eq!(interval_us(5000.0, 1), Some(125));
        assert_eq!(interval_us(480.0, 17), None);
        assert_eq!(interval_us(0.0, 4), None);
    }
}
