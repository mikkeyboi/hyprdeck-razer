mod cache;
mod firmware;
mod input;
mod openrazer;
mod products;
mod remapper;
mod usb;
mod wolverine;

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
    products: Vec<Product>,
    refresh_interval_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct Product {
    id: String,
    name: String,
    kind: String,
    description: String,
    status: String,
    connection: String,
    battery: Option<Battery>,
    sections: Vec<ProductSection>,
}

#[derive(Debug, Serialize)]
pub struct Battery {
    percentage: Option<f64>,
    charging: Option<bool>,
    source: String,
}

#[derive(Debug, Serialize)]
pub struct ProductSection {
    id: String,
    title: String,
    groups: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Group {
    id: String,
    title: String,
    description: String,
    rows: Vec<Row>,
    #[serde(default)]
    visualization: Option<Visualization>,
    #[serde(default)]
    collapsed: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Visualization {
    Controller {
        name: String,
        connection: String,
        status: String,
        inputs: Vec<ControllerInput>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream: Option<Value>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ControllerInput {
    id: String,
    label: String,
    detail: String,
    #[serde(default)]
    pressed: Option<bool>,
    #[serde(default)]
    value: Option<f64>,
    #[serde(default)]
    x: Option<f64>,
    #[serde(default)]
    y: Option<f64>,
    #[serde(default)]
    control: Option<Value>,
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
            visualization: None,
            collapsed: false,
        }
    }
    pub fn collapsed(mut self) -> Self {
        self.collapsed = true;
        self
    }
}
pub fn string_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("args.{key} must be a string"))
}

async fn state() -> Result<State> {
    let devices = usb::devices()?;
    let (daemon, firmware) = tokio::join!(openrazer::state(), firmware::state());
    let (mut groups, mut products) = products::assemble(&devices, daemon)?;
    for group in firmware {
        for product in &mut products {
            if let Some(section) = product
                .sections
                .iter_mut()
                .find(|section| section.id == "diagnostics")
            {
                section.groups.push(group.id.clone());
            }
        }
        groups.push(group.collapsed());
    }
    Ok(State {
        title: "Razer peripherals".into(),
        description: "Select a product for its controls, components and capability status. Raw USB/input/API records remain in Diagnostics; refresh never changes hardware settings.".into(),
        groups,
        products,
        refresh_interval_ms: 2000,
    })
}

async fn dispatch(mut request: Request) -> Result<State> {
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
                "rumble" => {
                    let device = string_arg(&request.args, "device")?.to_owned();
                    let profile = request
                        .args
                        .get("value")
                        .and_then(|v| v.get("profile"))
                        .and_then(Value::as_str)
                        .or_else(|| request.args.get("profile").and_then(Value::as_str))
                        .unwrap_or("dual")
                        .to_owned();
                    Some(
                        tokio::task::spawn_blocking(move || input::rumble(&device, &profile))
                            .await??,
                    )
                }
                "openrazer-call" => Some(openrazer::action(&request.args).await?),
                "openrazer-configure" => Some(openrazer::configure(&request.args).await?),
                "dpi-presets" => Some(openrazer::dpi_presets(&request.args).await?),
                "battery-notifications" => {
                    Some(openrazer::battery_notifications(&request.args).await?)
                }
                "wolverine-configure" => {
                    let args = request.args;
                    let (result, args) = tokio::task::spawn_blocking(move || {
                        let result = wolverine::action(&args);
                        (result, args)
                    })
                    .await?;
                    request.args = args;
                    Some(result?)
                }
                "input-remapper" => {
                    remapper::action(&request.args).await?;
                    None
                }
                "openrazer-setup" => {
                    openrazer::setup().await?;
                    None
                }
                "controller-support" => {
                    products::controller_support(&request.args).await?;
                    None
                }
                "firmware-check" => Some(firmware::vendor_check(&request.args).await?),
                "vendor-page" => {
                    firmware::open_page(&request.args).await?;
                    None
                }
                "usb-reset" => {
                    let device = string_arg(&request.args, "device")?.to_owned();
                    Some(tokio::task::spawn_blocking(move || usb::reset_device(&device)).await??)
                }
                _ => bail!(
                    "Unknown action {action:?}; firmware flashing is only available through the explicitly confirmed manual CLI"
                ),
            };
            let mut refreshed = state().await?;
            if let Some(mut group) = result {
                group.collapsed = action != "measure";
                let cached_id = format!("cached-{}", group.id);
                refreshed.groups.retain(|current| current.id != cached_id);
                if action == "measure" {
                    group.id = cached_id.clone();
                }
                for product in &mut refreshed.products {
                    for section in &mut product.sections {
                        section.groups.retain(|id| id != &cached_id);
                    }
                }
                let device = request.args.get("device").and_then(Value::as_str);
                for product in &mut refreshed.products {
                    let owns_device = device.is_some_and(|device| {
                        product
                            .sections
                            .iter()
                            .flat_map(|section| &section.groups)
                            .any(|id| {
                                id == device
                                    || id == &format!("openrazer:{device}")
                                    || id.starts_with(&format!("input:{device}:"))
                            })
                    });
                    if owns_device
                        && let Some(section) = product.sections.iter_mut().find(|section| {
                            section.id
                                == if action == "measure" {
                                    "tools"
                                } else {
                                    "diagnostics"
                                }
                        })
                    {
                        section.groups.push(group.id.clone());
                    }
                }
                refreshed.groups.push(group);
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
        Some("usb-reset") if arguments.len() == 2 => ("usb-reset", json!({"device": arguments[1]})),
        Some("rumble") if (2..=3).contains(&arguments.len()) => (
            "rumble",
            json!({"device": arguments[1], "profile": arguments.get(2).map(String::as_str).unwrap_or("dual")}),
        ),
        Some("action") if (2..=3).contains(&arguments.len()) => (
            arguments[1].as_str(),
            arguments
                .get(2)
                .map(|s| serde_json::from_str(s))
                .transpose()?
                .unwrap_or_else(empty_args),
        ),
        _ => bail!(
            "Usage: hyprdeck-razer state | measure <input-device-id> [seconds 1..8] | firmware-check <usb-device-id> | usb-reset <usb-device-id> | action <action> [JSON-object] | firmware-update <fwupd-device-id> --confirm | --request"
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
    if arguments.first().map(String::as_str) == Some("--input-stream") {
        let result = if arguments.len() == 2 {
            input::stream(&arguments[1])
        } else {
            Err(anyhow::anyhow!(
                "Usage: hyprdeck-razer --input-stream <usb-device-id>"
            ))
        };
        if let Err(error) = result {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
        return;
    }
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
            "hyprdeck-razer {}\n\nstate\nmeasure <input-device-id> [seconds 1..8]\nfirmware-check <usb-device-id>\nusb-reset <usb-device-id>\naction <action> [JSON-object]\nfirmware-update <fwupd-device-id> --confirm\n--request (one version-1 JSON request on stdin)\n\nUse state to discover device IDs, input permissions and available actions. Measurement requires moving controls during the sample. No device is grabbed or driver detached.",
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
