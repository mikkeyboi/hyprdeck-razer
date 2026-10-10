use crate::{ControllerInput, Group, Row, usb};
use anyhow::{Context, Result, bail};
use evdev::raw_stream::RawDevice;
use evdev::{AbsInfo, AbsoluteAxisCode, AttributeSet, AttributeSetRef, KeyCode};
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

pub struct Node {
    pub id: String,
    pub(crate) path: PathBuf,
    pub(crate) name: String,
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
struct KernelState {
    keys: Option<AttributeSet<KeyCode>>,
    // Linux ABS_MAX is 0x3f; keep the small ioctl result on the stack.
    axes: [Option<AbsInfo>; 64],
}

fn read_state(device: &RawDevice) -> Result<KernelState> {
    let keys = device
        .supported_keys()
        .map(|_| device.get_key_state())
        .transpose()
        .context("EVIOCGKEY initial button state (device removed?)")?;
    let mut axes = [None; 64];
    if device.supported_absolute_axes().is_some() {
        for (axis, info) in device
            .get_absinfo()
            .context("EVIOCGABS initial axes (device removed?)")?
        {
            axes[usize::from(axis.0)] = Some(info);
        }
    }
    Ok(KernelState { keys, axes })
}

fn normalized(info: &AbsInfo) -> Option<f64> {
    let minimum = f64::from(info.minimum());
    let maximum = f64::from(info.maximum());
    (maximum > minimum)
        .then(|| ((f64::from(info.value()) - minimum) / (maximum - minimum)).clamp(0.0, 1.0))
}

fn normalized_stick(info: &AbsInfo) -> Option<f64> {
    if info.minimum() < 0 && info.maximum() > 0 {
        let value = f64::from(info.value());
        let extent = if value < 0.0 {
            -f64::from(info.minimum())
        } else {
            f64::from(info.maximum())
        };
        Some((value / extent).clamp(-1.0, 1.0))
    } else {
        normalized(info).map(|value| value * 2.0 - 1.0)
    }
}

// xpad reports BTN_A/B/X/Y, whose Linux UAPI aliases are SOUTH/EAST/NORTH/WEST.
// Do not interpret NORTH/WEST as the physical positions on an Xbox controller.
const BUTTONS: [(&str, &str, KeyCode); 11] = [
    ("a", "A", KeyCode::BTN_SOUTH),
    ("b", "B", KeyCode::BTN_EAST),
    ("x", "X", KeyCode::BTN_NORTH),
    ("y", "Y", KeyCode::BTN_WEST),
    ("lb", "Left bumper · LB", KeyCode::BTN_TL),
    ("rb", "Right bumper · RB", KeyCode::BTN_TR),
    ("view", "View", KeyCode::BTN_SELECT),
    ("menu", "Menu", KeyCode::BTN_START),
    ("guide", "Xbox / Guide", KeyCode::BTN_MODE),
    ("left_stick", "Left stick · L3", KeyCode::BTN_THUMBL),
    ("right_stick", "Right stick · R3", KeyCode::BTN_THUMBR),
];

fn controller_inputs(
    supported: Option<&AttributeSetRef<KeyCode>>,
    state: Option<&KernelState>,
    rear: bool,
) -> Vec<ControllerInput> {
    let pressed = |key| {
        supported
            .filter(|keys| keys.contains(key))
            .and_then(|_| state?.keys.as_ref().map(|keys| keys.contains(key)))
    };
    let axis = |code: AbsoluteAxisCode| state?.axes[usize::from(code.0)].as_ref();
    let mut inputs = Vec::with_capacity(if rear { 23 } else { 17 });
    for (id, label, key) in BUTTONS {
        let mut input = ControllerInput {
            id: id.into(),
            label: label.into(),
            detail: format!(
                "Standard Xbox output ({key:?}). Inspect-only: onboard mapping is not readable or writable via xpad/evdev. A mapped extra button can produce this same output."
            ),
            pressed: pressed(key),
            value: None,
            x: None,
            y: None,
            control: None,
        };
        let stick = match id {
            "left_stick" => Some((AbsoluteAxisCode::ABS_X, AbsoluteAxisCode::ABS_Y)),
            "right_stick" => Some((AbsoluteAxisCode::ABS_RX, AbsoluteAxisCode::ABS_RY)),
            _ => None,
        };
        if let Some((x, y)) = stick {
            input.x = axis(x).and_then(normalized_stick);
            input.y = axis(y).and_then(normalized_stick);
            input.detail = "Position uses the kernel's advertised axis ranges: −1..1, negative Y is up. Stick click is the standard L3/R3 output. No dead zone, sensitivity, profile or calibration setting is changed.".into();
        }
        inputs.push(input);
    }
    for (id, label, code, key) in [
        (
            "lt",
            "Left trigger · LT",
            AbsoluteAxisCode::ABS_Z,
            KeyCode::BTN_TL2,
        ),
        (
            "rt",
            "Right trigger · RT",
            AbsoluteAxisCode::ABS_RZ,
            KeyCode::BTN_TR2,
        ),
    ] {
        inputs.push(ControllerInput {
            id: id.into(),
            label: label.into(),
            detail: "Analog travel is normalized to 0..1 using the kernel's advertised range. A digital-only trigger is reported only if the device exports its button. Physical trigger-lock position is not readable.".into(),
            pressed: pressed(key),
            value: axis(code).and_then(normalized),
            x: None,
            y: None,
            control: None,
        });
    }
    for (id, label, code, negative, key) in [
        (
            "dpad_up",
            "D-pad up",
            AbsoluteAxisCode::ABS_HAT0Y,
            true,
            KeyCode::BTN_DPAD_UP,
        ),
        (
            "dpad_down",
            "D-pad down",
            AbsoluteAxisCode::ABS_HAT0Y,
            false,
            KeyCode::BTN_DPAD_DOWN,
        ),
        (
            "dpad_left",
            "D-pad left",
            AbsoluteAxisCode::ABS_HAT0X,
            true,
            KeyCode::BTN_DPAD_LEFT,
        ),
        (
            "dpad_right",
            "D-pad right",
            AbsoluteAxisCode::ABS_HAT0X,
            false,
            KeyCode::BTN_DPAD_RIGHT,
        ),
    ] {
        inputs.push(ControllerInput {
            id: id.into(),
            label: label.into(),
            detail: "Standard D-pad output, read from the supported hat axis or digital button. Diagonal directions can both be active.".into(),
            pressed: axis(code)
                .filter(|info| info.minimum() < 0 && info.maximum() > 0)
                .map(|info| if negative { info.value() < 0 } else { info.value() > 0 })
                .or_else(|| pressed(key)),
            value: None,
            x: None,
            y: None,
            control: None,
        });
    }
    if rear {
        for index in 1..=6 {
            inputs.push(ControllerInput {
                id: format!("m{index}"),
                label: format!("Extra button · M{index}"),
                detail: "Independent press and onboard assignment are unavailable via standard xpad/evdev. Pressing a mapped extra button may highlight its assigned standard output on the front, not identify this button. Use Razer Controller Setup for Xbox on a supported Windows/Xbox system; Hyprdeck cannot read or write that profile.".into(),
                pressed: None,
                value: None,
                x: None,
                y: None,
                control: None,
            });
        }
    }
    inputs
}

pub fn unknown_controller_inputs() -> Vec<ControllerInput> {
    controller_inputs(None, None, true)
}

pub struct Snapshot {
    pub group: Group,
    pub inputs: Option<Vec<ControllerInput>>,
    pub error: Option<String>,
}

fn initialized_rows(device: &RawDevice) -> Result<Vec<Row>> {
    rows_from_state(device, &read_state(device)?)
}

fn rows_from_state(device: &RawDevice, state: &KernelState) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    if let Some(keys) = device.supported_keys() {
        let state = state
            .keys
            .as_ref()
            .context("Missing queried button state")?;
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
    if let Some(axes) = device.supported_absolute_axes() {
        for axis in axes.iter() {
            let info = state.axes[usize::from(axis.0)]
                .as_ref()
                .context("Missing queried axis state")?;
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
pub fn snapshot(node: &Node, controller: bool, rear: bool) -> Snapshot {
    let mut group = Group::new(
        &node.id,
        format!("Input details · {}", node.name),
        format!(
            "{} — nonexclusive read-only evdev snapshot",
            node.path.display()
        ),
    )
    .collapsed();
    group.rows.push(Row::info("kernel-name", "Native kernel input name", if controller && node.name.starts_with("Generic X-Box pad") {
        format!("{} — xpad's native name is unchanged. The original 1532:0a3f identity lacks a named entry in the upstream driver table; a product display label does not rename this kernel device or create headset/battery support.", node.name)
    } else {
        node.name.clone()
    }));
    let sample = open(node).and_then(|device| {
        let state = read_state(&device)?;
        let rows = rows_from_state(&device, &state)?;
        let gamepad = controller
            && device
                .supported_keys()
                .is_some_and(|keys| keys.contains(KeyCode::BTN_SOUTH))
            && device.supported_absolute_axes().is_some_and(|axes| {
                axes.contains(AbsoluteAxisCode::ABS_X) && axes.contains(AbsoluteAxisCode::ABS_Y)
            });
        let inputs =
            gamepad.then(|| controller_inputs(device.supported_keys(), Some(&state), rear));
        Ok((rows, inputs))
    });
    match sample {
        Ok((rows, inputs)) => {
            group.rows.push(Row::info("status", "Live input initialized", "Buttons and absolute axes queried directly with EVIOCGKEY/EVIOCGABS, even when idle. Input remains available to Steam/games. Snapshots refresh when Hyprdeck is mapped; no event history or rate is fabricated."));
            group.rows.extend(rows);
            group.rows.push(measurement_row(node));
            Snapshot {
                group,
                inputs,
                error: None,
            }
        }
        Err(error) => {
            let error = format!("{error:#}");
            group.rows.push(Row::info(
                "status",
                "Input unavailable / permission or removal",
                &error,
            ));
            Snapshot {
                group,
                inputs: None,
                error: Some(error),
            }
        }
    }
}

pub fn measurement_row(node: &Node) -> Row {
    Row::info("measurement", "Measure input cadence", "Move sticks/triggers continuously for four seconds. Changed evdev frames are measured, not every USB packet or internal poll.")
        .controlled(json!({"kind":"button","label":"Measure input (4 s)","action":"measure","args":{"device":node.id,"seconds":4},"destructive":false}))
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
    let elapsed = start.elapsed();
    let summary = Row::info(
        "last-measurement",
        "Last input measurement",
        if cadence.frames < 2 {
            format!(
                "{} changed frames in {:.2} s · no polling estimate. SYN_DROPPED: {}.",
                cadence.frames,
                elapsed.as_secs_f64(),
                cadence.drops
            )
        } else {
            format!(
                "{} changed frames in {:.2} s · {:.1} changed frames/s, not physical polling. SYN_DROPPED: {}.",
                cadence.frames,
                elapsed.as_secs_f64(),
                cadence.frames as f64 / elapsed.as_secs_f64(),
                cadence.drops
            )
        },
    );
    group.rows.insert(
        0,
        Row::info(
            "cadence",
            "Observed changed input-frame cadence",
            cadence.description(elapsed),
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
    crate::cache::store("measurement", id, &group, &summary)?;
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

    #[test]
    fn normalization_uses_advertised_ranges_and_clamps() {
        let axis = |value, minimum, maximum| AbsInfo::new(value, minimum, maximum, 0, 0, 0);
        assert_eq!(normalized(&axis(-32768, -32768, 32767)), Some(0.0));
        assert_eq!(normalized(&axis(32767, -32768, 32767)), Some(1.0));
        assert_eq!(normalized(&axis(512, 0, 1024)), Some(0.5));
        assert_eq!(normalized(&axis(-9, 0, 1023)), Some(0.0));
        assert_eq!(normalized(&axis(2048, 0, 1023)), Some(1.0));
        assert_eq!(normalized(&axis(0, 0, 0)), None);
        assert_eq!(normalized(&axis(0, 1, -1)), None);
        assert_eq!(normalized(&axis(i32::MIN, i32::MIN, i32::MAX)), Some(0.0));
        assert_eq!(normalized(&axis(i32::MAX, i32::MIN, i32::MAX)), Some(1.0));
        assert_eq!(normalized_stick(&axis(0, -32768, 32767)), Some(0.0));
        assert_eq!(normalized_stick(&axis(-32768, -32768, 32767)), Some(-1.0));
        assert_eq!(normalized_stick(&axis(32767, -32768, 32767)), Some(1.0));
        assert_eq!(normalized_stick(&axis(-300, -100, 100)), Some(-1.0));
        assert_eq!(normalized_stick(&axis(300, -100, 100)), Some(1.0));
        assert_eq!(normalized_stick(&axis(50, 0, 100)), Some(0.0));
        assert_eq!(normalized_stick(&axis(0, 0, 0)), None);
        assert_eq!(normalized_stick(&axis(0, 1, -1)), None);
        assert_eq!(
            normalized_stick(&axis(i32::MIN, i32::MIN, i32::MAX)),
            Some(-1.0)
        );
    }

    #[test]
    fn xbox_buttons_and_idle_axes_are_initialized() {
        // Fixed UAPI codes independently assert Xbox aliases, especially X/Y.
        let expected = [
            ("a", 304),
            ("b", 305),
            ("x", 307),
            ("y", 308),
            ("lb", 310),
            ("rb", 311),
            ("view", 314),
            ("menu", 315),
            ("guide", 316),
            ("left_stick", 317),
            ("right_stick", 318),
        ];
        let supported: AttributeSet<KeyCode> =
            expected.iter().map(|(_, code)| KeyCode(*code)).collect();
        for (id, code) in expected {
            let mut keys = AttributeSet::new();
            keys.insert(KeyCode(code));
            let mut state = KernelState {
                keys: Some(keys),
                axes: [None; 64],
            };
            state.axes[0] = Some(AbsInfo::new(0, -32768, 32767, 0, 0, 0));
            state.axes[1] = Some(AbsInfo::new(-32768, -32768, 32767, 0, 0, 0));
            state.axes[2] = Some(AbsInfo::new(0, 0, 1023, 0, 0, 0));
            state.axes[16] = Some(AbsInfo::new(-1, -1, 1, 0, 0, 0));
            state.axes[17] = Some(AbsInfo::new(0, -1, 1, 0, 0, 0));
            let inputs = controller_inputs(Some(&supported), Some(&state), true);
            for (candidate, _, _) in BUTTONS {
                assert_eq!(
                    inputs
                        .iter()
                        .find(|input| input.id == candidate)
                        .unwrap()
                        .pressed,
                    Some(candidate == id)
                );
            }
            let stick = inputs
                .iter()
                .find(|input| input.id == "left_stick")
                .unwrap();
            assert_eq!((stick.x, stick.y), (Some(0.0), Some(-1.0)));
            assert_eq!(
                inputs.iter().find(|input| input.id == "lt").unwrap().value,
                Some(0.0)
            );
            assert_eq!(
                inputs
                    .iter()
                    .find(|input| input.id == "dpad_left")
                    .unwrap()
                    .pressed,
                Some(true)
            );
            assert_eq!(
                inputs
                    .iter()
                    .find(|input| input.id == "dpad_right")
                    .unwrap()
                    .pressed,
                Some(false)
            );
            assert_eq!(
                inputs
                    .iter()
                    .find(|input| input.id == "dpad_up")
                    .unwrap()
                    .pressed,
                Some(false)
            );
        }
    }

    #[test]
    fn unavailable_and_unsupported_inputs_are_unknown_not_released() {
        let unknown = unknown_controller_inputs();
        assert_eq!(unknown.len(), 23);
        assert!(unknown.iter().all(|input| input.pressed.is_none()
            && input.value.is_none()
            && input.x.is_none()
            && input.y.is_none()
            && input.control.is_none()));
        let supported: AttributeSet<KeyCode> = [KeyCode::BTN_SOUTH].into_iter().collect();
        let missing = controller_inputs(Some(&supported), None, true);
        assert!(missing.iter().all(|input| input.pressed.is_none()));
        let state = KernelState {
            keys: Some(AttributeSet::new()),
            axes: [None; 64],
        };
        let inputs = controller_inputs(Some(&supported), Some(&state), true);
        assert_eq!(
            inputs.iter().find(|input| input.id == "a").unwrap().pressed,
            Some(false)
        );
        assert_eq!(
            inputs.iter().find(|input| input.id == "b").unwrap().pressed,
            None
        );
        assert!(
            inputs
                .iter()
                .filter(|input| input.id.starts_with('m') && input.id != "menu")
                .all(|input| input.pressed.is_none() && input.control.is_none())
        );
        let generic = controller_inputs(Some(&supported), Some(&state), false);
        assert_eq!(generic.len(), 17);
        assert!(!generic.iter().any(|input| input.id == "m1"));
    }
}
