use crate::{Group, Row, input, string_arg, usb};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

// Embedded adapter uses the installed backend's discovery, schema/parser and
// daemon API. It does not implement input injection or assume event-node order.
const BACKEND: &str = include_str!("input_remapper.py");
const INSTALL: &str = "Input Remapper v2 is optional software, not onboard memory. On Arch, review/build the AUR input-remapper-git package (https://aur.archlinux.org/packages/input-remapper-git). It requires its GTK editor, Python backend, system-bus service and input/uinput access. Hyprdeck never installs it, enables a service or changes autoload settings.";

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

fn python() -> Result<PathBuf> {
    // Distribution-installed Input Remapper needs the system Python, not an
    // activated virtualenv with a different set of packages.
    let system = PathBuf::from("/usr/bin/python3");
    if system.is_file() {
        return Ok(system);
    }
    executable("python3").context("Python 3 is required by Input Remapper")
}

fn decode(output: std::process::Output) -> Result<Value> {
    let value: Value = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "Input Remapper backend returned no usable response: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        bail!("Input Remapper: {error}");
    }
    if !output.status.success() {
        bail!(
            "Input Remapper backend failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(value)
}

fn discover(paths: &[String]) -> Result<Value> {
    decode(
        Command::new(python()?)
            .args(["-I", "-c", BACKEND])
            .arg(json!({"operation":"discover","paths":paths}).to_string())
            .stdin(Stdio::null())
            .output()
            .context("Reading installed Input Remapper device groups and daemon state")?,
    )
}

fn device_paths(device: &usb::Device) -> Result<Vec<String>> {
    Ok(input::nodes(device)?
        .into_iter()
        .map(|node| node.path.to_string_lossy().into_owned())
        .collect())
}

fn supports_remapping(device: &usb::Device) -> bool {
    device.is_controller() || matches!(device.inferred_kind(), "mouse" | "keyboard" | "keypad")
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn backend_button(operation: &str, device: &usb::Device, key: &str, label: &str) -> Value {
    json!({
        "kind":"button", "label":label, "action":"input-remapper",
        "args":{"operation":operation,"device":device.id,"group":key},
        "destructive":false
    })
}

pub fn group(id: &str, devices: &[&usb::Device], controller: bool) -> Group {
    let mut group = Group::new(
        format!("{id}:mapping"),
        "Software macros",
        if controller {
            "For paddles, first assign distinct F13–F18 keys above, then choose their keyboard events as triggers. Software presets run on Linux, not in onboard memory."
        } else {
            "Input Remapper software presets run on Linux, not in onboard memory. Save here; choose the device and apply or stop in its editor."
        },
    );
    let gui = executable("input-remapper-gtk").is_some();
    if executable("input-remapper-control").is_none() {
        group.rows.push(Row::info(
            "availability",
            "Input Remapper backend is not installed",
            INSTALL,
        ));
        return group;
    }
    if !gui {
        group.rows.push(Row::info(
            "gui-unavailable",
            "Input Remapper GTK editor is missing",
            "Complete the Input Remapper installation for recording combinations and editing full presets. The native form below can create a single-key/button programmable macro preset when backend discovery is available.",
        ));
    }
    let mut product_paths = Vec::new();
    for device in devices {
        if !supports_remapping(device) {
            continue;
        }
        match device_paths(device) {
            Ok(paths) => product_paths.push((*device, paths)),
            Err(error) => group.rows.push(Row::info(
                format!("nodes:{}", device.port),
                "Physical input discovery unavailable",
                error.to_string(),
            )),
        }
    }
    let paths: Vec<String> = product_paths
        .iter()
        .flat_map(|(_, paths)| paths.iter().cloned())
        .collect();
    let discovered = match discover(&paths) {
        Ok(value) => value,
        Err(error) => {
            group.rows.push(Row::info(
                "discovery-error",
                "Input Remapper backend unavailable",
                format!("{error}. Check the distribution Python dependencies and input-node read permissions; no mapping was started. {INSTALL}"),
            ));
            return group;
        }
    };
    let Some(backends) = discovered.get("groups").and_then(Value::as_array) else {
        group.rows.push(Row::info("discovery-error", "Input Remapper discovery failed", "The installed backend did not return device groups. Check its version and dependencies."));
        return group;
    };
    if backends.is_empty() {
        group.rows.push(Row::info(
            "no-backend-input",
            "No readable Input Remapper group for this product",
            "The installed backend did not discover the product's physical input nodes. Check input permissions and connection mode. A controller may expose duplicated controller assignments rather than independent paddle events; no unrelated keyboard or first event node will be selected as a fallback.",
        ));
    }
    for backend in backends {
        let backend_paths = backend.get("paths").and_then(Value::as_array);
        let Some((device, _)) = product_paths.iter().find(|(_, paths)| {
            backend_paths.is_some_and(|found| {
                found.iter().any(|path| {
                    path.as_str()
                        .is_some_and(|path| paths.iter().any(|own| own == path))
                })
            })
        }) else {
            continue;
        };
        let key = text(backend, "key");
        let row_id = format!("{}:{key}", device.port);
        if gui {
            group.rows.push(Row::info(
                format!("editor:{row_id}"),
                "Advanced keyboard & macro editor",
                format!("Record combinations, edit presets, and apply or stop mappings. Select “{key}” yourself; administrator authentication may be required. Hyprdeck does not start/stop root injectors through ambiguous device-name keys."),
            ).controlled(backend_button("editor", device, key, "Open Input Remapper editor")));
        }
        if let Some(options) = backend
            .get("inputs")
            .and_then(Value::as_array)
            .filter(|options| !options.is_empty())
        {
            let mut choices =
                vec![json!({"value":"","label":"Choose an advertised input key/button"})];
            choices.extend(options.iter().cloned());
            group.rows.push(Row::info(
                format!("create:{row_id}"),
                "Create a programmable macro",
                "Choose an advertised trigger and write a macro, for example key(KEY_A).wait(100).key(KEY_B). The installed parser validates it. Save creates a new preset without applying or overwriting anything.",
            ).controlled(json!({
                "kind":"form", "label":"Save macro preset", "action":"input-remapper",
                "args":{"operation":"save","device":device.id,"group":key},
                "fields":[
                    {"id":"name","label":"New preset name (no dots)","kind":"text","value":"Hyprdeck macro"},
                    {"id":"input","label":"Trigger key / button (searchable)","kind":"choice","value":"","options":choices},
                    {"id":"macro","label":"Keyboard symbol / Input Remapper macro","kind":"text","value":"key(KEY_A).wait(100).key(KEY_B)"}
                ]
            })));
        } else {
            group.rows.push(Row::info(
                format!("create-unavailable:{row_id}"),
                "Native macro trigger selection unavailable",
                format!("No readable advertised key/button inputs: {}. Use the full editor to record analog inputs or combinations. No independent paddle events are inferred.", text(backend, "input_error")),
            ));
        }
        let count = backend
            .get("presets")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        group.rows.push(Row::info(
            format!("presets:{row_id}"),
            format!("{count} saved software presets"),
            "Open Input Remapper to browse, edit, apply or stop presets, including names created in its editor. Saving here never injects keys, enables autoload or changes another device's running mappings.",
        ));
    }
    group
}

pub async fn action(args: &Value) -> Result<()> {
    let operation = string_arg(args, "operation")?;
    if !matches!(operation, "editor" | "save") {
        bail!("Unknown Input Remapper action");
    }
    let device = usb::find(string_arg(args, "device")?)?;
    if !supports_remapping(&device) {
        bail!("Remapping is restricted to connected mouse/keyboard/keypad/controller products");
    }
    let paths = device_paths(&device)?;
    if paths.is_empty() {
        bail!("No physical input nodes for this connected product");
    }
    let key = string_arg(args, "group")?;
    let mut request = json!({"operation":operation,"paths":paths,"group":key});
    if operation == "editor" {
        // There is no --device flag in upstream input-remapper-gtk. Validate the
        // scoped group, then launch without inventing one or altering global config.
        let discovered = backend_action(&json!({"operation":"discover","paths":paths})).await?;
        if !discovered
            .get("groups")
            .and_then(Value::as_array)
            .is_some_and(|groups| groups.iter().any(|group| text(group, "key") == key))
        {
            bail!("Selected Input Remapper group is no longer connected; refresh devices");
        }
        let binary = executable("input-remapper-gtk")
            .context("Install Input Remapper's actual GTK editor first")?;
        Command::new(binary)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context(
                "Launching the real Input Remapper editor; select the named device group in its UI",
            )?;
        return Ok(());
    }
    let values = args.get("value").unwrap_or(&Value::Null);
    for field in ["name", "input", "macro"] {
        request[field] = Value::String(string_arg(values, field)?.to_owned());
    }
    backend_action(&request).await?;
    Ok(())
}

async fn backend_action(request: &Value) -> Result<Value> {
    let mut command = tokio::process::Command::new(python()?);
    command.kill_on_drop(true);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        command
            .args(["-I", "-c", BACKEND])
            .arg(request.to_string())
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .context("Input Remapper timed out; refresh its daemon state before retrying")?
    .context("Communicating with the installed Input Remapper backend")?;
    decode(output)
}
