use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Stage {
    id: String,
    label: String,
    #[serde(deserialize_with = "stage_dpi")]
    x: u64,
    #[serde(deserialize_with = "stage_dpi")]
    y: u64,
}
fn stage_dpi<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u64, D::Error> {
    unsigned_integer(&Json::deserialize(deserializer)?).map_err(serde::de::Error::custom)
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Preferences {
    product_id: u16,
    linked: bool,
    stages: Vec<Stage>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u32,
    devices: BTreeMap<String, Preferences>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            devices: BTreeMap::new(),
        }
    }
}
fn config_path() -> Result<PathBuf> {
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("HOME / XDG_CONFIG_HOME unavailable for host DPI presets")?;
    Ok(root.join("hyprdeck-razer/dpi.json"))
}
fn validate_preferences(preferences: &Preferences, max: u64) -> Result<()> {
    if !(1..=8).contains(&preferences.stages.len()) {
        bail!("Host DPI presets require 1..=8 stages");
    }
    let mut ids = BTreeSet::new();
    for stage in &preferences.stages {
        if stage.id.is_empty()
            || stage.id.len() > 64
            || !stage.id.as_bytes()[0].is_ascii_lowercase()
            || !stage.id.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte)
            })
            || !ids.insert(&stage.id)
        {
            bail!("Host DPI stage IDs must be unique, at most 64 bytes and match [a-z][a-z0-9_-]*");
        }
        if stage.label.trim().is_empty() || stage.label.len() > 128 || stage.label.contains('\0') {
            bail!("Host DPI stage labels must be nonempty and at most 128 bytes");
        }
        if !(100..=max).contains(&stage.x) || !(100..=max).contains(&stage.y) {
            bail!("Host DPI stage values must be 100..={max}");
        }
        if preferences.linked && stage.x != stage.y {
            bail!("Linked DPI stages require equal X and Y");
        }
    }
    Ok(())
}
fn load(path: &Path) -> Result<Config> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(error).context("Reading host DPI config metadata"),
    };
    if !metadata.file_type().is_file() || metadata.len() > 1024 * 1024 {
        bail!("Host DPI config must be a regular file of at most 1 MiB");
    }
    let config: Config = serde_json::from_slice(&fs::read(path)?)
        .context("Malformed host DPI config; existing file was not overwritten")?;
    if config.version != 1 {
        bail!(
            "Unsupported host DPI config version {}; existing file was not overwritten",
            config.version
        );
    }
    for (serial, preferences) in &config.devices {
        if serial.is_empty() {
            bail!("Host DPI config contains an empty daemon serial");
        }
        validate_preferences(preferences, u16::MAX as u64)
            .with_context(|| format!("Invalid host DPI config for serial {serial:?}"))?;
    }
    Ok(config)
}
fn samples(product_id: u16, max: u64, linked: bool) -> Preferences {
    Preferences {
        product_id,
        linked,
        stages: [400, 800, 1600, 3200]
            .into_iter()
            .filter(|dpi| *dpi <= max)
            .enumerate()
            .map(|(index, dpi)| Stage {
                id: format!("stage-{}", index + 1),
                label: format!("{dpi} DPI"),
                x: dpi,
                y: dpi,
            })
            .collect(),
    }
}
type Current = (Option<(u64, u64)>, u64, u16);
fn current(device: &DeviceSnapshot) -> Result<Current> {
    method(device, DPI, "getDPI", "").context("Current DPI getter is unavailable")?;
    method(device, DPI, "setDPI", "qq").context("Two-axis DPI setter is unavailable")?;
    if device
        .value(DPI, "availableDPI")
        .and_then(Json::as_array)
        .is_some_and(|values| !values.is_empty())
    {
        bail!("Host linked-axis presets require a two-axis DPI device");
    }
    let max = max_dpi(device).context("Current maximum DPI unavailable")?;
    let pair = device
        .value(DPI, "getDPI")
        .and_then(Json::as_array)
        .filter(|pair| pair.len() == 2)
        .and_then(|pair| {
            Some((
                integer(&pair[0], 100, max).ok()?,
                integer(&pair[1], 100, max).ok()?,
            ))
        });
    let product_id = device
        .product_id()
        .context("Current device VID/PID unavailable")?;
    if !matches!(product_id, 0x0086 | 0x0088) {
        bail!("Host DPI presets are established only for Basilisk Ultimate 0086/0088");
    }
    Ok((pair, max, product_id))
}
fn preferences(config: &Config, device: &DeviceSnapshot, state: Current) -> Result<Preferences> {
    let (pair, max, product_id) = state;
    let preferences = config
        .devices
        .get(&device.serial)
        .cloned()
        .unwrap_or_else(|| samples(product_id, max, pair.is_none_or(|(x, y)| x == y)));
    if preferences.product_id != product_id
        && !(matches!(preferences.product_id, 0x0086 | 0x0088)
            && matches!(product_id, 0x0086 | 0x0088))
    {
        bail!("Saved host DPI presets belong to a different USB product for this daemon serial");
    }
    validate_preferences(&preferences, max)?;
    Ok(preferences)
}
fn active(preferences: &Preferences, x: u64, y: u64) -> Option<&str> {
    preferences
        .stages
        .iter()
        .find(|stage| stage.x == x && stage.y == y)
        .map(|stage| stage.id.as_str())
}
fn cycle(preferences: &Preferences, x: u64, y: u64, previous: bool) -> &Stage {
    let index = preferences
        .stages
        .iter()
        .position(|stage| stage.x == x && stage.y == y);
    let len = preferences.stages.len();
    let next = match index {
        Some(index) if previous => (index + len - 1) % len,
        Some(index) => (index + 1) % len,
        None => 0,
    };
    &preferences.stages[next]
}
pub(super) fn control(device: &DeviceSnapshot) -> Result<Json> {
    control_from_config(device, &load(&config_path()?)?)
}
fn control_from_config(device: &DeviceSnapshot, config: &Config) -> Result<Json> {
    let state = current(device)?;
    let preferences = preferences(config, device, state)?;
    Ok(
        json!({"kind":"dpi","x":state.0.map(|pair| pair.0),"y":state.0.map(|pair| pair.1),"min":100,"max":state.1,"step":50,
        "linked":preferences.linked,"stages":preferences.stages,
        "active":state.0.and_then(|(x,y)| active(&preferences,x,y)),"storage":"host",
        "action":"dpi-presets","args":{"device":device.serial}}),
    )
}
fn save(path: &Path, serial: &str, preferences: Preferences) -> Result<()> {
    validate_preferences(&preferences, u16::MAX as u64)?;
    let parent = path.parent().context("Missing host DPI config directory")?;
    fs::create_dir_all(parent)?;
    // The process-per-request protocol can overlap. Hold an advisory lock across
    // read/modify/atomic rename so one device never drops another device's presets.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.with_extension("lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("Locking host DPI config");
    }
    let mut config = load(path)?;
    config.devices.insert(serial.into(), preferences);
    let temporary = path.with_extension(format!(
        "{}-{}.tmp",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        serde_json::to_writer_pretty(&mut file, &config)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path).context("Replacing host DPI config atomically")?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
#[derive(Debug, PartialEq)]
enum Operation {
    Save(Preferences),
    Apply(u64, u64),
}
fn operation(device: &DeviceSnapshot, saved: &Preferences, value: &Json) -> Result<Operation> {
    let (pair, max, product_id) = current(device)?;
    match string_arg(value, "operation")? {
        "save" => {
            let preferences = Preferences {
                product_id,
                linked: field(value, "linked")?
                    .as_bool()
                    .context("linked must be a boolean")?,
                stages: serde_json::from_value(field(value, "stages")?.clone())
                    .context("stages must contain integer X/Y DPI, IDs and labels")?,
            };
            validate_preferences(&preferences, max)?;
            Ok(Operation::Save(preferences))
        }
        "apply" => {
            let linked = field(value, "linked")?
                .as_bool()
                .context("linked must be a boolean")?;
            let pair = validate_dpi(device, field(value, "x")?, field(value, "y")?)?;
            let (x, y) = (pair[0].as_u64().unwrap(), pair[1].as_u64().unwrap());
            if linked && x != y {
                bail!("Linked DPI requires equal X and Y");
            }
            Ok(Operation::Apply(x, y))
        }
        "activate" => {
            let stage = string_arg(value, "stage")?;
            let stage = saved
                .stages
                .iter()
                .find(|current| current.id == stage)
                .context("Host DPI stage does not exist")?;
            Ok(Operation::Apply(stage.x, stage.y))
        }
        "next" | "previous" => {
            let (x, y) = pair.unwrap_or((0, 0));
            let stage = cycle(saved, x, y, value["operation"] == "previous");
            Ok(Operation::Apply(stage.x, stage.y))
        }
        _ => bail!("Unknown host DPI preset operation"),
    }
}
pub async fn action(args: &Json) -> Result<Group> {
    let serial = string_arg(args, "device")?;
    let value = args.get("value").context("Missing args.value")?;
    let connection = connection().await?;
    if !serials(&connection)
        .await?
        .iter()
        .any(|current| current == serial)
    {
        bail!("OpenRazer device removed or unknown daemon serial");
    }
    let device = device_snapshot(&connection, serial).await?;
    let state = current(&device)?;
    let path_config = config_path()?;
    let saved = preferences(&load(&path_config)?, &device, state)?;
    let mut group = Group::new(
        "dpi-presets-result",
        "DPI preset operation completed",
        "Host presets are not onboard profiles; refreshed actual daemon DPI follows",
    );
    match operation(&device, &saved, value)? {
        Operation::Save(preferences) => {
            save(&path_config, serial, preferences)?;
            group.rows.push(Row::info(
                "saved",
                "Host presets saved",
                "No hardware write. The physical DPI-cycle button does not use these host presets.",
            ));
        }
        Operation::Apply(x, y) => {
            let values = validate_dpi(&device, &json!(x), &json!(y))?;
            let setter = method(&device, DPI, "setDPI", "qq").context("DPI setter unavailable")?;
            call(&connection, &path(serial)?, setter, &values).await?;
            group.rows.push(Row::info("applied", format!("Applied {x} × {y} DPI"),
                "Real setDPI accepted. Active preset is recomputed from refreshed getDPI, not an assumed hardware readback."));
        }
    }
    Ok(group)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn device(serial: &str, pid: u16, x: u64, y: u64) -> DeviceSnapshot {
        DeviceSnapshot {
            serial: serial.into(),
            methods: parse_methods("<node><interface name='razer.device.dpi'><method name='getDPI'><arg type='ai' direction='out'/></method><method name='setDPI'><arg type='q' direction='in'/><arg type='q' direction='in'/></method></interface></node>").unwrap(),
            values: BTreeMap::from([
                ("razer.device.dpi.maxDPI".into(), Ok(json!(20000))),
                ("razer.device.dpi.getDPI".into(), Ok(json!([x,y]))),
                ("razer.device.misc.getVidPid".into(), Ok(json!([0x1532,pid]))),
            ]), diagnostics: Group::new("api", "API", ""),
        }
    }
    fn temporary() -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "hyprdeck-razer-dpi-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
            .join("dpi.json")
    }
    #[test]
    fn stage_domains_linking_and_counts_are_validated() {
        let device = device("serial", 0x0088, 800, 800);
        let saved = samples(0x0088, 20000, true);
        assert_eq!(
            operation(
                &device,
                &saved,
                &json!({"operation":"apply","x":100,"y":20000,"linked":false})
            )
            .unwrap(),
            Operation::Apply(100, 20000)
        );
        for value in [
            json!({"x":99,"y":800,"linked":false}),
            json!({"x":20001,"y":800,"linked":false}),
            json!({"x":800.5,"y":800,"linked":false}),
            json!({"x":800,"y":1600,"linked":true}),
        ] {
            let mut value = value;
            value["operation"] = json!("apply");
            assert!(operation(&device, &saved, &value).is_err());
        }
        for count in [0, 1, 8, 9] {
            let stages: Vec<Stage> = (0..count)
                .map(|index| Stage {
                    id: format!("stage-{index}"),
                    label: "Preset".into(),
                    x: 100,
                    y: 20000,
                })
                .collect();
            let value = json!({"operation":"save","linked":false,"stages":stages});
            assert_eq!(
                operation(&device, &saved, &value).is_ok(),
                (1..=8).contains(&count)
            );
        }
        let mut invalid = saved.clone();
        invalid.stages[1].id = invalid.stages[0].id.clone();
        assert!(validate_preferences(&invalid, 20000).is_err());
        invalid = saved.clone();
        invalid.stages[0].y = 500;
        assert!(validate_preferences(&invalid, 20000).is_err());
        for id in ["", "1-stage", "Upper", "with space", "with.dot"] {
            let mut invalid = saved.clone();
            invalid.stages[0].id = id.into();
            assert!(validate_preferences(&invalid, 20000).is_err());
        }
        let mut invalid = saved;
        invalid.stages[0].id = "a".repeat(65);
        assert!(validate_preferences(&invalid, 20000).is_err());
    }
    #[test]
    fn cycling_wraps_and_uses_actual_current_values() {
        let saved = samples(0x0088, 20000, true);
        assert_eq!(cycle(&saved, 3200, 3200, false).id, "stage-1");
        assert_eq!(cycle(&saved, 400, 400, true).id, "stage-4");
        assert_eq!(cycle(&saved, 800, 800, false).id, "stage-3");
        assert_eq!(cycle(&saved, 900, 900, true).id, "stage-1");
        assert_eq!(active(&saved, 800, 1600), None);
        let single = Preferences {
            stages: vec![saved.stages[0].clone()],
            ..saved
        };
        assert_eq!(cycle(&single, 400, 400, true).id, "stage-1");
        assert_eq!(cycle(&single, 400, 400, false).id, "stage-1");
    }
    #[test]
    fn unknown_current_dpi_still_exposes_host_presets_without_readback_claims() {
        let sleeping = device("serial", 0x0088, 0, 0);
        let config = Config::default();
        let control = control_from_config(&sleeping, &config).unwrap();
        assert_eq!(control["kind"], "dpi");
        assert!(control["x"].is_null() && control["y"].is_null() && control["active"].is_null());
        assert_eq!(control["stages"][0]["x"], 400);
        assert_eq!(control["storage"], "host");
        assert!(config.devices.is_empty());
        let saved = samples(0x0088, 20000, true);
        assert_eq!(
            operation(&sleeping, &saved, &json!({"operation":"next"})).unwrap(),
            Operation::Apply(400, 400)
        );
        assert_eq!(
            operation(
                &sleeping,
                &saved,
                &json!({"operation":"activate","stage":"stage-3"})
            )
            .unwrap(),
            Operation::Apply(1600, 1600)
        );
        assert!(
            operation(
                &sleeping,
                &saved,
                &json!({"operation":"activate","stage":"missing"})
            )
            .is_err()
        );
        let awake = device("serial", 0x0088, 1600, 1600);
        let control = control_from_config(&awake, &config).unwrap();
        assert_eq!(control["x"], 1600);
        assert_eq!(control["active"], "stage-3");
        assert!(
            control_from_config(&device("serial", 0x0088, 950, 950), &config).unwrap()["active"]
                .is_null()
        );
        let stage = json!({"operation":"save","linked":true,"stages":[{"id":"custom","label":"Custom","x":850.0,"y":850.0}]});
        assert!(matches!(
            operation(&awake, &saved, &stage).unwrap(),
            Operation::Save(_)
        ));
        let mut stage = stage;
        stage["stages"][0]["x"] = json!(850.5);
        assert!(operation(&awake, &saved, &stage).is_err());
        assert!(control_from_config(&device("serial", 0xffff, 800, 800), &config).is_err());
        let mut missing_setter = awake;
        missing_setter
            .methods
            .retain(|method| method.name != "setDPI");
        assert!(
            operation(
                &missing_setter,
                &saved,
                &json!({"operation":"apply","x":800,"y":800,"linked":true})
            )
            .is_err()
        );
    }
    #[test]
    fn host_config_preserves_serial_isolation_and_malformed_bytes() {
        let path = temporary();
        let first = samples(0x0088, 20000, true);
        save(&path, "real-serial-a", first.clone()).unwrap();
        let mut second = samples(0x0088, 20000, false);
        second.stages[0].y = 600;
        save(&path, "real-serial-b", second.clone()).unwrap();
        let config = load(&path).unwrap();
        assert_eq!(
            preferences(
                &config,
                &device("real-serial-a", 0x0088, 1600, 1600),
                (Some((1600, 1600)), 20000, 0x0088)
            )
            .unwrap(),
            first
        );
        assert_eq!(config.devices["real-serial-b"], second);
        assert_eq!(
            preferences(
                &config,
                &device("real-serial-a", 0x0086, 800, 800),
                (Some((800, 800)), 20000, 0x0086)
            )
            .unwrap(),
            first
        );
        assert!(
            preferences(
                &config,
                &device("real-serial-a", 0x007e, 800, 800),
                (Some((800, 800)), 20000, 0x007e)
            )
            .is_err()
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for malformed in [
            b"{broken".as_slice(),
            b"{\"version\":2,\"devices\":{}}".as_slice(),
            b"{\"version\":1,\"devices\":{\"real-serial-a\":{\"product_id\":136,\"linked\":true,\"stages\":[]}}}".as_slice(),
        ] {
            fs::write(&path,malformed).unwrap();
            assert!(load(&path).is_err());
            assert!(save(&path,"real-serial-a",first.clone()).is_err());
            assert_eq!(fs::read(&path).unwrap(),malformed);
        }
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
