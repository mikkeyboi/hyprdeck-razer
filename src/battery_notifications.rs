use super::*;
use std::collections::BTreeSet;
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 1024 * 1024;
const MAX_INTERVAL: u64 = 12 * 60 * 60;
const SERVICE: &str = "openrazer-daemon.service";
const KEYS: [&str; 3] = [
    "battery_notifier",
    "battery_notifier_percent",
    "battery_notifier_freq",
];
const LEGACY: [&str; 2] = ["mouse_battery_notifier", "mouse_battery_notifier_freq"];

#[derive(Clone, Copy, Debug, PartialEq)]
struct Preferences {
    enabled: bool,
    percent: u64,
    seconds: u64,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            enabled: false,
            percent: 33,
            seconds: 600,
        }
    }
}
fn config_path() -> Result<PathBuf> {
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("HOME / XDG_CONFIG_HOME unavailable for OpenRazer preferences")?;
    if !root.is_absolute() {
        bail!("OpenRazer configuration directory must be absolute; refusing an ambiguous path");
    }
    Ok(root.join("openrazer/razer.conf"))
}
fn boolean(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "yes" | "true" | "on" => Ok(true),
        "0" | "no" | "false" | "off" => Ok(false),
        _ => bail!("Expected a ConfigParser boolean, got {value:?}"),
    }
}
fn bounded(value: &str, min: u64, max: u64) -> Result<u64> {
    let digits = value.strip_prefix('+').unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("Expected an integer in {min}..={max}, got {value:?}");
    }
    let value: u64 = digits.parse()?;
    if !(min..=max).contains(&value) {
        bail!("Expected an integer in {min}..={max}, got {value}");
    }
    Ok(value)
}
fn validate_value(key: &str, value: &str) -> Result<()> {
    match key {
        "battery_notifier" | "mouse_battery_notifier" => {
            boolean(value)?;
        }
        "battery_notifier_percent" => {
            bounded(value, 1, 100)?;
        }
        "battery_notifier_freq" | "mouse_battery_notifier_freq" => {
            bounded(value, 0, MAX_INTERVAL)?;
        }
        _ => unreachable!(),
    }
    Ok(())
}
struct Setting {
    line: usize,
    key: String,
    startup: bool,
    value_start: usize,
    value_end: usize,
}
struct Parsed {
    preferences: Preferences,
    settings: Vec<Setting>,
    startup_end: Option<usize>,
}
// ConfigParser is case-sensitive for sections and case-insensitive for options.
// Preserve source text; do not round-trip unrelated settings through an INI serializer.
fn parse(text: &str) -> Result<Parsed> {
    if text.contains('\0') {
        bail!("OpenRazer config contains a NUL; existing file was not overwritten");
    }
    let lines: Vec<_> = text.split_inclusive('\n').collect();
    let mut sections = BTreeSet::new();
    let mut options = BTreeSet::new();
    let mut section: Option<&str> = None;
    let mut previous: Option<(usize, bool)> = None;
    let mut settings = Vec::new();
    let mut values = BTreeMap::new();
    let mut defaults = BTreeMap::new();
    let mut startup_end = None;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(['#', ';']) {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if let Some((previous_indent, targeted)) = previous
            && indent > previous_indent
        {
            if targeted {
                bail!(
                    "Multiline battery setting on line {}; existing file was not overwritten",
                    index + 1
                );
            }
            continue;
        }
        if trimmed.starts_with('[') {
            let end = trimmed
                .rfind(']')
                .context("Malformed OpenRazer section header; existing file was not overwritten")?;
            let name = &trimmed[1..end];
            if name.is_empty() || !sections.insert(name) {
                bail!(
                    "Empty or duplicate OpenRazer section on line {}; existing file was not overwritten",
                    index + 1
                );
            }
            if section == Some("Startup") {
                startup_end = Some(index);
            }
            if name == "Startup" {
                startup_end = Some(lines.len());
            }
            section = Some(name);
            options.clear();
            previous = None;
            continue;
        }
        let section = section
            .context("OpenRazer option outside a section; existing file was not overwritten")?;
        let delimiter = line
            .find(['=', ':'])
            .context("Malformed OpenRazer option; existing file was not overwritten")?;
        let key = line[..delimiter].trim().to_ascii_lowercase();
        if key.is_empty() || !options.insert(key.clone()) {
            bail!(
                "Empty or duplicate OpenRazer option on line {}; existing file was not overwritten",
                index + 1
            );
        }
        let targeted = matches!(section, "Startup" | "DEFAULT")
            && (KEYS.contains(&key.as_str()) || LEGACY.contains(&key.as_str()));
        previous = Some((indent, targeted));
        if targeted {
            let value = line[delimiter + 1..].trim();
            validate_value(&key, value).with_context(|| {
                format!("Invalid [{section}] {key}; existing file was not overwritten")
            })?;
            let value_start = delimiter + 1 + line[delimiter + 1..].len()
                - line[delimiter + 1..].trim_start().len();
            let value_end = line.trim_end().len();
            if section == "Startup" {
                values.insert(key.clone(), value);
            } else {
                defaults.insert(key.clone(), value);
            }
            settings.push(Setting {
                line: index,
                key,
                startup: section == "Startup",
                value_start,
                value_end,
            });
        }
    }
    let value = |key: &str| values.get(key).or_else(|| defaults.get(key)).copied();
    let enabled = value(LEGACY[0]).or_else(|| value(KEYS[0]));
    let seconds = value(LEGACY[1]).or_else(|| value(KEYS[2]));
    Ok(Parsed {
        preferences: Preferences {
            enabled: enabled.map(boolean).transpose()?.unwrap_or(false),
            percent: value(KEYS[1])
                .map(|value| bounded(value, 1, 100))
                .transpose()?
                .unwrap_or(33),
            seconds: seconds
                .map(|value| bounded(value, 0, MAX_INTERVAL))
                .transpose()?
                .unwrap_or(600),
        },
        settings,
        startup_end,
    })
}
fn updated(text: &str, parsed: &Parsed, preferences: Preferences) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let values = [
        if preferences.enabled {
            "True".into()
        } else {
            "False".into()
        },
        preferences.percent.to_string(),
        preferences.seconds.to_string(),
    ];
    let lines: Vec<_> = text.split_inclusive('\n').collect();
    let mut output = String::with_capacity(text.len() + 160);
    let mut settings = parsed.settings.iter().peekable();
    let missing: Vec<_> = KEYS
        .iter()
        .zip(&values)
        .filter(|(key, _)| {
            !parsed
                .settings
                .iter()
                .any(|setting| setting.startup && setting.key == **key)
        })
        .collect();
    for index in 0..=lines.len() {
        if index == parsed.startup_end.unwrap_or(lines.len()) && !missing.is_empty() {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push_str(newline);
            }
            if parsed.startup_end.is_none() {
                output.push_str("[Startup]");
                output.push_str(newline);
            }
            for (key, value) in &missing {
                output.push_str(key);
                output.push_str(" = ");
                output.push_str(value);
                output.push_str(newline);
            }
        }
        let Some(line) = lines.get(index) else { break };
        if settings.peek().is_some_and(|setting| setting.line == index) {
            let setting = settings.next().unwrap();
            // Old names override modern keys in the daemon: remove them on cutover,
            // including DEFAULT inheritance, rather than saving ignored preferences.
            if LEGACY.contains(&setting.key.as_str()) {
                continue;
            }
            if setting.startup {
                let key = KEYS.iter().position(|key| *key == setting.key).unwrap();
                output.push_str(&line[..setting.value_start]);
                output.push_str(&values[key]);
                output.push_str(&line[setting.value_end..]);
                continue;
            }
        }
        output.push_str(line);
    }
    output
}

fn c_name(name: &OsStr) -> Result<CString> {
    CString::new(name.as_bytes()).context("NUL in OpenRazer config path")
}
fn open_at(parent: &File, name: &OsStr, flags: i32, mode: u32) -> std::io::Result<File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
// Walk directories by descriptor, never following a swapped/symlinked ancestor.
fn directory(path: &Path, create: bool) -> Result<Option<File>> {
    let mut current = File::open("/")?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            if component == Component::RootDir {
                continue;
            }
            bail!("OpenRazer config path must be absolute without parent traversal");
        };
        match open_at(&current, name, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
            Ok(next) => current = next,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                let name_c = c_name(name)?;
                if unsafe { libc::mkdirat(current.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(error.into());
                    }
                }
                current = open_at(&current, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).context("Opening OpenRazer config directory without symlinks");
            }
        }
    }
    Ok(Some(current))
}
fn regular(file: &File) -> Result<std::fs::Metadata> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        bail!(
            "OpenRazer config/lock must be a user-owned regular file, not a symlink or hard link"
        );
    }
    Ok(metadata)
}
fn load_at(parent: &File) -> Result<(String, u32)> {
    let file = match open_at(
        parent,
        OsStr::new("razer.conf"),
        libc::O_RDONLY | libc::O_NONBLOCK,
        0,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((String::new(), 0o600));
        }
        Err(error) => return Err(error).context("Opening OpenRazer config without symlinks"),
    };
    let metadata = regular(&file)?;
    if metadata.len() > MAX_BYTES {
        bail!("OpenRazer config exceeds 1 MiB; existing file was not overwritten");
    }
    let mut text = String::new();
    file.take(MAX_BYTES + 1)
        .read_to_string(&mut text)
        .context("Reading UTF-8 OpenRazer config")?;
    if text.len() as u64 > MAX_BYTES {
        bail!("OpenRazer config exceeds 1 MiB; existing file was not overwritten");
    }
    Ok((text, metadata.permissions().mode() & 0o777))
}
fn load(path: &Path) -> Result<(String, Parsed)> {
    let text = match directory(
        path.parent().context("Missing OpenRazer config parent")?,
        false,
    )? {
        Some(parent) => load_at(&parent)?.0,
        None => String::new(),
    };
    let parsed = parse(&text)?;
    Ok((text, parsed))
}
fn save(path: &Path, preferences: Preferences) -> Result<()> {
    let parent = directory(
        path.parent().context("Missing OpenRazer config parent")?,
        true,
    )?
    .unwrap();
    let lock = open_at(
        &parent,
        OsStr::new("razer.conf.lock"),
        libc::O_RDWR | libc::O_CREAT | libc::O_NONBLOCK,
        0o600,
    )?;
    regular(&lock)?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("Locking OpenRazer preferences");
    }
    let (text, mode) = load_at(&parent)?;
    let parsed = parse(&text)?;
    let output = updated(&text, &parsed, preferences);
    let temporary = format!(
        ".razer.conf-{}-{}.tmp",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let name = c_name(OsStr::new(&temporary))?;
    let target = c_name(OsStr::new("razer.conf"))?;
    let mut file = open_at(
        &parent,
        OsStr::new(&temporary),
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    let result = (|| -> Result<()> {
        file.write_all(output.as_bytes())?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        if unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                name.as_ptr(),
                parent.as_raw_fd(),
                target.as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error())
                .context("Replacing OpenRazer config atomically");
        }
        parent.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0);
        }
    }
    result
}

pub(super) fn group(device: &DeviceSnapshot) -> Group {
    let mut group = Group::new(
        format!("settings:{}:battery-notifications", device.serial),
        "OpenRazer battery notifications · all devices",
        "Global desktop popups, not the hardware indicator. Notifications may repeat while charging. Per-device preferences and separate charging/full alerts are not supported.",
    );
    let result = (|| -> Result<()> {
        let path = config_path()?;
        group.rows.push(Row::info(
            "config-path",
            "OpenRazer configuration file",
            path.display().to_string(),
        ));
        let (text, parsed) = load(&path)?;
        let preferences = parsed.preferences;
        group.rows.push(Row::info("saved", "Saved preferences · not running-state readback", if text.is_empty() {
            "File is missing/empty: daemon fallbacks are disabled, 33%, 600 seconds. Saving and refreshing never restart the daemon."
        } else {
            "Values below are read from disk. The daemon reads them at startup; its running notification preferences cannot be queried. Saving and refreshing never restart it."
        }));
        group.rows.push(Row::info("preferences", "Battery popup preferences · all OpenRazer battery devices", "Interval is both check cadence and repeat delay, not a one-shot cooldown. 0 disables notifications even when enabled. Maximum 43200 seconds (12 hours), below the daemon's day-wrapping timer limit.").controlled(json!({
            "kind":"form", "action":"battery-notifications", "args":{"operation":"save"}, "label":"Save preferences",
            "fields":[
                {"id":"enabled","label":"Enable battery popups · all devices","kind":"switch","value":preferences.enabled,"options":[]},
                {"id":"percent","label":"Notify at or below (%)","kind":"number","value":preferences.percent,"min":1,"max":100,"step":1,"options":[]},
                {"id":"seconds","label":"Check / repeat interval (seconds; 0 = disabled)","kind":"number","value":preferences.seconds,"min":0,"max":MAX_INTERVAL,"step":1,"options":[]}
            ]
        })));
        if parsed
            .settings
            .iter()
            .any(|setting| LEGACY.contains(&setting.key.as_str()))
        {
            group.rows.push(Row::info("legacy", "Legacy notifier settings detected", "Displayed values include legacy overrides. Saving migrates the old notifier keys to the current global keys, preserving unrelated configuration."));
        }
        group.rows.push(Row::info("apply", "Apply saved preferences to all Razer devices", "Explicit restart only: all Razer devices briefly lose daemon controls. Requires a running default-config OpenRazer user service. This does not independently read back live notifier preferences.").controlled(json!({
            "kind":"button","label":"Restart OpenRazer to apply","action":"battery-notifications","args":{"operation":"restart"},"destructive":true
        })));
        Ok(())
    })();
    if let Err(error) = result {
        group.rows.push(Row::info(
            "error",
            "Battery notification preferences unavailable",
            format!("{error:#}. Existing file is preserved; no daemon restart performed."),
        ));
    }
    group
}

async fn systemctl(arguments: &[&str]) -> Result<String> {
    let mut command = tokio::process::Command::new("systemctl");
    command
        .args(["--user", "--no-pager"])
        .args(arguments)
        .kill_on_drop(true);
    // The upstream battery worker can delay shutdown; the observed daemon took
    // six seconds. Keep restart within the outer 13-second request deadline.
    let deadline = if arguments.first() == Some(&"restart") {
        10
    } else {
        4
    };
    let output = tokio::time::timeout(Duration::from_secs(deadline), command.output()).await
        .context("OpenRazer service operation timed out; inspect systemctl --user status openrazer-daemon.service before retrying")??;
    if !output.status.success() {
        bail!(
            "systemctl {} failed: {}{}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim(),
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    String::from_utf8(output.stdout).context("Non-UTF-8 OpenRazer service response")
}
fn service_config_path(manager: &[String], unit: &[String], unset: &[String]) -> Result<PathBuf> {
    let mut environment = BTreeMap::new();
    for assignment in manager.iter().chain(unit) {
        let (name, value) = assignment
            .split_once('=')
            .context("Malformed systemd service environment")?;
        environment.insert(name, value);
    }
    for entry in unset {
        if let Some((name, value)) = entry.split_once('=') {
            if environment.get(name) == Some(&value) {
                environment.remove(name);
            }
        } else {
            environment.remove(entry.as_str());
        }
    }
    let root = environment
        .get("XDG_CONFIG_HOME")
        .map(|root| PathBuf::from(*root))
        .or_else(|| {
            environment
                .get("HOME")
                .map(|home| PathBuf::from(*home).join(".config"))
        })
        .context("Cannot verify OpenRazer service's default configuration directory")?;
    if !root.is_absolute() {
        bail!("OpenRazer service configuration directory must be absolute");
    }
    Ok(root.join("openrazer/razer.conf"))
}
async fn configured_service_path() -> Result<PathBuf> {
    let connection = zbus::Connection::session().await?;
    let manager = zbus::Proxy::new(
        &connection,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .await?;
    let unit_path: zbus::zvariant::OwnedObjectPath = manager.call("GetUnit", &(SERVICE,)).await?;
    let service = zbus::Proxy::new(
        &connection,
        "org.freedesktop.systemd1",
        unit_path,
        "org.freedesktop.systemd1.Service",
    )
    .await?;
    let files: Vec<(String, bool)> = service.get_property("EnvironmentFiles").await?;
    let pam: String = service.get_property("PAMName").await?;
    let root_directory: String = service.get_property("RootDirectory").await?;
    let root_image: String = service.get_property("RootImage").await?;
    if !files.is_empty() || !pam.is_empty() || !root_directory.is_empty() || !root_image.is_empty()
    {
        bail!(
            "OpenRazer service uses environment files, PAM or a custom root; cannot verify its default configuration path"
        );
    }
    let inherited: Vec<String> = manager.get_property("Environment").await?;
    let overrides: Vec<String> = service.get_property("Environment").await?;
    let unset: Vec<String> = service.get_property("UnsetEnvironment").await?;
    service_config_path(&inherited, &overrides, &unset)
}
async fn running_default_service(path: &Path) -> Result<()> {
    let output = systemctl(&[
        "show",
        SERVICE,
        "--property=LoadState,ActiveState,MainPID,ExecStart",
    ])
    .await?;
    let properties: BTreeMap<_, _> = output
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    if properties.get("LoadState") != Some(&"loaded")
        || properties.get("ActiveState") != Some(&"active")
    {
        bail!(
            "OpenRazer user service must exist and already be running; refusing to start an unrelated/inactive daemon"
        );
    }
    let exec = properties
        .get("ExecStart")
        .context("OpenRazer unit ExecStart unavailable")?;
    let argv = exec
        .split_once("argv[]=")
        .and_then(|(_, tail)| tail.split_once(" ; "))
        .map(|(argv, _)| argv)
        .context("Cannot verify OpenRazer service command; refusing restart")?;
    let mut arguments = argv.split_whitespace();
    if arguments.next() != Some("/usr/bin/openrazer-daemon")
        || arguments.any(|argument| !matches!(argument, "-F" | "--foreground" | "-v" | "--verbose"))
    {
        bail!(
            "OpenRazer service uses a custom command/configuration; refusing to restart it against the displayed default config"
        );
    }
    let pid: u32 = properties
        .get("MainPID")
        .context("OpenRazer MainPID unavailable")?
        .parse()?;
    if pid == 0 {
        bail!("OpenRazer service has no running main process");
    }
    // Upstream setproctitle replaces cmdline and the original /proc environment.
    // Verify the command above and the next startup's authoritative systemd
    // environment instead; this is not a readback of the live notifier config.
    let configured_path = tokio::time::timeout(Duration::from_secs(4), configured_service_path())
        .await
        .context("Verifying OpenRazer service configuration timed out; no restart performed")??;
    if configured_path != path {
        bail!(
            "OpenRazer service uses another default config directory; displayed preferences were not applied"
        );
    }
    Ok(())
}
pub async fn action(args: &Json) -> Result<Group> {
    let path = config_path()?;
    let mut result = Group::new(
        "battery-notifications-result",
        "OpenRazer battery notification preferences",
        "Global settings for all OpenRazer battery devices",
    );
    match string_arg(args, "operation")? {
        "save" => {
            let value = args
                .get("value")
                .context("Missing args.value form fields")?;
            let object = value.as_object().context("args.value must be an object")?;
            if object.len() != 3
                || !["enabled", "percent", "seconds"]
                    .iter()
                    .all(|key| object.contains_key(*key))
            {
                bail!("Battery preferences require exactly enabled, percent and seconds");
            }
            let preferences = Preferences {
                enabled: object["enabled"]
                    .as_bool()
                    .context("enabled must be a boolean")?,
                percent: integer(&object["percent"], 1, 100)?,
                seconds: integer(&object["seconds"], 0, MAX_INTERVAL)?,
            };
            let saved_path = path.clone();
            tokio::task::spawn_blocking(move || save(&saved_path, preferences)).await??;
            result.rows.push(Row::info("saved", "Preferences saved · running daemon unchanged", format!("{} · Use the separate Restart OpenRazer action to apply to every Razer device. No restart or hardware write occurred.", path.display())));
        }
        "restart" => {
            // Refuse malformed disk values before disturbing the user's devices.
            let (_, parsed) = load(&path)?;
            if parsed.startup_end.is_none() {
                bail!(
                    "Save preferences first to create OpenRazer's required [Startup] section; no restart performed"
                );
            }
            running_default_service(&path).await?;
            systemctl(&["restart", SERVICE]).await?;
            running_default_service(&path).await?;
            result.rows.push(Row::info("restarted", "OpenRazer user service restarted · all Razer devices", format!("Service is active with the default configuration path {}. The daemon reloads saved preferences at startup; notifier internals have no readback API.", path.display())));
        }
        operation => {
            bail!("Unknown battery-notifications operation {operation:?}; expected save or restart")
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn preferences() -> Preferences {
        Preferences {
            enabled: true,
            percent: 20,
            seconds: 1200,
        }
    }
    #[test]
    fn service_environment_uses_unit_overrides_and_final_unsets() {
        let manager = vec!["HOME=/home/user".into(), "XDG_CONFIG_HOME=/manager".into()];
        let unit = vec!["XDG_CONFIG_HOME=/unit path".into()];
        assert_eq!(
            service_config_path(&manager, &unit, &[]).unwrap(),
            PathBuf::from("/unit path/openrazer/razer.conf")
        );
        assert_eq!(
            service_config_path(&manager, &unit, &["XDG_CONFIG_HOME=/other".into()]).unwrap(),
            PathBuf::from("/unit path/openrazer/razer.conf")
        );
        for unset in ["XDG_CONFIG_HOME", "XDG_CONFIG_HOME=/unit path"] {
            assert_eq!(
                service_config_path(&manager, &unit, &[unset.into()]).unwrap(),
                PathBuf::from("/home/user/.config/openrazer/razer.conf")
            );
        }
        assert!(service_config_path(&[], &[], &[]).is_err());
        assert!(service_config_path(&manager, &["XDG_CONFIG_HOME=relative".into()], &[]).is_err());
    }
    #[test]
    fn daemon_defaults_and_legacy_overrides() {
        assert_eq!(parse("").unwrap().preferences, Preferences::default());
        let parsed = parse("[DEFAULT]\nmouse_battery_notifier = yes\n[Startup]\nbattery_notifier = False\nmouse_battery_notifier_freq = 0\n").unwrap();
        assert_eq!(
            parsed.preferences,
            Preferences {
                enabled: true,
                percent: 33,
                seconds: 0
            }
        );
    }
    #[test]
    fn preserves_unrelated_text_and_migrates_legacy() {
        let text = "# retained\r\n[Startup]\r\n  battery_notifier : False  \r\nmouse_battery_notifier = no\r\nrestore_persistence = True\r\n; retained too\r\n[Other]\r\ncustom = value\r\n";
        let output = updated(text, &parse(text).unwrap(), preferences());
        assert_eq!(
            output,
            "# retained\r\n[Startup]\r\n  battery_notifier : True  \r\nrestore_persistence = True\r\n; retained too\r\nbattery_notifier_percent = 20\r\nbattery_notifier_freq = 1200\r\n[Other]\r\ncustom = value\r\n"
        );
        assert_eq!(parse(&output).unwrap().preferences, preferences());
    }
    #[test]
    fn rejects_invalid_or_ambiguous_target_values() {
        for body in [
            "battery_notifier = maybe",
            "battery_notifier_percent = 0",
            "battery_notifier_percent = 101",
            "battery_notifier_freq = 43201",
            "battery_notifier_freq = -1",
            "battery_notifier_freq = 600 # comment",
            "battery_notifier_freq = 600\nBATTERY_NOTIFIER_FREQ=10",
            "battery_notifier=True\n  unexpected continuation",
            "mouse_battery_notifier=nope",
            "battery_notifier",
            "battery_notifier=True\n[Startup]\nbattery_notifier=False",
        ] {
            assert!(parse(&format!("[Startup]\n{body}\n")).is_err(), "{body}");
        }
        assert_eq!(
            parse("[Startup]\nbattery_notifier_freq=0\n")
                .unwrap()
                .preferences
                .seconds,
            0
        );
        assert_eq!(
            parse("[Startup]\nbattery_notifier_freq=43200\n")
                .unwrap()
                .preferences
                .seconds,
            43200
        );
    }
    #[test]
    fn adds_missing_section_without_losing_last_line() {
        let text = "# note\n[Other]\nvalue = preserved";
        let output = updated(text, &parse(text).unwrap(), preferences());
        assert!(output.starts_with("# note\n[Other]\nvalue = preserved\n[Startup]\n"));
        assert_eq!(parse(&output).unwrap().preferences, preferences());
    }
    #[test]
    fn legacy_default_is_removed_without_changing_other_defaults() {
        let text = "[DEFAULT]\nmouse_battery_notifier = True\nbattery_notifier_percent = 15\nother = stay\n[Startup]\nrestore_persistence = True\n";
        let output = updated(text, &parse(text).unwrap(), preferences());
        assert!(!output.contains("mouse_battery_notifier"));
        assert!(output.contains("battery_notifier_percent = 15\nother = stay"));
        assert_eq!(parse(&output).unwrap().preferences, preferences());
    }
    fn temporary() -> PathBuf {
        std::env::temp_dir().join(format!(
            "hyprdeck-battery-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    #[test]
    fn save_is_atomic_preserves_permissions_and_refuses_malformed_input() {
        let root = temporary();
        let path = root.join("openrazer/razer.conf");
        save(&path, preferences()).unwrap();
        assert_eq!(load(&path).unwrap().1.preferences, preferences());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let malformed = "# do not overwrite\n[Startup]\nbattery_notifier = broken\n";
        std::fs::write(&path, malformed).unwrap();
        assert!(save(&path, preferences()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), malformed);
        let valid = "# keep\n[Startup]\nbattery_notifier = False\nrestore_persistence = True\n[Other]\nvalue = unchanged";
        std::fs::write(&path, valid).unwrap();
        save(&path, preferences()).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.ends_with("[Other]\nvalue = unchanged"));
        assert!(saved.contains("restore_persistence = True"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn refuses_config_lock_and_ancestor_symlinks() {
        let root = temporary();
        std::fs::create_dir_all(root.join("real")).unwrap();
        let victim = root.join("victim");
        std::fs::write(&victim, "untouched").unwrap();
        let path = root.join("real/razer.conf");
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        assert!(load(&path).is_err());
        assert!(save(&path, preferences()).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(root.join("real/razer.conf.lock")).unwrap();
        std::os::unix::fs::symlink(&victim, root.join("real/razer.conf.lock")).unwrap();
        assert!(save(&path, preferences()).is_err());
        std::os::unix::fs::symlink(root.join("real"), root.join("linked")).unwrap();
        assert!(save(&root.join("linked/razer.conf"), preferences()).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn concurrent_saves_serialize_without_truncating_other_settings() {
        let root = temporary();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("razer.conf");
        std::fs::write(
            &path,
            "[Startup]\nrestore_persistence = True\n[Other]\nvalue = preserved\n",
        )
        .unwrap();
        let workers: Vec<_> = (1..=8)
            .map(|percent| {
                let path = path.clone();
                std::thread::spawn(move || {
                    save(
                        &path,
                        Preferences {
                            percent,
                            ..preferences()
                        },
                    )
                    .unwrap()
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let (text, parsed) = load(&path).unwrap();
        assert!((1..=8).contains(&parsed.preferences.percent));
        assert!(text.contains("restore_persistence = True\n"));
        assert!(text.ends_with("[Other]\nvalue = preserved\n"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
