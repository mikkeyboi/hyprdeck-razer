mod cache;
mod firmware;
mod input;
mod openrazer;
mod usb;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{self, Read, Write};
use std::time::Duration;

#[derive(Debug, Deserialize)]
struct Request {
    api_version: u32,
    method: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default = "empty_args")]
    args: Value,
}
fn empty_args() -> Value {
    json!({})
}

#[derive(Debug, Serialize)]
pub struct State {
    title: String,
    description: String,
    groups: Vec<Group>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Group {
    id: String,
    title: String,
    description: String,
    rows: Vec<Row>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Row {
    id: String,
    title: String,
    subtitle: String,
    control: Option<Value>,
}
impl Row {
    pub fn info(
        id: impl Into<String>,
        title: impl Into<String>,
        subtitle: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            subtitle: subtitle.into(),
            control: None,
        }
    }
    pub fn controlled(mut self, control: Value) -> Self {
        self.control = Some(control);
        self
    }
}
impl Group {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: description.into(),
            rows: Vec::new(),
        }
    }
}
pub fn string_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("args.{key} must be a string"))
}

async fn state() -> Result<State> {
    let devices = usb::devices()?;
    let mut groups = Vec::new();
    for device in &devices {
        groups.push(device.group());
        if let Some(result) = cache::load("firmware", &device.id) {
            groups.push(result);
        }
        for node in input::nodes(device)? {
            groups.push(input::snapshot(&node));
            if let Some(result) = cache::load("measurement", &node.id) {
                groups.push(result);
            }
        }
    }
    if devices.is_empty() {
        let mut group = Group::new("usb", "USB devices", "Read-only Linux sysfs discovery");
        group.rows.push(Row::info("none", "No connected Razer USB device", "No device with USB vendor 1532 is currently enumerated. Reconnect and refresh; no stale device records are retained."));
        groups.push(group);
    }
    let (openrazer, firmware) = tokio::join!(openrazer::state(), firmware::state());
    groups.extend(openrazer);
    groups.extend(firmware);
    Ok(State {
        title: "Razer peripherals".into(),
        description: "Live sysfs/evdev diagnostics and capabilities exported by OpenRazer. Reads never grab input devices or change settings. USB scheduling, configured rate and measured changed-event cadence are distinct.".into(),
        groups,
    })
}

async fn dispatch(request: Request) -> Result<State> {
    if request.api_version != 1 {
        bail!(
            "Unsupported api_version {}; expected 1",
            request.api_version
        );
    }
    if !request.args.is_object() {
        bail!("args must be a JSON object");
    }
    match request.method.as_str() {
        "state" => {
            if request.action.is_some() {
                bail!("state requests must not include an action");
            }
            state().await
        }
        "action" => {
            let action = request
                .action
                .as_deref()
                .context("action method requires action")?;
            let result = match action {
                "measure" => {
                    let device = string_arg(&request.args, "device")?.to_owned();
                    let seconds = request
                        .args
                        .get("seconds")
                        .map(|value| value.as_u64().context("seconds must be an integer"))
                        .transpose()?
                        .unwrap_or(4);
                    if !(1..=8).contains(&seconds) {
                        bail!("measurement seconds must be 1..=8 to fit the host request deadline");
                    }
                    Some(
                        tokio::task::spawn_blocking(move || {
                            input::measure(&device, Duration::from_secs(seconds))
                        })
                        .await??,
                    )
                }
                "openrazer-call" => Some(openrazer::action(&request.args).await?),
                "firmware-check" => Some(firmware::vendor_check(&request.args).await?),
                "vendor-page" => {
                    firmware::open_page(&request.args).await?;
                    None
                }
                _ => bail!(
                    "Unknown action {action:?}; firmware flashing is only available through the explicitly confirmed manual CLI"
                ),
            };
            let mut refreshed = state().await?;
            if let Some(group) = result {
                refreshed
                    .groups
                    .retain(|current| current.id != format!("cached-{}", group.id));
                refreshed.groups.insert(0, group);
            }
            Ok(refreshed)
        }
        _ => bail!(
            "Unknown method {:?}; expected state or action",
            request.method
        ),
    }
}

fn cli_request(arguments: &[String]) -> Result<Request> {
    let (action, args) = match arguments.first().map(String::as_str) {
        Some("state") if arguments.len() == 1 => {
            return Ok(Request {
                api_version: 1,
                method: "state".into(),
                action: None,
                args: empty_args(),
            });
        }
        Some("measure") if (2..=3).contains(&arguments.len()) => (
            "measure",
            json!({"device": arguments[1], "seconds": arguments.get(2).map(|s| s.parse::<u64>()).transpose()?.unwrap_or(4)}),
        ),
        Some("firmware-check") if arguments.len() == 2 => {
            ("firmware-check", json!({"device": arguments[1]}))
        }
        Some("action") if (2..=3).contains(&arguments.len()) => (
            arguments[1].as_str(),
            arguments
                .get(2)
                .map(|s| serde_json::from_str(s))
                .transpose()?
                .unwrap_or_else(empty_args),
        ),
        _ => bail!(
            "Usage: hyprdeck-razer state | measure <input-device-id> [seconds 1..8] | firmware-check <usb-device-id> | action <action> [JSON-object] | firmware-update <fwupd-device-id> --confirm | --request"
        ),
    };
    Ok(Request {
        api_version: 1,
        method: "action".into(),
        action: Some(action.into()),
        args,
    })
}

#[tokio::main]
async fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().map(String::as_str) == Some("firmware-update") {
        let result = if arguments.len() == 3 && arguments[2] == "--confirm" {
            firmware::update(&arguments[1]).await
        } else {
            Err(anyhow::anyhow!(
                "Firmware update requires: firmware-update <fwupd-device-id> --confirm. This may modify firmware and requires uninterrupted power; never run it under the host's 15-second request timeout."
            ))
        };
        if let Err(error) = result {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
        return;
    }
    if arguments.is_empty()
        || matches!(arguments.first().map(String::as_str), Some("--help" | "-h"))
    {
        println!(
            "hyprdeck-razer {}\n\nstate\nmeasure <input-device-id> [seconds 1..8]\nfirmware-check <usb-device-id>\naction <action> [JSON-object]\nfirmware-update <fwupd-device-id> --confirm\n--request (one version-1 JSON request on stdin)\n\nUse state to discover device IDs, input permissions and available actions. Measurement requires moving controls during the sample. No device is grabbed or driver detached.",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    if arguments == ["--version"] {
        println!("hyprdeck-razer {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let request_mode = arguments == ["--request"];
    let request = if request_mode {
        let mut data = Vec::new();
        io::stdin()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut data)
            .context("Reading request")
            .and_then(|_| {
                if data.len() > 1024 * 1024 {
                    bail!("Request exceeds 1 MiB");
                }
                serde_json::from_slice(&data).context("Invalid request JSON")
            })
    } else {
        cli_request(&arguments)
    };
    // Reserve time for the host to receive an explicit error instead of killing a hung bus request.
    let result = match request {
        Ok(request) => tokio::time::timeout(Duration::from_secs(13), dispatch(request))
            .await
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "Backend request exceeded 13 seconds; device/daemon may be unresponsive"
                ))
            }),
        Err(error) => Err(error),
    };
    let failed = result.is_err();
    let response = match result {
        Ok(state) => json!({"api_version": 1, "error": null, "state": state}),
        Err(error) => json!({"api_version": 1, "error": format!("{error:#}"), "state": null}),
    };
    let mut stdout = io::stdout().lock();
    let written = if request_mode {
        serde_json::to_writer(&mut stdout, &response)
    } else {
        serde_json::to_writer_pretty(&mut stdout, &response)
    };
    if let Err(error) = written {
        eprintln!("Writing response: {error}");
        std::process::exit(1);
    }
    let _ = writeln!(stdout);
    if failed && !request_mode {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn invalid_protocol_rejected_before_hardware() {
        let request =
            serde_json::from_value(json!({"api_version":2,"method":"state","args":{}})).unwrap();
        assert!(dispatch(request).await.is_err());
        let request =
            serde_json::from_value(json!({"api_version":1,"method":"unknown","args":{}})).unwrap();
        assert!(dispatch(request).await.is_err());
    }
    #[tokio::test]
    async fn invalid_action_boundaries_do_not_touch_hardware() {
        for request in [
            json!({"api_version":1,"method":"action","action":"unknown","args":{}}),
            json!({"api_version":1,"method":"state","args":[]}),
            json!({"api_version":1,"method":"action","action":"measure","args":{"device":"missing","seconds":0}}),
            json!({"api_version":1,"method":"action","action":"measure","args":{"device":"missing","seconds":9}}),
        ] {
            assert!(
                dispatch(serde_json::from_value(request).unwrap())
                    .await
                    .is_err()
            );
        }
    }
}
