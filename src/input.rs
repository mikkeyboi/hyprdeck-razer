use crate::{Group, Row, usb};
use anyhow::{Context, Result, bail};
use evdev::raw_stream::RawDevice;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

pub struct Node {
    pub id: String,
    path: PathBuf,
    name: String,
}
pub fn nodes(device: &usb::Device) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();
    for entry in fs::read_dir("/sys/class/input").context("Reading Linux input sysfs")? {
        let entry = entry?;
        let event = entry.file_name().to_string_lossy().into_owned();
        if !event.starts_with("event") {
            continue;
        }
        let Ok(path) = fs::canonicalize(entry.path().join("device")) else {
            continue;
        };
        if !path.starts_with(&device.path) {
            continue;
        }
        nodes.push(Node {
            id: format!("input:{}:{event}", device.id),
            path: PathBuf::from("/dev/input").join(&event),
            name: usb::text(path.join("name")).unwrap_or(event),
        });
    }
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(nodes)
}
fn open(node: &Node) -> Result<RawDevice> {
    // O_RDONLY is intentional: diagnostics cannot inject events, upload force feedback,
    // or claim exclusive ownership. from_fd avoids evdev's read/write open helper.
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(&node.path)
        .with_context(|| format!("Opening {} read-only: active-seat uaccess ACL is normally sufficient; if denied, inspect getfacl and your login/seat permissions. Do not grant broad input-group access or run the GUI as root", node.path.display()))?;
    RawDevice::from_fd(file.into())
        .context("Reading evdev capabilities; input may have been removed")
}
fn initialized_rows(device: &RawDevice) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    if let Some(keys) = device.supported_keys() {
        let state = device
            .get_key_state()
            .context("EVIOCGKEY initial button state (device removed?)")?;
        if device.supported_absolute_axes().is_some() {
            for key in keys.iter() {
                rows.push(Row::info(
                    format!("key-{}", key.0),
                    format!("{key:?} (0x{:03x})", key.0),
                    if state.contains(key) {
                        "Pressed"
                    } else {
                        "Released"
                    },
                ));
            }
        } else {
            rows.push(Row::info("key-state", "Current buttons / keyboard keys", format!("Pressed: {:?}. Supported: {:?}. All other supported keys are released (fresh EVIOCGKEY).", state.iter().collect::<Vec<_>>(), keys.iter().collect::<Vec<_>>())));
        }
    }
    if device.supported_absolute_axes().is_some() {
        for (axis, info) in device
            .get_absinfo()
            .context("EVIOCGABS initial axes (device removed?)")?
        {
            rows.push(Row::info(
                format!("abs-{}", axis.0),
                format!("{axis:?} (0x{:02x})", axis.0),
                format!(
                    "{} (range {}..{}, dead zone {}, fuzz {}, resolution {})",
                    info.value(),
                    info.minimum(),
                    info.maximum(),
                    info.flat(),
                    info.fuzz(),
                    info.resolution()
                ),
            ));
        }
    }
    if let Some(axes) = device.supported_relative_axes() {
        rows.push(Row::info("relative-axes", "Relative input", format!("{:?}; relative deltas have no persistent position. Use measurement while moving the device.", axes.iter().collect::<Vec<_>>())));
    }
    Ok(rows)
}
pub fn snapshot(node: &Node) -> Group {
    let mut group = Group::new(
        &node.id,
        &node.name,
        format!(
            "{} — nonexclusive read-only evdev snapshot",
            node.path.display()
        ),
    );
    match open(node).and_then(|device| initialized_rows(&device)) {
        Ok(rows) => {
            group.rows.push(Row::info("status", "Live input initialized", "Buttons and absolute axes queried directly with EVIOCGKEY/EVIOCGABS, even when idle. Input remains available to Steam/games. Snapshots refresh when Hyprdeck is mapped; no event history or rate is fabricated."));
            group.rows.extend(rows);
            group.rows.push(Row::info("measurement", "Changed input-frame cadence", "Move sticks/triggers or the mouse continuously for four seconds. This measures evdev SYN_REPORT frames containing changed standard input, not every USB packet or internal sensor poll.").controlled(json!({"kind":"button","label":"Measure input (4 s)","action":"measure","args":{"device":node.id,"seconds":4},"destructive":false})));
        }
        Err(error) => group.rows.push(Row::info(
            "status",
            "Input unavailable / permission or removal",
            format!("{error:#}"),
        )),
    }
    group
}
fn find(id: &str) -> Result<Node> {
    for device in usb::devices()? {
        if let Some(node) = nodes(&device)?.into_iter().find(|node| node.id == id) {
            return Ok(node);
        }
    }
    bail!(
        "Input {id:?} is disconnected or its event node changed; refresh state and use the current ID"
    )
}

#[derive(Default)]
struct Cadence {
    changed: bool,
    lost: bool,
    frames: u64,
    drops: u64,
    intervals: Vec<f64>,
    previous: Option<SystemTime>,
    invalid_timestamps: u64,
}
impl Cadence {
    fn feed(&mut self, event: &evdev::InputEvent) {
        match (event.event_type().0, event.code()) {
            (0, 3) => {
                self.drops += 1;
                self.lost = true;
                self.changed = false;
                self.previous = None;
            }
            (0, 0) => {
                if self.lost {
                    self.lost = false;
                    self.changed = false;
                    return;
                }
                if self.changed {
                    self.frames += 1;
                    let time = event.timestamp();
                    if let Some(previous) = self.previous {
                        match time.duration_since(previous) {
                            Ok(interval) if !interval.is_zero() => {
                                self.intervals.push(interval.as_secs_f64() * 1000.0)
                            }
                            _ => {
                                self.invalid_timestamps += 1;
                            }
                        }
                    }
                    self.previous = Some(time);
                }
                self.changed = false;
            }
            (1..=3, _) if !self.lost => self.changed = true,
            _ => {}
        }
    }
    fn description(&mut self, elapsed: Duration) -> String {
        if self.frames == 0 {
            return format!(
                "Idle / no changed standard input: 0 frames in {:.3} s. No polling-rate estimate is possible. Move controls throughout measurement. SYN_DROPPED: {}.",
                elapsed.as_secs_f64(),
                self.drops
            );
        }
        if self.intervals.is_empty() {
            return format!(
                "Insufficient timing sample: {} changed frames in {:.3} s, no valid consecutive frame interval. SYN_DROPPED: {}; invalid/nonmonotonic timestamps: {}.",
                self.frames,
                elapsed.as_secs_f64(),
                self.drops,
                self.invalid_timestamps
            );
        }
        self.intervals.sort_by(f64::total_cmp);
        let average = self.intervals.iter().sum::<f64>() / self.intervals.len() as f64;
        let median = self.intervals[self.intervals.len() / 2];
        let p95 = self.intervals[((self.intervals.len() - 1) as f64 * 0.95).round() as usize];
        format!(
            "{} changed frames in {:.3} s ({:.1} frames/s over observation window); {} valid intervals: mean {:.3} ms ({:.1} Hz reciprocal), median {:.3} ms, p95 {:.3} ms, min {:.3} ms, max {:.3} ms. SYN_DROPPED: {}; invalid/nonmonotonic timestamps: {}. Changed-event cadence only: idle gaps, filtering, radio, kernel batching and application scheduling prevent inferring physical polling.{}",
            self.frames,
            elapsed.as_secs_f64(),
            self.frames as f64 / elapsed.as_secs_f64(),
            self.intervals.len(),
            average,
            1000.0 / average,
            median,
            p95,
            self.intervals[0],
            self.intervals[self.intervals.len() - 1],
            self.drops,
            self.invalid_timestamps,
            if self.drops > 0 {
                " Lost frames make this sample incomplete; repeat under lower load."
            } else {
                ""
            }
        )
    }
}
pub fn measure(id: &str, duration: Duration) -> Result<Group> {
    let node = find(id)?;
    let mut device = open(&node)?;
    let mut group = Group::new(
        format!("measurement:{id}"),
        format!("Measurement: {}", node.name),
        "Nonexclusive changed-input measurement; not USB/sensor physical polling",
    );
    group.rows.extend(initialized_rows(&device)?);
    // Use kernel monotonic timestamps if supported. Failure leaves default realtime
    // timestamps; nonmonotonic intervals are rejected rather than fabricated.
    let clock = libc::CLOCK_MONOTONIC;
    let monotonic =
        unsafe { libc::ioctl(device.as_raw_fd(), 0x400445a0 as libc::c_ulong, &clock) } >= 0;
    let start = Instant::now();
    let mut cadence = Cadence::default();
    while start.elapsed() < duration {
        let remaining = duration.saturating_sub(start.elapsed());
        let timeout = remaining.as_millis().clamp(1, 100) as i32;
        let mut descriptor = libc::pollfd {
            fd: device.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let status = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if status < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("Polling input");
        }
        if descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            bail!(
                "Device removed or input stream unavailable during measurement; no completed rate sample"
            );
        }
        if status == 0 {
            continue;
        }
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    cadence.feed(&event);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                return Err(error).context("Reading changed input; device may have been removed");
            }
        }
    }
    group.rows.insert(
        0,
        Row::info(
            "cadence",
            "Observed changed input-frame cadence",
            cadence.description(start.elapsed()),
        ),
    );
    group.rows.insert(
        1,
        Row::info(
            "clock",
            "Event timestamp clock",
            if monotonic {
                "Kernel monotonic clock"
            } else {
                "Kernel default realtime clock; invalid/nonmonotonic intervals excluded"
            },
        ),
    );
    group.rows.push(Row::info(
        "final-state",
        "Final input state",
        "Fresh ioctl snapshot after measurement (recovers state after any SYN_DROPPED).",
    ));
    for mut row in initialized_rows(&device)? {
        row.id = format!("final-{}", row.id);
        group.rows.push(row);
    }
    crate::cache::store("measurement", id, &group)?;
    Ok(group)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_and_loss_are_not_fake_polling() {
        let mut cadence = Cadence::default();
        cadence.feed(&evdev::InputEvent::new(0, 0, 0));
        assert_eq!(cadence.frames, 0);
        assert!(cadence.intervals.is_empty());
        cadence.feed(&evdev::InputEvent::new(3, 0, 42));
        cadence.feed(&evdev::InputEvent::new(0, 3, 0));
        cadence.feed(&evdev::InputEvent::new(0, 0, 0));
        assert_eq!(cadence.frames, 0);
        assert_eq!(cadence.drops, 1);
        cadence.feed(&evdev::InputEvent::new(3, 0, 43));
        cadence.feed(&evdev::InputEvent::new(0, 0, 0));
        assert_eq!(cadence.frames, 1);
        assert!(cadence.intervals.is_empty());
    }
}
