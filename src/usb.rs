use crate::{ControllerInput, Group, Row, Visualization};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::fs;
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
        matches!(self.product_id, 0x0a3f | 0x0a57 | 0x0a59)
    }

    pub fn controller_group(&self, inputs: Option<Vec<ControllerInput>>, status: &str) -> Group {
        let name = match self.product_id {
            0x0a3f => "Wolverine V3 Pro",
            0x0a57 | 0x0a59 => "Wolverine V3 Pro 8K PC",
            _ => &self.name,
        };
        let connection = match self.product_id {
            0x0a3f | 0x0a59 => "2.4 GHz USB receiver",
            0x0a57 => "Wired USB",
            _ => "USB connected · transport undetermined",
        };
        let mut group = Group::new(
            format!("controller:{}", self.id),
            name,
            "Live standard inputs · select a button or stick to inspect",
        );
        if self.product_id == 0x0a3f || inputs.is_some() {
            group.visualization = Some(Visualization::Controller {
                name: name.into(),
                connection: connection.into(),
                status: status.into(),
                inputs: inputs.unwrap_or_else(crate::input::unknown_controller_inputs),
            });
        }
        group.rows.push(Row::info("connection", connection, status));
        group.rows.push(Row::info("polling", "Polling & live view", if self.product_id == 0x0a3f {
            "Up to 250 Hz wireless · 1000 Hz wired PC Tournament Mode (Function + Menu + A). UI sampling is separate; no interval override."
        } else {
            "500 ms view sampling; physical polling limits are not inferred from this USB identity. Measurement and USB scheduling details are separate."
        }));
        if self.product_id == 0x0a3f {
            group.rows.push(Row::info("firmware-check", "Firmware · official Windows updater", "Installed firmware is unreadable here. Check the offered updater; no comparison or Linux flash.").controlled(json!({"kind":"button","label":"Check vendor updater","action":"firmware-check","args":{"device":self.id},"destructive":false})));
            group.rows.push(Row::info("vendor-page", "Windows firmware instructions", "Follow the official controller/dongle updater prompts on a Windows PC.").controlled(json!({"kind":"button","label":"Open official support page","action":"vendor-page","args":{"device":self.id},"destructive":false})));
        } else {
            group.rows.push(Row::info("firmware", "Firmware · vendor model required", "Installed proprietary firmware is not readable via xpad/evdev. The verified original 0a3f updater is not offered for this different USB identity."));
        }
        group
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
        if matches!(self.product_id, 0x0a3f | 0x0a57 | 0x0a59) {
            group.rows.push(Row::info("controller-state", "Controller proprietary configuration", "Unavailable through standard xpad/evdev: onboard profiles, independent paddle assignments, physical trigger-lock positions, proprietary battery/configuration and firmware. Mapped button/axis effects are observable below; no guessed USB packets, grabs, driver changes or firmware comparisons are performed."));
            group.rows.push(Row::info("controller-limits", "Physical polling limits", if self.product_id == 0x0a3f {
                "Original Wolverine V3 Pro: vendor documents up to 250 Hz wireless and 1000 Hz wired PC Tournament Mode (Function + Menu + A). A 4 ms wireless USB endpoint does not become 1000 Hz via a Linux interval override. The 8K PC model is different hardware."
            } else {
                "1532:0a57 wired and 0a59 wireless are Wolverine V3 Pro 8K PC identities documented in OpenRazer issue #2569 and upstream xpad support. They are distinct from original 0a3f; advertised physical polling is not inferred from endpoint scheduling or UI sampling."
            }));
        }
        if self.product_id == 0x0a3f {
            group.rows.push(Row::info("firmware-check", "Original Wolverine vendor updater", "Check the official download redirect for the currently offered Windows updater. Installed firmware is unavailable; no newer-than-installed claim is possible.").controlled(json!({"kind":"button","label":"Check vendor updater","action":"firmware-check","args":{"device":self.id},"destructive":false})));
            group.rows.push(Row::info("vendor-page", "Official Windows firmware instructions", crate::firmware::VENDOR_PAGE).controlled(json!({"kind":"button","label":"Open official support page","action":"vendor-page","args":{"device":self.id},"destructive":false})));
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
