use crate::{Group, Row};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize)]
struct Entry {
    identity: String,
    unix_seconds: u64,
    group: Group,
    #[serde(default)]
    summary: Option<Row>,
}
fn root() -> Result<PathBuf> {
    let root = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("HOME / XDG_STATE_HOME unavailable for diagnostic cache")?;
    Ok(root.join("hyprdeck-razer"))
}
fn file(kind: &str, id: &str) -> Result<PathBuf> {
    let encoded: String = id.bytes().map(|byte| format!("{byte:02x}")).collect();
    Ok(root()?.join(format!("{kind}-{encoded}.json")))
}
fn identity(id: &str) -> Result<String> {
    let port = id
        .strip_prefix("usb:")
        .or_else(|| {
            id.strip_prefix("input:usb:")
                .and_then(|id| id.split(':').next())
        })
        .context("Unsupported cache device identity")?;
    let device = crate::usb::find(&format!("usb:{port}"))?;
    let metadata = fs::metadata(&device.path)?;
    Ok(format!(
        "{}:{:04x}:{}:{}:{}:{}",
        device.port,
        device.product_id,
        metadata.ino(),
        metadata.ctime(),
        metadata.ctime_nsec(),
        crate::usb::text(device.path.join("serial")).unwrap_or_default()
    ))
}
pub fn store(kind: &str, id: &str, group: &Group, summary: &Row) -> Result<()> {
    // Serialization borrows the result rather than copying its rows.
    let identity = identity(id)?;
    let unix_seconds = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let value = serde_json::json!({"identity":identity,"unix_seconds":unix_seconds,"group":group,"summary":summary});
    let path = file(kind, id)?;
    fs::create_dir_all(path.parent().context("Missing cache parent")?)?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = serde_json::to_writer(&mut output, &value)
        .context("Writing diagnostic cache")
        .and_then(|_| fs::rename(&temporary, &path).context("Replacing diagnostic cache"));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
pub fn load(kind: &str, id: &str) -> Option<(Group, Option<Row>)> {
    let path = file(kind, id).ok()?;
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > 1024 * 1024 {
        return None;
    }
    let mut entry: Entry = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
    if entry.identity != identity(id).ok()? {
        return None;
    }
    entry.group.id = format!("cached-{}", entry.group.id);
    entry.group.title = format!("Last explicit result: {}", entry.group.title);
    entry.group.description = format!(
        "Recorded Unix timestamp {}, not a live measurement. Current USB enumeration identity still matches. {}",
        entry.unix_seconds, entry.group.description
    );
    entry.group.rows.insert(0, Row::info("recorded", "Historical explicit result", format!("Recorded at Unix timestamp {}. Live input is the separate current ioctl snapshot; this result is not automatically remeasured or rechecked.",entry.unix_seconds)));
    if let Some(summary) = &mut entry.summary {
        summary.subtitle.push_str(&format!(
            " Recorded Unix timestamp {}; not automatically rechecked.",
            entry.unix_seconds
        ));
    }
    Some((entry.group, entry.summary))
}
