use crate::{Battery, Group, Product, ProductSection, Row, cache, input, openrazer, remapper, usb};
use anyhow::Result;
use serde_json::Value;
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
    device.controller_display_name()
}

fn map_daemon_kind(dtype: &str, pid: u16, name: &str) -> &'static str {
    match dtype {
        "mouse" => "mouse",
        "keyboard" => "keyboard",
        "keypad" => "keypad",
        "headset" => "headset",
        "mousemat" => "mousemat",
        "accessory" | "core" | "monitor" => {
            if matches!(pid, 0x007e | 0x00a4 | 0x0f21) || name.contains("Dock") {
                "dock"
            } else {
                "accessory"
            }
        }
        _ => "peripheral",
    }
}

fn kind_description(kind: &str, basilisk_kit: bool) -> &'static str {
    if basilisk_kit {
        return "Mouse, wireless receiver and connected charging dock";
    }
    match kind {
        "controller" => "Live inputs, connection status and firmware tools",
        "mouse" => "Available sensitivity, lighting and power controls",
        "keyboard" => "Backlight, Chroma effects and available device information",
        "keypad" => "Gaming keypad lighting and software remapping tools",
        "headset" => "Headset lighting and USB device status",
        "mousemat" => "Chroma surface lighting and brightness controls",
        "dock" => "Charging dock Chroma lighting and brightness controls",
        "accessory" => "Chroma accessory lighting and device status",
        _ => "Physical USB product · verified API capabilities only",
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
fn component_connection(device: &usb::Device) -> &'static str {
    match device.product_id {
        0x0088 => "2.4 GHz USB receiver",
        0x0086 => "Wired USB",
        0x007e => "USB charging dock",
        _ => "USB",
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
        let basilisk = matches!(primary.product_id, 0x0086 | 0x0088);
        let controller = primary.is_controller();
        let mut kind = if basilisk {
            "mouse"
        } else if controller {
            "controller"
        } else {
            primary.inferred_kind()
        };
        let mut display_name: String = if basilisk {
            "Basilisk Ultimate".into()
        } else if controller {
            controller_name(primary).into()
        } else if primary.product_id == 0x007e {
            "Mouse Dock Chroma".into()
        } else {
            primary
                .name
                .strip_prefix("Razer ")
                .unwrap_or(&primary.name)
                .into()
        };
        let connection = if controller {
            primary.controller_connection_label()
        } else {
            component_connection(primary)
        };
        let mut summary = Group::new(
            format!("{id}:overview"),
            if controller {
                "Connection Details"
            } else {
                &display_name
            },
            if controller {
                ""
            } else {
                "Connected hardware & firmware status"
            },
        );
        let mut diagnostics = Vec::new();
        let mut settings: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        let mut battery = None;
        for device in &components {
            if !controller {
                summary.rows.push(Row::info(
                    format!("component-{}", device.port),
                    component_name(device),
                    format!(
                        "Connected on USB {} · {}",
                        device.port,
                        component_connection(device)
                    ),
                ));
            }
            let detail = device.group();
            diagnostics.push(detail.id.clone());
            groups.push(detail);
            let mut summaries = Vec::new();
            if let Some((result, recorded)) = cache::load("firmware", &device.id) {
                summaries.extend(recorded);
                diagnostics.push(result.id.clone());
                groups.push(result.collapsed());
            }
            let mut live_inputs = None;
            let mut input_error = false;
            let mut tools = controller.then(|| Group::new(format!("controller-tools:{}", device.id), "Controller checks", "Run a timed input analysis while moving both sticks. Results appear below after completion; no controller settings are changed."));
            let primary_input = if controller {
                input::primary_node(device)?
            } else {
                None
            };
            for node in input::nodes(device)? {
                let sample = input::snapshot(&node, controller, device.has_rear_buttons());
                let primary_node = primary_input
                    .as_ref()
                    .is_some_and(|primary| primary.id == node.id);
                if primary_node && let Some(tools) = tools.as_mut() {
                    live_inputs = sample.inputs;
                    input_error = sample.error.is_some();
                    tools.rows.push(input::measurement_row(&node));
                    if let Some(row) = input::rumble_row(&node) {
                        tools.rows.push(row);
                    } else {
                        tools.rows.push(Row::info("rumble-support", "Rumble test unavailable in this input mode", "The selected gamepad does not advertise Linux force feedback. No nonfunctional evdev test or guessed native playback packet is offered; native haptic scaling details remain in Diagnostics."));
                    }
                }
                diagnostics.push(sample.group.id.clone());
                groups.push(sample.group.collapsed());
                if let Some((mut result, recorded)) = cache::load("measurement", &node.id) {
                    if controller
                        && primary_node
                        && result
                            .rows
                            .iter()
                            .any(|row| row.id == "left-stick-analysis")
                    {
                        result.collapsed = false;
                        settings.entry("tools").or_default().push(result.id.clone());
                    } else {
                        diagnostics.push(result.id.clone());
                        result.collapsed = true;
                        if !controller {
                            summaries.extend(recorded);
                        }
                    }
                    groups.push(result);
                }
            }
            if let Some(mut tools) = tools {
                let status = if live_inputs.is_some() {
                    "Live input ready"
                } else if input_error {
                    "Input access required"
                } else {
                    "Waiting for controller"
                };
                let diagram = device.controller_group(live_inputs, status);
                settings
                    .entry("overview")
                    .or_default()
                    .push(diagram.id.clone());
                groups.push(diagram);
                if let Some(row) = device.recovery_row() {
                    tools.rows.push(row);
                }
                tools.rows.extend(summaries);
                settings
                    .entry("tools")
                    .or_default()
                    .insert(0, tools.id.clone());
                groups.push(tools);
                battery = kernel_battery(device);
                if device.product_id == 0x0a79 {
                    match crate::wolverine::snapshot(device) {
                        Ok(snapshot) => {
                            for (section, group) in snapshot.groups {
                                if section == "diagnostics" {
                                    diagnostics.push(group.id.clone());
                                } else {
                                    settings.entry(section).or_default().push(group.id.clone());
                                }
                                groups.push(group);
                            }
                            if snapshot.battery.is_some() {
                                battery = snapshot.battery;
                            }
                        }
                        Err(error) => {
                            for (section, title) in [
                                ("sensitivity", "Native polling rate unavailable"),
                                ("controls", "Native onboard mapping unavailable"),
                                ("macros", "Native keyboard shortcuts unavailable"),
                                ("firmware", "Installed firmware unavailable"),
                                ("power", "Native battery unavailable"),
                            ] {
                                let mut unavailable = Group::new(
                                    format!("{}:native-{section}-error", device.id),
                                    title,
                                    "Wolverine V4 Pro PC HID read failed; no settings were changed",
                                );
                                unavailable.rows.push(Row::info(
                                    "read-error",
                                    "Native HID access required",
                                    format!("{error:#}. Ensure your user has read/write access to the controller's vendor HID interface, reload the installed udev permissions and reconnect the controller if needed. Keep Function + Menu + Y PC HID mode enabled, then refresh. No default values are substituted."),
                                ));
                                settings
                                    .entry(section)
                                    .or_default()
                                    .push(unavailable.id.clone());
                                groups.push(unavailable);
                            }
                        }
                    }
                }
            } else {
                summary.rows.extend(summaries);
            }
            for (index, snapshot) in daemon.devices.iter().enumerate() {
                if claimed_daemon.contains(&index)
                    || !daemon_matches(device, snapshot, devices, &daemon.devices)
                {
                    continue;
                }
                claimed_daemon.insert(index);
                if !controller
                    && device.id == primary.id
                    && let Some(dname) = snapshot.device_name()
                    && !basilisk
                    && device.product_id != 0x007e
                {
                    display_name = dname.strip_prefix("Razer ").unwrap_or(dname).to_owned();
                }
                if !controller
                    && device.id == primary.id
                    && !basilisk
                    && let Some(dtype) = snapshot.device_type()
                {
                    let mapped = map_daemon_kind(dtype, device.product_id, &display_name);
                    if mapped != "peripheral" {
                        kind = mapped;
                    }
                }
                if battery.is_none() && device.product_id != 0x007e {
                    battery = snapshot.battery();
                }
                if let Some(fw) = snapshot.firmware_version() {
                    let fw_text = match snapshot.driver_version() {
                        Some(driver) => format!("{fw} · OpenRazer getFirmware · driver {driver}"),
                        None => format!("{fw} · OpenRazer getFirmware; driver version unavailable"),
                    };
                    summary.rows.push(Row::info(
                        format!("firmware-{}", device.port),
                        format!("{} · Firmware", component_name(device)),
                        fw_text,
                    ));
                } else {
                    summary.rows.push(Row::info(
                        format!("firmware-{}", device.port),
                        format!("{} · Firmware", component_name(device)),
                        "Installed firmware unavailable from OpenRazer. A sleeping/unread value is not a version; USB bcdDevice is not firmware.",
                    ));
                }
                if let Some(layout) = snapshot.keyboard_layout() {
                    summary.rows.push(Row::info(
                        format!("layout-{}", device.port),
                        "Keyboard layout",
                        layout,
                    ));
                }
                for (section, group) in openrazer::product_groups(snapshot, component_name(device))
                {
                    settings.entry(section).or_default().push(group.id.clone());
                    groups.push(group);
                }
                diagnostics.push(snapshot.diagnostics.id.clone());
            }
        }
        let mouse = kind == "mouse";
        let dock = kind == "dock";
        if battery.is_none() && (mouse || controller) {
            let mut status = Group::new(
                format!("{id}:battery-diagnostics"),
                "Battery source",
                "Only actual device battery APIs are used",
            )
            .collapsed();
            status.rows.push(Row::info("source", "No percentage source available", if primary.product_id == 0x0a79 { "Neither the native PC HID battery getter nor a matching kernel power_supply provided a real percentage. See Power for native read status; no percentage is inferred from the USB connection." } else if controller { "No battery power_supply with a real capacity attribute belongs to this physical USB device. xpad input and receiver presence do not provide battery percentage. A compatible verified GIP battery driver is required." } else { "OpenRazer getBattery is unavailable or returned its unknown sentinel; receiver/dock presence and USB descriptor revision are not battery readings." }));
            diagnostics.push(status.id.clone());
            groups.push(status);
        }
        if matches!(kind, "mouse" | "controller" | "keyboard" | "keypad") {
            let mapping = remapper::group(&id, &components, controller);
            if !mapping.rows.is_empty() {
                settings
                    .entry(if controller { "macros" } else { "buttons" })
                    .or_default()
                    .push(mapping.id.clone());
                groups.push(mapping);
            }
        }
        if mouse && !settings.contains_key("sensitivity") {
            let mut unavailable = Group::new(
                format!("{id}:capabilities"),
                "Mouse sensitivity",
                "Sensitivity configuration requires a readable supported OpenRazer API",
            );
            unavailable.rows.push(Row::info("missing", "DPI and polling controls unavailable", "The mouse remains usable with its current native input setup. Other exported lighting and power controls remain available in their own sections; no unsupported/default sensitivity controls are fabricated."));
            settings
                .entry("sensitivity")
                .or_default()
                .push(unavailable.id.clone());
            groups.push(unavailable);
            if let Some(setup) = &setup_id {
                settings.entry("overview").or_default().push(setup.clone());
            }
        }
        if !controller && !settings.contains_key("lighting") {
            let mut lighting = Group::new(format!("{id}:lighting-status"), "Lighting", "");
            lighting.rows.push(Row::info(
                "status",
                "Lighting controls unavailable",
                if mouse || dock {
                    "Ensure openrazer-daemon is running for Chroma lighting controls."
                } else {
                    "No OpenRazer lighting interface is exported for this device."
                },
            ));
            settings
                .entry("lighting")
                .or_default()
                .push(lighting.id.clone());
            if dock && let Some(setup) = &setup_id {
                settings.entry("lighting").or_default().push(setup.clone());
            }
            groups.push(lighting);
        }
        if !controller && summary.title != display_name {
            summary.title = display_name.clone();
        }
        if !summary.rows.is_empty() {
            let summary_id = summary.id.clone();
            groups.push(summary);
            let overview = settings.entry("overview").or_default();
            if controller {
                overview.push(summary_id);
            } else {
                overview.insert(0, summary_id);
            }
        }
        let overview = settings.remove("overview").unwrap_or_default();
        let mut sections = vec![section("overview", "Overview", overview)];
        for (section_id, title) in [
            ("controls", "Buttons"),
            ("macros", "Keys & Macros"),
            ("sensitivity", "Sensitivity"),
            ("lighting", "Lighting"),
            ("power", "Power"),
            ("buttons", "Buttons & Macros"),
            ("tools", "Tools"),
            ("firmware", "Firmware"),
        ] {
            if let Some(ids) = settings.remove(section_id) {
                sections.push(section(section_id, title, ids));
            }
        }
        sections.push(section("diagnostics", "Diagnostics", diagnostics));
        products.push(Product {
            id,
            name: display_name,
            kind: kind.into(),
            description: kind_description(kind, basilisk).into(),
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
    let url = match device.product_id {
        0x0a3f => "https://mysupport.razer.com/app/answers/detail/a_id/14587",
        0x0a76 | 0x0a78 | 0x0a79 => crate::firmware::V4_VENDOR_PAGE,
        _ => anyhow::bail!(
            "These verified support instructions apply only to Wolverine V3 Pro (0a3f) and Wolverine V4 Pro (0a76/0a78/0a79)"
        ),
    };
    let connection = zbus::Connection::session().await?;
    let options: BTreeMap<&str, zbus::zvariant::Value<'_>> = BTreeMap::new();
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        connection.call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.OpenURI"),
            "OpenURI",
            &("", url, options),
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
            device(0x0a76, "1-8"),
            device(0x1234, "1-5"),
        ];
        let sets = product_sets(&devices);
        assert_eq!(sets.len(), 4);
        assert_eq!(sets[0].0, "basilisk-ultimate");
        assert_eq!(sets[0].1, vec![0, 2, 1]);
        assert!(sets.iter().any(|(id, _)| id == "wolverine-1-4"));
        assert!(sets.iter().any(|(id, _)| id == "wolverine-1-8"));
        assert!(sets.iter().any(|(id, _)| id == "peripheral-1-5"));
        let ambiguous = vec![
            device(0x0088, "1-1"),
            device(0x0088, "1-2"),
            device(0x007e, "1-3"),
        ];
        assert_eq!(product_sets(&ambiguous).len(), 3);
    }

    #[test]
    fn wolverine_v4_and_openrazer_categories_resolve_rich_product_kinds() {
        let v4 = device(0x0a76, "1-8");
        assert!(v4.is_controller());
        assert!(v4.is_wolverine_v4());
        assert!(v4.has_rear_buttons());
        assert_eq!(v4.controller_display_name(), "Wolverine V4 Pro");
        assert_eq!(v4.inferred_kind(), "controller");
        let v4_pc = device(0x0a78, "1-8");
        assert!(v4_pc.is_controller());
        assert_eq!(
            v4_pc.controller_display_name(),
            "Wolverine V4 Pro (PC XInput)"
        );
        let v4_hid = device(0x0a79, "1-8");
        assert!(v4_hid.is_wolverine_v4());
        assert_eq!(
            v4_hid.controller_display_name(),
            "Wolverine V4 Pro (PC HID)"
        );
        assert_eq!(device(0x025a, "1-2").inferred_kind(), "keyboard");
        assert_eq!(device(0x0244, "1-3").inferred_kind(), "keypad");
        assert_eq!(device(0x0527, "1-4").inferred_kind(), "headset");
        assert_eq!(device(0x0c04, "1-5").inferred_kind(), "mousemat");
        assert_eq!(device(0x0f08, "1-6").inferred_kind(), "accessory");
        assert_eq!(device(0x00a4, "1-7").inferred_kind(), "dock");
    }
}
