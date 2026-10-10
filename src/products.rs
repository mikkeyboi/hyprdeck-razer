use crate::{Battery, Group, Product, ProductSection, Row, cache, input, openrazer, remapper, usb};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

fn section(id: &str, title: &str, groups: Vec<String>) -> ProductSection {
    ProductSection {
        id: id.into(),
        title: title.into(),
        groups,
    }
}
fn controller_name(device: &usb::Device) -> &str {
    match device.product_id {
        0x0a3f => "Wolverine V3 Pro",
        0x0a57 | 0x0a59 => "Wolverine V3 Pro 8K PC",
        _ => &device.name,
    }
}
fn product_sets(devices: &[usb::Device]) -> Vec<(String, Vec<usize>)> {
    let mut sets = Vec::new();
    let mouse: Vec<usize> = devices
        .iter()
        .enumerate()
        .filter(|(_, device)| matches!(device.product_id, 0x0086 | 0x0088))
        .map(|(index, _)| index)
        .collect();
    let docks: Vec<usize> = devices
        .iter()
        .enumerate()
        .filter(|(_, device)| device.product_id == 0x007e)
        .map(|(index, _)| index)
        .collect();
    // One receiver and at most one wired personality describe the requested kit.
    // Multiple receivers/mice are kept separate; do not attach ambiguous docks.
    let single_kit = mouse
        .iter()
        .filter(|index| devices[**index].product_id == 0x0088)
        .count()
        <= 1
        && mouse
            .iter()
            .filter(|index| devices[**index].product_id == 0x0086)
            .count()
            <= 1;
    let mut consumed = BTreeSet::new();
    if !mouse.is_empty() && single_kit {
        let mut components = mouse;
        if docks.len() == 1 {
            components.push(docks[0]);
        }
        consumed.extend(components.iter().copied());
        sets.push(("basilisk-ultimate".into(), components));
    }
    for (index, device) in devices.iter().enumerate() {
        if consumed.contains(&index) {
            continue;
        }
        let prefix = if device.is_controller() {
            "wolverine"
        } else if matches!(device.product_id, 0x0086 | 0x0088) {
            "basilisk-ultimate"
        } else if device.product_id == 0x007e {
            "mouse-dock"
        } else {
            "peripheral"
        };
        sets.push((
            format!("{prefix}-{}", device.port.replace('.', "-")),
            vec![index],
        ));
    }
    sets
}
fn daemon_matches(
    device: &usb::Device,
    daemon: &openrazer::DeviceSnapshot,
    devices: &[usb::Device],
    snapshots: &[openrazer::DeviceSnapshot],
) -> bool {
    if daemon.product_id() != Some(device.product_id) {
        return false;
    }
    // Read only the real OpenRazer driver serial attributes under this physical
    // USB device. This also handles daemon serials different from USB descriptors.
    fn contains_serial(path: &Path, serial: &str, depth: usize) -> bool {
        if usb::text(path.join("device_serial")).as_deref() == Some(serial) {
            return true;
        }
        if depth == 0 {
            return false;
        }
        fs::read_dir(path).is_ok_and(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .any(|entry| contains_serial(&entry.path(), serial, depth - 1))
        })
    }
    if contains_serial(&device.path, &daemon.serial, 3)
        || usb::text(device.path.join("serial")).as_deref() == Some(daemon.serial.as_str())
    {
        return true;
    }
    // PID-only attachment is safe only when both physical and daemon identities
    // are unique. No getDeviceName/name-string trust and no arbitrary first match.
    devices
        .iter()
        .filter(|current| current.product_id == device.product_id)
        .count()
        == 1
        && snapshots
            .iter()
            .filter(|current| current.product_id() == Some(device.product_id))
            .count()
            == 1
}
fn kernel_battery(device: &usb::Device) -> Option<Battery> {
    let entries = fs::read_dir("/sys/class/power_supply").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(parent) = fs::canonicalize(path.join("device")) else {
            continue;
        };
        if !parent.starts_with(&device.path)
            || usb::text(path.join("type")).as_deref() != Some("Battery")
        {
            continue;
        }
        if usb::text(path.join("present")).as_deref() == Some("0") {
            continue;
        }
        let Some(percentage) = usb::text(path.join("capacity"))
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
        else {
            continue;
        };
        let charging = match usb::text(path.join("status")).as_deref() {
            Some("Charging") => Some(true),
            Some("Discharging" | "Full" | "Not charging") => Some(false),
            _ => None,
        };
        return Some(Battery {
            percentage: Some(percentage),
            charging,
            source: format!(
                "Kernel power_supply {}",
                entry.file_name().to_string_lossy()
            ),
        });
    }
    None
}
fn component_name(device: &usb::Device) -> &str {
    match device.product_id {
        0x0086 => "Basilisk Ultimate · wired",
        0x0088 => "Basilisk Ultimate · receiver",
        0x007e => "Mouse Dock Chroma",
        _ => &device.name,
    }
}
pub fn assemble(
    devices: &[usb::Device],
    mut daemon: openrazer::Snapshot,
) -> Result<(Vec<Group>, Vec<Product>)> {
    let mut groups = Vec::new();
    let mut products = Vec::new();
    let mut claimed_daemon = BTreeSet::new();
    let setup_id = daemon.status.as_ref().map(|group| group.id.clone());
    if let Some(status) = daemon.status.take() {
        groups.push(status);
    }
    for (id, indices) in product_sets(devices) {
        let components: Vec<&usb::Device> = indices.iter().map(|index| &devices[*index]).collect();
        let primary = components
            .iter()
            .find(|device| device.product_id == 0x0088)
            .copied()
            .unwrap_or(components[0]);
        let mouse = matches!(primary.product_id, 0x0086 | 0x0088);
        let controller = primary.is_controller();
        let dock = primary.product_id == 0x007e;
        let name = if mouse {
            "Basilisk Ultimate"
        } else if controller {
            controller_name(primary)
        } else if dock {
            "Mouse Dock Chroma"
        } else {
            &primary.name
        };
        let connection = match primary.product_id {
            0x0088 => "2.4 GHz USB receiver",
            0x0086 => "Wired USB",
            0x0a3f | 0x0a59 => "2.4 GHz USB receiver",
            _ => "USB",
        };
        let mut summary = Group::new(
            format!("{id}:overview"),
            if controller {
                "Current connection"
            } else {
                name
            },
            if controller {
                "Observed kernel driver and USB scheduling"
            } else {
                "Physical product and its connected components"
            },
        );
        let mut diagnostics = Vec::new();
        let mut settings: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        let mut battery = None;
        for device in &components {
            if !controller {
                summary.rows.push(Row::info(format!("component-{}", device.port), component_name(device), if device.product_id == 0x007e { "Charging dock connected · charging status comes from the mouse, not dock USB presence" } else { "Connected component · actual configuration capabilities queried separately" }));
            }
            let detail = device.group();
            if controller {
                if let Some(driver) = detail.rows.iter().find(|row| row.id == "drivers") {
                    summary.rows.push(Row::info(
                        "driver",
                        "Current kernel driver",
                        &driver.subtitle,
                    ));
                }
                let schedules: BTreeSet<&str> = detail
                    .rows
                    .iter()
                    .filter(|row| row.id.contains("-ep-"))
                    .filter_map(|row| row.subtitle.split(';').next())
                    .collect();
                summary.rows.push(Row::info(
                    "usb-schedule",
                    "USB interrupt scheduling",
                    if schedules.is_empty() {
                        "No readable interrupt-IN schedule · not a physical polling measurement"
                            .into()
                    } else {
                        format!(
                            "{} · scheduling opportunities, not measured reports",
                            schedules.into_iter().collect::<Vec<_>>().join("; ")
                        )
                    },
                ));
            }
            diagnostics.push(detail.id.clone());
            groups.push(detail);
            let mut summaries = Vec::new();
            if let Some((result, recorded)) = cache::load("firmware", &device.id) {
                summaries.extend(recorded);
                diagnostics.push(result.id.clone());
                groups.push(result.collapsed());
            }
            let mut live_inputs = None;
            let mut measurement = None;
            let mut input_error = false;
            for node in input::nodes(device)? {
                let sample = input::snapshot(&node, controller, device.product_id == 0x0a3f);
                if live_inputs.is_none() && sample.inputs.is_some() {
                    live_inputs = sample.inputs;
                    measurement = Some(input::measurement_row(&node));
                }
                input_error |= sample.error.is_some();
                diagnostics.push(sample.group.id.clone());
                groups.push(sample.group.collapsed());
                if let Some((result, recorded)) = cache::load("measurement", &node.id) {
                    summaries.extend(recorded);
                    diagnostics.push(result.id.clone());
                    groups.push(result.collapsed());
                }
            }
            summary.rows.extend(summaries);
            if controller {
                let status = if live_inputs.is_some() {
                    "Kernel input available · sampled every 500 ms"
                } else if input_error {
                    "Input unavailable · see Diagnostics for permissions/removal"
                } else {
                    "USB present · controller input unreadable"
                };
                let mut diagram = device.controller_group(live_inputs, status);
                if let Some(row) = measurement {
                    diagram.rows.push(row);
                }
                settings
                    .entry("overview")
                    .or_default()
                    .push(diagram.id.clone());
                groups.push(diagram);
                battery = kernel_battery(device);
            }
            for (index, snapshot) in daemon.devices.iter().enumerate() {
                if claimed_daemon.contains(&index)
                    || !daemon_matches(device, snapshot, devices, &daemon.devices)
                {
                    continue;
                }
                claimed_daemon.insert(index);
                if mouse && matches!(device.product_id, 0x0086 | 0x0088) && battery.is_none() {
                    battery = snapshot.battery();
                }
                for (section, group) in openrazer::product_groups(snapshot, component_name(device))
                {
                    settings.entry(section).or_default().push(group.id.clone());
                    groups.push(group);
                }
                diagnostics.push(snapshot.diagnostics.id.clone());
            }
        }
        if battery.is_none() && (mouse || controller) {
            let mut status = Group::new(
                format!("{id}:battery-diagnostics"),
                "Battery source",
                "Only actual device battery APIs are used",
            )
            .collapsed();
            status.rows.push(Row::info("source", "No percentage source available", if controller { "No battery power_supply with a real capacity attribute belongs to this physical USB device. xpad input and receiver presence do not provide battery percentage. A compatible verified GIP battery driver is required." } else { "OpenRazer getBattery is unavailable or returned its unknown sentinel; receiver/dock presence and USB descriptor revision are not battery readings." }));
            diagnostics.push(status.id.clone());
            groups.push(status);
        }
        if mouse || controller {
            let mapping = remapper::group(&id, &components, controller);
            settings
                .entry("buttons")
                .or_default()
                .push(mapping.id.clone());
            groups.push(mapping);
        }
        if mouse && !settings.contains_key("sensitivity") {
            let mut unavailable = Group::new(
                format!("{id}:capabilities"),
                "Mouse controls",
                "Device configuration requires the actual OpenRazer daemon object",
            );
            unavailable.rows.push(Row::info("missing", "DPI, lighting & power controls unavailable", "The mouse remains usable with its current native input setup. No unsupported/default write controls are fabricated. Setup status is shown once below."));
            settings
                .entry("sensitivity")
                .or_default()
                .push(unavailable.id.clone());
            groups.push(unavailable);
            if let Some(setup) = &setup_id {
                settings.entry("overview").or_default().push(setup.clone());
            }
        }
        if !settings.contains_key("lighting") {
            let mut lighting = Group::new(
                format!("{id}:lighting-status"),
                "Lighting capability",
                "No guessed proprietary USB commands",
            );
            lighting.rows.push(Row::info("status", if controller { "RGB requires the vendor app" } else { "Lighting API unavailable" }, if controller { "This controller supports brightness/effect/color in Razer Controller Setup for Xbox on Windows/Xbox. The wireless receiver control envelope is not established for Linux; xpad/OpenRazer cannot configure it. Controller RGB does not sync with PC Chroma. No false color picker." } else if mouse || dock { "The physical component is connected, but no usable OpenRazer lighting controls were returned. Follow the Mouse controls setup status; no color/effect guesses or unsupported USB writes." } else { "No supported lighting API has been established for this connected product. USB identification alone does not prove writable RGB support." }));
            if primary.product_id == 0x0a3f {
                lighting.rows.push(Row::info("vendor", "Official controller setup & lighting instructions", "Razer's verified FAQ for the original Wolverine V3 Pro.").controlled(json!({"kind":"button","label":"Open official setup instructions","action":"controller-support","args":{"device":primary.id},"destructive":false})));
            }
            settings
                .entry("lighting")
                .or_default()
                .push(lighting.id.clone());
            if dock && let Some(setup) = &setup_id {
                settings.entry("lighting").or_default().push(setup.clone());
            }
            groups.push(lighting);
        }
        if controller {
            let mut audio = Group::new(
                format!("{id}:audio"),
                "Headset audio",
                "Controller 3.5 mm output is separate from standard gamepad input",
            );
            audio.rows.push(Row::info("status", "Headset output unavailable through xpad", "The original receiver exposes vendor-class GIP audio interfaces, not standard USB Audio. xpad registers input but no ALSA headset. A compatible verified GIP audio driver/protocol is required; this plugin does not detach the working input driver. Hardware volume: hold Audio Configuration + D-pad Up/Down."));
            settings.entry("audio").or_default().push(audio.id.clone());
            groups.push(audio);
        }
        let summary_id = summary.id.clone();
        groups.push(summary);
        let mut overview = settings.remove("overview").unwrap_or_default();
        if controller {
            overview.push(summary_id);
        } else {
            overview.insert(0, summary_id);
        }
        let mut sections = vec![section("overview", "Overview", overview)];
        for (section_id, title) in [
            ("sensitivity", "Sensitivity"),
            ("lighting", "Lighting"),
            ("power", "Power"),
            ("buttons", "Buttons & macros"),
            ("audio", "Headset audio"),
        ] {
            if let Some(ids) = settings.remove(section_id) {
                sections.push(section(section_id, title, ids));
            }
        }
        sections.push(section("diagnostics", "Diagnostics", diagnostics));
        products.push(Product {
            id,
            name: name.into(),
            kind: if mouse {
                "mouse"
            } else if controller {
                "controller"
            } else if dock {
                "dock"
            } else {
                "peripheral"
            }
            .into(),
            description: if mouse {
                "Mouse, wireless receiver and connected charging dock"
            } else if controller {
                "Live inputs, connection status and firmware tools"
            } else {
                "Physical USB product · verified API capabilities only"
            }
            .into(),
            status: "USB connected".into(),
            connection: connection.into(),
            battery,
            sections,
        });
    }
    for (index, snapshot) in daemon.devices.into_iter().enumerate() {
        if !claimed_daemon.contains(&index) {
            for product in &mut products {
                if let Some(section) = product
                    .sections
                    .iter_mut()
                    .find(|section| section.id == "diagnostics")
                {
                    section.groups.push(snapshot.diagnostics.id.clone());
                }
            }
        }
        groups.push(snapshot.diagnostics);
    }
    if let Some(setup) = setup_id {
        for product in &mut products {
            if !product
                .sections
                .iter()
                .any(|section| section.groups.contains(&setup))
                && let Some(section) = product
                    .sections
                    .iter_mut()
                    .find(|section| section.id == "diagnostics")
            {
                section.groups.push(setup.clone());
            }
        }
    }
    if devices.is_empty() {
        let mut group = Group::new("usb", "USB devices", "Read-only physical USB discovery");
        group.rows.push(Row::info("none", "No connected Razer product", "No USB device with vendor 1532 is enumerated; no stale hardware cards or input-interface products are retained."));
        groups.push(group);
    }
    Ok((groups, products))
}

pub async fn controller_support(args: &Value) -> Result<()> {
    let device = usb::find(crate::string_arg(args, "device")?)?;
    if device.product_id != 0x0a3f {
        anyhow::bail!(
            "These verified support instructions apply only to the original Wolverine V3 Pro"
        );
    }
    let connection = zbus::Connection::session().await?;
    let options: BTreeMap<&str, zbus::zvariant::Value<'_>> = BTreeMap::new();
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        connection.call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.OpenURI"),
            "OpenURI",
            &(
                "",
                "https://mysupport.razer.com/app/answers/detail/a_id/14587",
                options,
            ),
        ),
    )
    .await??;
    let _: zbus::zvariant::OwnedObjectPath = reply.body().deserialize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn device(pid: u16, port: &str) -> usb::Device {
        usb::Device {
            id: format!("usb:{port}"),
            port: port.into(),
            path: Path::new("/nonexistent").join(port),
            product_id: pid,
            name: "Misleading Keyboard Interface".into(),
            speed: 12.0,
        }
    }
    #[test]
    fn physical_identities_group_kit_not_interfaces_or_unknown_devices() {
        let devices = vec![
            device(0x0088, "1-1"),
            device(0x007e, "1-2"),
            device(0x0086, "1-3"),
            device(0x0a3f, "1-4"),
            device(0x1234, "1-5"),
        ];
        let sets = product_sets(&devices);
        assert_eq!(sets.len(), 3);
        assert_eq!(sets[0].0, "basilisk-ultimate");
        assert_eq!(sets[0].1, vec![0, 2, 1]);
        assert!(sets.iter().any(|(id, _)| id == "wolverine-1-4"));
        assert!(sets.iter().any(|(id, _)| id == "peripheral-1-5"));
        let ambiguous = vec![
            device(0x0088, "1-1"),
            device(0x0088, "1-2"),
            device(0x007e, "1-3"),
        ];
        assert_eq!(product_sets(&ambiguous).len(), 3);
    }
}
