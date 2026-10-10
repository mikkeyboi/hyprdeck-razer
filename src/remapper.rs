use crate::{Group, Row, input, string_arg, usb};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn executable(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|path| path.join(name))
            .find(|path| {
                fs::metadata(path).is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
    })
}
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|root| root.join("input-remapper-2"))
}
fn preset_dir(device: &usb::Device) -> Option<PathBuf> {
    // Upstream groups interfaces by physical device and uses its shortest input name
    // as the preset model folder. Actual CLI device selection uses an event path,
    // never a guessed/remapper display name or an unrelated keyboard.
    let nodes = input::nodes(device).ok()?;
    let name = nodes
        .iter()
        .map(|node| node.name.as_str())
        .min_by_key(|name| name.len())?;
    let name: String = name
        .chars()
        .map(|character| {
            if "/\\?%*:|\"<>".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect();
    Some(config_dir()?.join("presets").join(name))
}
fn presets(device: &usb::Device) -> Vec<String> {
    let Some(directory) = preset_dir(device) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()?.to_str()? != "json" {
                return None;
            }
            let value: Value = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
            if !value.is_array() {
                return None;
            }
            path.file_stem()?.to_str().map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}
pub fn group(id: &str, devices: &[&usb::Device], controller: bool) -> Group {
    let mut group = Group::new(
        format!("{id}:mapping"),
        "Buttons & macros",
        "Software remapping is separate from onboard Razer profiles",
    );
    group.rows.push(Row::info("onboard", "Onboard assignments unavailable", if controller { "xpad exposes mapped standard inputs, not independent rear-button assignments or onboard profile writes. Use Razer Controller Setup for Xbox on Windows/Xbox for device profiles. Razer's original Wolverine FAQ explicitly excludes onboard macros; software macros below are a separate optional tool." } else { "OpenRazer's Basilisk Ultimate METHODS list exposes DPI, lighting and power, but no button remapping or macros. Installing the daemon does not add those unsupported APIs." }));
    let installed = executable("input-remapper-gtk").is_some();
    let mut row = Row::info(
        "editor",
        if installed {
            "Input Remapper · mapping & macro editor"
        } else {
            "Optional Linux mapping & macros setup"
        },
        "Input Remapper supports Wayland/X11, button combinations and programmable macros via its privileged service and virtual input devices. Select this physical product in its editor, record the actual input, create/save a preset and Apply explicitly. It may grab/remap the device while active; unlike this plugin's read-only diagnostics. Stop in the editor to restore original input. Hidden mouse keyboard interfaces are part of the same product, not extra keyboards.",
    );
    if installed {
        row.control = Some(
            json!({"kind":"button","label":"Edit mappings & macros","action":"input-remapper","args":{"operation":"editor"},"destructive":false}),
        );
    } else {
        row.subtitle.push_str(" Install input-remapper (Arch: upstream AUR input-remapper-git) and follow its service/authorization setup. This plugin never auto-installs it.");
    }
    group.rows.push(row);
    if executable("input-remapper-control").is_some() {
        for device in devices {
            if !device.is_controller() && !matches!(device.product_id, 0x0086 | 0x0088) {
                continue;
            }
            let available = presets(device);
            if available.is_empty() {
                continue;
            }
            let mut options = vec![json!({"value":"","label":"Choose a saved preset"})];
            options.extend(
                available
                    .into_iter()
                    .map(|name| json!({"value":name,"label":name})),
            );
            group.rows.push(Row::info(format!("presets:{}", device.port), "Apply a saved software preset", "Existing Input Remapper v2 preset files for this physical device. Explicit application can alter/grab input; this is not an onboard profile. Service and input permissions are required.").controlled(json!({"kind":"form","fields":[{"id":"preset","label":"Saved mapping / macro preset","kind":"choice","value":"","options":options}],"action":"input-remapper","args":{"operation":"start","device":device.id},"label":"Apply preset"})));
            group.rows.push(Row::info(format!("stop:{}", device.port), "Restore original software input", "Stops only this physical device's Input Remapper injection, not all system mappings.").controlled(json!({"kind":"button","label":"Stop software mapping","action":"input-remapper","args":{"operation":"stop","device":device.id},"destructive":false})));
        }
    }
    group
}
pub async fn action(args: &Value) -> Result<()> {
    match string_arg(args, "operation")? {
        "editor" => {
            let binary = executable("input-remapper-gtk")
                .context("Install Input Remapper's actual GUI before editing mappings/macros")?;
            Command::new(binary)
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context(
                    "Launching Input Remapper; its own service authentication remains required",
                )?;
        }
        operation @ ("start" | "stop") => {
            let device = usb::find(string_arg(args, "device")?)?;
            if !device.is_controller() && !matches!(device.product_id, 0x0086 | 0x0088) {
                bail!("Remapping action is restricted to connected mouse/controller products");
            }
            let binary = executable("input-remapper-control")
                .context("Input Remapper control command is not installed")?;
            let nodes = input::nodes(&device)?;
            let node = nodes
                .first()
                .context("No current physical input node for this product")?;
            let mut command = tokio::process::Command::new(binary);
            command.kill_on_drop(true);
            command
                .args(["--command", operation, "--device"])
                .arg(&node.path);
            if operation == "start" {
                let preset = args
                    .get("value")
                    .and_then(|value| value.get("preset"))
                    .and_then(Value::as_str)
                    .context("Form args.value.preset must name an existing preset")?;
                if preset.is_empty() || !presets(&device).iter().any(|name| name == preset) {
                    bail!("Select an existing device-specific preset");
                }
                command.arg("--preset").arg(preset);
            }
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                command.stdin(Stdio::null()).output(),
            )
            .await
            .context("Input Remapper service timed out")?
            .context("Communicating with the actual Input Remapper service")?;
            if !output.status.success() {
                bail!(
                    "Input Remapper failed: {} {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            // The upstream CLI starts asynchronously; do not equate exit success
            // with proven injection/hardware state. The editor shows service state.
        }
        _ => bail!("Unknown Input Remapper action"),
    }
    Ok(())
}
