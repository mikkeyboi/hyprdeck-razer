use crate::{Group, Row, string_arg, usb};
use anyhow::{Context, Result, bail};
use reqwest::{Client, Url, redirect::Policy};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use zbus::{Connection, zvariant::OwnedValue};

pub const VENDOR_PAGE: &str = "https://mysupport.razer.com/app/answers/detail/a_id/14630/~/razer-wolverine-v3-pro-firmware-updater-%7C-rz06-0520";
const DOWNLOAD: &str = "https://rzr.to/b556gr";
const FWUPD: &str = "org.freedesktop.fwupd";
type Properties = HashMap<String, OwnedValue>;

fn trusted(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some("rzr.to" | "dl.razerzone.com" | "mysupport.razer.com")
        )
}
fn updater_version(url: &Url) -> Result<String> {
    if !trusted(url)
        || url.host_str() != Some("dl.razerzone.com")
        || !url.path().starts_with("/drivers/WolverineV3Pro/win/")
    {
        bail!(
            "Official redirect did not resolve to the verified original Wolverine Windows updater path"
        );
    }
    let name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .context("Missing updater filename")?;
    let version = name
        .strip_prefix("WolverineV3Pro_0A3F_FirmwareUpdater_v")
        .and_then(|value| value.strip_suffix(".exe"))
        .context("Official updater filename changed; do not infer firmware version")?;
    if version.is_empty()
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'r'))
    {
        bail!("Unrecognized updater version syntax");
    }
    Ok(version.to_owned())
}
pub async fn vendor_check(args: &Value) -> Result<Group> {
    let id = string_arg(args, "device")?;
    usb::require_original(id)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .user_agent("hyprdeck-razer/0.1.0")
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= 6 {
                attempt.error("Too many official redirects")
            } else if trusted(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("Refusing non-official firmware redirect")
            }
        }))
        .build()?;
    let response = client.head(DOWNLOAD).send().await.context("Official firmware check unavailable; last manually verified 2026-10-09 was updater v2.04.00_r1 (not a live latest-version result)")?.error_for_status()?;
    let version = updater_version(response.url())?;
    let mut group = Group::new(
        format!("firmware-check:{id}"),
        "Available official vendor updater",
        "Live HTTPS HEAD redirect check; no executable downloaded or run",
    );
    group.rows.push(Row::info("updater", "Available Windows updater version", format!("v{version}. Available vendor updater, NOT a claim that it is newer than installed firmware. Installed controller firmware cannot be read via standard xpad/evdev; bcdDevice is not compared.")));
    group.rows.push(Row::info(
        "download",
        "Official download",
        response.url().as_str(),
    ));
    group.rows.push(Row::info("instructions", "Windows prerequisite / explicit manual update", format!("Use a Windows PC and follow {VENDOR_PAGE}; close applications using the controller, maintain power and follow the updater's dongle/controller connection prompts. Hyprdeck does not launch Wine, flash packets or change xpad.")));
    crate::cache::store("firmware", id, &group)?;
    Ok(group)
}
pub async fn open_page(args: &Value) -> Result<()> {
    usb::require_original(string_arg(args, "device")?)?;
    // Portal receives the request over D-Bus, so no spawned opener can be killed
    // with the host's subprocess process group. Completion means portal acceptance.
    let connection = Connection::session().await?;
    let options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
    let reply = tokio::time::timeout(
        Duration::from_secs(3),
        connection.call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.OpenURI"),
            "OpenURI",
            &("", VENDOR_PAGE, options),
        ),
    )
    .await
    .context("Desktop portal OpenURI timeout")??;
    let _: zbus::zvariant::OwnedObjectPath = reply.body().deserialize()?;
    Ok(())
}
async fn bus() -> Result<Connection> {
    tokio::time::timeout(Duration::from_secs(1), Connection::system())
        .await
        .context("fwupd system bus timeout")?
        .context("fwupd system bus unavailable")
}
async fn devices(connection: &Connection) -> Result<Vec<Properties>> {
    // Avoid service activation on every state refresh. Absence is a useful state,
    // not permission to start or install firmware services implicitly.
    let names = zbus::fdo::DBusProxy::new(connection).await?;
    if !names.name_has_owner(FWUPD.try_into()?).await? {
        bail!(
            "fwupd daemon is not running; install/start fwupd deliberately if LVFS device support is needed"
        );
    }
    let reply = tokio::time::timeout(
        Duration::from_secs(1),
        connection.call_method(Some(FWUPD), "/", Some(FWUPD), "GetDevices", &()),
    )
    .await
    .context("fwupd GetDevices timeout")??;
    Ok(reply.body().deserialize()?)
}
fn string(properties: &Properties, key: &str) -> Option<String> {
    properties
        .get(key)
        .and_then(|value| <&str>::try_from(value).ok())
        .map(str::to_owned)
}
fn flags(properties: &Properties) -> u64 {
    properties
        .get("Flags")
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(0)
}
fn razer(properties: &Properties) -> bool {
    string(properties, "VendorId").is_some_and(|id| {
        id.split('|')
            .any(|id| id.eq_ignore_ascii_case("USB:0x1532"))
    })
}
fn supported(properties: &Properties) -> bool {
    // Updatable and metadata-supported; no historical, inhibited, wildcard,
    // unreachable or emulated targets. Wildcard install may flash other devices.
    let flags = flags(properties);
    let excluded = (1_u64 << 22) | (1_u64 << 37) | (1_u64 << 42) | (1_u64 << 44) | (1_u64 << 49);
    razer(properties) && flags & (1 << 1) != 0 && flags & (1 << 5) != 0 && flags & excluded == 0
}
pub async fn state() -> Vec<Group> {
    let mut group = Group::new(
        "fwupd",
        "Firmware support",
        "fwupd/LVFS read-only discovery; no automatic update",
    );
    let result = async {
        let connection = bus().await?;
        devices(&connection).await
    }
    .await;
    match result {
        Err(error) => group.rows.push(Row::info("status", "fwupd unavailable", format!("{error:#}. Installing fwupd does not imply Razer support; only currently enumerated USB:0x1532 devices with metadata-supported and updatable flags qualify. Original Wolverine official updater requires Windows."))),
        Ok(devices) => {
            for device in devices.iter().filter(|device| razer(device)) {
                let id = string(device,"DeviceId").unwrap_or_else(|| "Unknown device ID".into());
                let name = string(device,"Name").unwrap_or_else(|| "Razer firmware device".into());
                let version = string(device,"Version").unwrap_or_else(|| "Unavailable".into());
                group.rows.push(Row::info(&id, name, if supported(device) {
                    format!("Installed firmware from fwupd: {version}; metadata-supported, updatable. To check/install device-specific upgrades deliberately, use: hyprdeck-razer firmware-update {id} --confirm . This is CLI-only to avoid the host killing a firmware flash at 15 seconds. fwupd still enforces power/authentication/device requirements.")
                } else { format!("fwupd firmware: {version}; no eligible metadata-supported, reachable, real, nonwildcard/updatable firmware path currently advertised (may be inhibited). No flash action exposed.") }));
            }
            if group.rows.is_empty() { group.rows.push(Row::info("none", "No supported Razer firmware devices", "fwupd is running but enumerates no USB:0x1532 firmware devices. This is not evidence of newer or current firmware; use the verified vendor path for original Wolverine.")); }
        }
    }
    vec![group]
}
pub async fn update(id: &str) -> Result<()> {
    if id.len() != 40 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!(
            "fwupd device ID must be the 40-character hexadecimal DeviceId discovered in state, never a wildcard or option"
        );
    }
    let connection = bus().await?;
    let devices = devices(&connection).await?;
    let device = devices
        .iter()
        .find(|device| string(device, "DeviceId").as_deref() == Some(id))
        .context("Device is not currently enumerated by fwupd")?;
    if !supported(device) {
        bail!(
            "Device is not a currently connected metadata-supported/updatable Razer USB device; refusing firmware update"
        );
    }
    let reply = tokio::time::timeout(
        Duration::from_secs(10),
        connection.call_method(Some(FWUPD), "/", Some(FWUPD), "GetUpgrades", &(id,)),
    )
    .await
    .context("Device upgrade discovery timeout")??;
    let upgrades: Vec<Properties> = reply.body().deserialize()?;
    if upgrades.is_empty() {
        bail!("fwupd advertises no upgrades for this device; nothing to flash");
    }
    eprintln!(
        "Explicit confirmation accepted for fwupd device {id}. Keep power connected and do not interrupt. Only this device is passed to fwupdmgr; no force, downgrade or whole-system update flags."
    );
    let status = tokio::process::Command::new("fwupdmgr")
        .arg("update")
        .arg(id)
        .status()
        .await
        .context("Launching fwupdmgr; install its distribution package if absent")?;
    if !status.success() {
        bail!("fwupdmgr exited with {status}; review its diagnostic output before retrying");
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redirect_identity_is_not_guessable() {
        let good = Url::parse("https://dl.razerzone.com/drivers/WolverineV3Pro/win/C883CEA1200064BDC0285A18CA39CDED/WolverineV3Pro_0A3F_FirmwareUpdater_v2.04.00_r1.exe").unwrap();
        assert_eq!(updater_version(&good).unwrap(), "2.04.00_r1");
        assert!(
            updater_version(
                &Url::parse(
                    "https://evil.example/WolverineV3Pro_0A3F_FirmwareUpdater_v2.04.00_r1.exe"
                )
                .unwrap()
            )
            .is_err()
        );
        assert!(!trusted(&Url::parse("http://rzr.to/b556gr").unwrap()));
        assert!(
            updater_version(
                &Url::parse("https://dl.razerzone.com/drivers/Other/win/updater.exe").unwrap()
            )
            .is_err()
        );
    }
}
