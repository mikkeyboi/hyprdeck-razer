use crate::{ControllerInput, Group, Row, usb};
use anyhow::{Context, Result, bail};
use evdev::raw_stream::RawDevice;
use evdev::{AbsInfo, AbsoluteAxisCode, AttributeSet, AttributeSetRef, FFEffectCode, KeyCode};
use serde_json::json;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
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
// Sysfs bitmaps are most-significant native-word first. Inspect advertised
// capabilities without opening unrelated keyboard/mouse nodes (or requiring
// permissions for those nodes).
fn capability(bits: &str, code: u16) -> bool {
    let word_bits = usize::BITS as usize;
    bits.split_whitespace()
        .rev()
        .nth(usize::from(code) / word_bits)
        .and_then(|word| usize::from_str_radix(word, 16).ok())
        .is_some_and(|word| word & (1usize << (usize::from(code) % word_bits)) != 0)
}

/// Select the physical device's gamepad, never its keyboard, mouse or auxiliary
/// sensor event node. The returned ID is accepted by measure() and rumble().
pub fn primary_node(device: &usb::Device) -> Result<Option<Node>> {
    let mut best = None;
    let mut best_score = 0;
    for node in nodes(device)? {
        let Some(event) = node.path.file_name() else {
            continue;
        };
        let caps = PathBuf::from("/sys/class/input")
            .join(event)
            .join("device/capabilities");
        let Some(keys) = usb::text(caps.join("key")) else {
            continue;
        };
        let Some(axes) = usb::text(caps.join("abs")) else {
            continue;
        };
        if !capability(&keys, KeyCode::BTN_SOUTH.0)
            || !capability(&axes, AbsoluteAxisCode::ABS_X.0)
            || !capability(&axes, AbsoluteAxisCode::ABS_Y.0)
        {
            continue;
        }
        let layout = AxisLayout::advertised(|axis| capability(&axes, axis.0));
        let score = 1
            + usize::from(layout.right.is_some()) * 4
            + usize::from(layout.left_trigger.is_some())
            + usize::from(layout.right_trigger.is_some())
            + usize::from(capability(&keys, KeyCode::BTN_THUMBR.0));
        if score > best_score {
            best_score = score;
            best = Some(node);
        }
    }
    Ok(best)
}

pub fn supports_rumble(node: &Node) -> Result<bool> {
    Ok(open(node)?
        .supported_ff()
        .is_some_and(|effects| effects.contains(FFEffectCode::FF_RUMBLE)))
}

#[derive(Clone, Copy)]
struct AxisLayout {
    right: Option<(AbsoluteAxisCode, AbsoluteAxisCode)>,
    left_trigger: Option<AbsoluteAxisCode>,
    right_trigger: Option<AbsoluteAxisCode>,
}
impl AxisLayout {
    fn advertised(has: impl Fn(AbsoluteAxisCode) -> bool) -> Self {
        use AbsoluteAxisCode as A;
        let rx_ry = has(A::ABS_RX) && has(A::ABS_RY);
        // HID Wolverine PC mode advertises X/Y/Z/RZ + BRAKE/GAS; xpad uses
        // X/Y/RX/RY + Z/RZ. Never mistake the centered HID Z stick for LT.
        let hid_right =
            !rx_ry && has(A::ABS_Z) && has(A::ABS_RZ) && has(A::ABS_BRAKE) && has(A::ABS_GAS);
        Self {
            right: if rx_ry {
                Some((A::ABS_RX, A::ABS_RY))
            } else if hid_right {
                Some((A::ABS_Z, A::ABS_RZ))
            } else {
                None
            },
            left_trigger: if has(A::ABS_BRAKE) {
                Some(A::ABS_BRAKE)
            } else if !hid_right && has(A::ABS_Z) {
                Some(A::ABS_Z)
            } else {
                None
            },
            right_trigger: if has(A::ABS_GAS) {
                Some(A::ABS_GAS)
            } else if !hid_right && has(A::ABS_RZ) {
                Some(A::ABS_RZ)
            } else {
                None
            },
        }
    }
    fn from_state(state: Option<&KernelState>) -> Self {
        Self::advertised(|axis| {
            state.is_some_and(|state| state.axes[usize::from(axis.0)].is_some())
        })
    }
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

impl KernelState {
    // Update initialized capabilities only: an unsupported axis must stay unknown.
    fn apply(&mut self, event: &evdev::InputEvent) -> bool {
        match event.event_type().0 {
            1 => {
                let Some(keys) = self.keys.as_mut() else {
                    return false;
                };
                let key = KeyCode(event.code());
                let pressed = event.value() != 0;
                let changed = keys.contains(key) != pressed;
                if pressed {
                    keys.insert(key);
                } else {
                    keys.remove(key);
                }
                changed
            }
            3 => {
                let Some(Some(info)) = self.axes.get_mut(usize::from(event.code())) else {
                    return false;
                };
                let changed = info.value() != event.value();
                *info = AbsInfo::new(
                    event.value(),
                    info.minimum(),
                    info.maximum(),
                    info.fuzz(),
                    info.flat(),
                    info.resolution(),
                );
                changed
            }
            _ => false,
        }
    }
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
    let layout = AxisLayout::from_state(state);
    let mut inputs = Vec::with_capacity(if rear { 23 } else { 17 });
    for (id, label, key) in BUTTONS {
        let mut input = ControllerInput {
            id: id.into(),
            label: label.into(),
            detail: format!("Standard gamepad button ({key:?})."),
            pressed: pressed(key),
            value: None,
            x: None,
            y: None,
            control: None,
        };
        let stick = match id {
            "left_stick" => Some((AbsoluteAxisCode::ABS_X, AbsoluteAxisCode::ABS_Y)),
            "right_stick" => layout.right,
            _ => None,
        };
        if let Some((x, y)) = stick {
            input.x = axis(x).and_then(normalized_stick);
            input.y = axis(y).and_then(normalized_stick);
            input.detail = format!(
                "Normalized {x:?}/{y:?} axes (−1.0 to +1.0) and stick click; range comes from the kernel."
            );
        }
        inputs.push(input);
    }
    for (id, label, code, key) in [
        (
            "lt",
            "Left trigger · LT",
            layout.left_trigger,
            KeyCode::BTN_TL2,
        ),
        (
            "rt",
            "Right trigger · RT",
            layout.right_trigger,
            KeyCode::BTN_TR2,
        ),
    ] {
        inputs.push(ControllerInput {
            id: id.into(),
            label: label.into(),
            detail: "Analog trigger travel (0–100%). Toggle rear HyperTrigger locks on the controller for instant mouse-click vs full-range analog pull.".into(),
            pressed: pressed(key),
            value: code.and_then(axis).and_then(normalized),
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
            detail: "8-way floating D-pad directional input.".into(),
            pressed: axis(code)
                .filter(|info| info.minimum() < 0 && info.maximum() > 0)
                .map(|info| {
                    if negative {
                        info.value() < 0
                    } else {
                        info.value() > 0
                    }
                })
                .or_else(|| pressed(key)),
            value: None,
            x: None,
            y: None,
            control: None,
        });
    }
    if rear {
        for (index, placement) in [
            (1, "Left claw-grip bumper · M1"),
            (2, "Right claw-grip bumper · M2"),
            (3, "Upper-left magnetic paddle · M3"),
            (4, "Upper-right magnetic paddle · M4"),
            (5, "Lower-left magnetic paddle · M5"),
            (6, "Lower-right magnetic paddle · M6"),
        ] {
            inputs.push(ControllerInput {
                id: format!("m{index}"),
                label: placement.into(),
                detail: "Hardware-mapped extra button. Edit its onboard action in Buttons or Keys & Macros. This input mode may report only the assigned output, not an independent M-button press.".into(),
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

fn update_inputs(
    inputs: &mut [ControllerInput],
    supported: Option<&AttributeSetRef<KeyCode>>,
    state: &KernelState,
    layout: AxisLayout,
) {
    let pressed = |key| {
        supported
            .filter(|keys| keys.contains(key))
            .and_then(|_| state.keys.as_ref().map(|keys| keys.contains(key)))
    };
    let axis = |code: AbsoluteAxisCode| state.axes[usize::from(code.0)].as_ref();
    for (input, (_, _, key)) in inputs.iter_mut().zip(BUTTONS) {
        input.pressed = pressed(key);
    }
    for (index, axes) in [
        (9, Some((AbsoluteAxisCode::ABS_X, AbsoluteAxisCode::ABS_Y))),
        (10, layout.right),
    ] {
        inputs[index].x = axes.and_then(|(x, _)| axis(x)).and_then(normalized_stick);
        inputs[index].y = axes.and_then(|(_, y)| axis(y)).and_then(normalized_stick);
    }
    for (index, code, key) in [
        (11, layout.left_trigger, KeyCode::BTN_TL2),
        (12, layout.right_trigger, KeyCode::BTN_TR2),
    ] {
        inputs[index].value = code.and_then(axis).and_then(normalized);
        inputs[index].pressed = pressed(key);
    }
    for (index, code, negative, key) in [
        (13, AbsoluteAxisCode::ABS_HAT0Y, true, KeyCode::BTN_DPAD_UP),
        (
            14,
            AbsoluteAxisCode::ABS_HAT0Y,
            false,
            KeyCode::BTN_DPAD_DOWN,
        ),
        (
            15,
            AbsoluteAxisCode::ABS_HAT0X,
            true,
            KeyCode::BTN_DPAD_LEFT,
        ),
        (
            16,
            AbsoluteAxisCode::ABS_HAT0X,
            false,
            KeyCode::BTN_DPAD_RIGHT,
        ),
    ] {
        inputs[index].pressed = axis(code)
            .filter(|info| info.minimum() < 0 && info.maximum() > 0)
            .map(|info| {
                if negative {
                    info.value() < 0
                } else {
                    info.value() > 0
                }
            })
            .or_else(|| pressed(key));
    }
}

pub fn unknown_controller_inputs() -> Vec<ControllerInput> {
    controller_inputs(None, None, true)
}

// Read fixed-size batches without an allocation per event. InputEvent is a
// repr(transparent) wrapper of Linux input_event, containing only integer fields.
fn read_events(device: &RawDevice, events: &mut [evdev::InputEvent]) -> io::Result<usize> {
    let bytes = unsafe {
        libc::read(
            device.as_raw_fd(),
            events.as_mut_ptr().cast(),
            std::mem::size_of_val(events),
        )
    };
    if bytes < 0 {
        return Err(io::Error::last_os_error());
    }
    if bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Input device disconnected (evdev EOF)",
        ));
    }
    if !(bytes as usize).is_multiple_of(std::mem::size_of::<evdev::InputEvent>()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Incomplete evdev event record",
        ));
    }
    Ok(bytes as usize / std::mem::size_of::<evdev::InputEvent>())
}

fn emit_frame(output: &mut impl Write, inputs: &[ControllerInput], status: &str) -> Result<bool> {
    #[derive(serde::Serialize)]
    struct Frame<'a> {
        inputs: &'a [ControllerInput],
        status: &'a str,
    }
    if let Err(error) = serde_json::to_writer(&mut *output, &Frame { inputs, status }) {
        if error.io_error_kind() == Some(io::ErrorKind::BrokenPipe) {
            return Ok(false);
        }
        return Err(error).context("Writing live input frame");
    }
    match output.write_all(b"\n").and_then(|_| output.flush()) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(error).context("Flushing live input frame"),
    }
}

/// Nonexclusive live input for exactly the currently enumerated physical USB
/// device. No grabs, driver detach, force-feedback uploads or HID writes.
pub fn stream(id: &str) -> Result<()> {
    let stdout = io::stdout();
    let mut output = BufWriter::with_capacity(16 * 1024, stdout.lock());
    match stream_to(id, &mut output) {
        Ok(()) => Ok(()),
        Err(error) => {
            // A stale last position must not look live after removal/read failure.
            let status = format!("Input disconnected / unavailable: {error:#}");
            if !emit_frame(&mut output, &unknown_controller_inputs(), &status)? {
                return Ok(());
            }
            Err(error)
        }
    }
}

fn stream_to(id: &str, output: &mut impl Write) -> Result<()> {
    let physical = usb::find(id)?;
    let node =
        primary_node(&physical)?.context("No gamepad event node on this physical USB device")?;
    let device = open(&node)?;
    let mut state = read_state(&device)?;
    let layout = AxisLayout::from_state(Some(&state));
    let mut inputs = controller_inputs(
        device.supported_keys(),
        Some(&state),
        physical.has_rear_buttons(),
    );
    if !emit_frame(output, &inputs, "Live input")? {
        return Ok(());
    }
    let frame_interval = Duration::from_nanos(16_666_667);
    let mut last_axis_frame = Instant::now();
    let mut pending_axis = false;
    let mut lost = false;
    let mut events = [evdev::InputEvent::new(0, 0, 0); 64];
    loop {
        let timeout = if pending_axis && !lost {
            let remaining = frame_interval.saturating_sub(last_axis_frame.elapsed());
            if remaining.is_zero() {
                0
            } else {
                remaining.as_millis().saturating_add(1) as i32
            }
        } else {
            -1
        };
        let mut descriptor = libc::pollfd {
            fd: device.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("Polling live input");
        }
        if descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            bail!("Gamepad disconnected or evdev stream unavailable");
        }
        if result > 0 {
            let count = match read_events(&device, &mut events) {
                Ok(count) => count,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error).context("Reading live gamepad input"),
            };
            for event in &events[..count] {
                if event.event_type().0 == 0 && event.code() == 3 {
                    lost = true;
                    pending_axis = false;
                    continue;
                }
                if lost {
                    if event.event_type().0 == 0 && event.code() == 0 {
                        state =
                            read_state(&device).context("Resynchronizing dropped live input")?;
                        lost = false;
                        update_inputs(&mut inputs, device.supported_keys(), &state, layout);
                        if !emit_frame(
                            output,
                            &inputs,
                            "Live input · resynchronized after dropped events",
                        )? {
                            return Ok(());
                        }
                        last_axis_frame = Instant::now();
                    }
                    continue;
                }
                if state.apply(event) {
                    let button_edge = event.event_type().0 == 1
                        || (event.event_type().0 == 3 && matches!(event.code(), 16 | 17));
                    if button_edge {
                        update_inputs(&mut inputs, device.supported_keys(), &state, layout);
                        if !emit_frame(output, &inputs, "Live input")? {
                            return Ok(());
                        }
                        pending_axis = false;
                        last_axis_frame = Instant::now();
                    } else if event.event_type().0 == 3 {
                        pending_axis = true;
                    }
                }
                if event.event_type().0 == 0
                    && event.code() == 0
                    && pending_axis
                    && last_axis_frame.elapsed() >= frame_interval
                {
                    update_inputs(&mut inputs, device.supported_keys(), &state, layout);
                    if !emit_frame(output, &inputs, "Live input")? {
                        return Ok(());
                    }
                    pending_axis = false;
                    last_axis_frame = Instant::now();
                }
            }
        }
        if pending_axis && !lost && last_axis_frame.elapsed() >= frame_interval {
            update_inputs(&mut inputs, device.supported_keys(), &state, layout);
            if !emit_frame(output, &inputs, "Live input")? {
                return Ok(());
            }
            pending_axis = false;
            last_axis_frame = Instant::now();
        }
    }
}

pub struct Snapshot {
    pub group: Group,
    pub inputs: Option<Vec<ControllerInput>>,
    pub error: Option<String>,
}

fn rows_from_state(device: &RawDevice, state: &KernelState) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    if let Some(keys) = device.supported_keys() {
        let state = state
            .keys
            .as_ref()
            .context("Missing queried button state")?;
        let mut detail = format!("{} advertised keys. Pressed: ", keys.iter().count());
        let mut pressed = false;
        for key in state.iter() {
            if pressed {
                detail.push_str(", ");
            }
            let _ = write!(&mut detail, "{key:?}");
            pressed = true;
        }
        if !pressed {
            detail.push_str("none");
        }
        detail.push_str(". All other supported keys are released (fresh EVIOCGKEY).");
        rows.push(Row::info(
            "key-state",
            "Current buttons / keyboard keys",
            detail,
        ));
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
        format!("{} — xpad's native name is unchanged. This USB identity (e.g. 1532:0a3f or 1532:0a76/0a78) matches xpad's generic vendor/interface wildcard rather than a named entry in xpad_device[]; a product display label does not rename this kernel device or create headset/battery support.", node.name)
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
    Row::info(
        "measurement",
        "Stick movement & event cadence analyzer",
        "For 4 seconds, rotate BOTH sticks around their full range, then release to center. Measures observed movement and changed evdev-frame timing, not physical USB polling.",
    )
    .controlled(json!({
        "kind": "button",
        "label": "Run 4 s test",
        "action": "measure",
        "args": {"device": node.id, "seconds": 4},
        "destructive": false
    }))
}

pub fn rumble_row(node: &Node) -> Option<Row> {
    if !supports_rumble(node).unwrap_or(false) {
        return None;
    }
    Some(
        Row::info(
            "rumble",
            "Haptic rumble test",
            "Test dual-motor force feedback (heavy low-frequency + sharp high-frequency impulse)",
        )
        .controlled(json!({
            "kind": "form",
            "label": "Test Rumble",
            "action": "rumble",
            "args": {"device": node.id},
            "fields": [
                {
                    "id": "profile",
                    "label": "Haptic pattern",
                    "kind": "choice",
                    "value": "dual",
                    "options": [
                        {"value": "dual", "label": "Dual motor pulse (Balanced)"},
                        {"value": "heavy", "label": "Heavy bass motor (Strong)"},
                        {"value": "sharp", "label": "High-frequency motor (Crisp)"},
                        {"value": "double", "label": "Double tap heartbeat"}
                    ]
                }
            ]
        })),
    )
}

pub fn rumble(id: &str, profile: &str) -> Result<Group> {
    let node = find(id)?;
    if !supports_rumble(&node)? {
        bail!("This input node does not advertise evdev FF_RUMBLE; no effect was uploaded");
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&node.path)
        .with_context(|| {
            format!(
                "Opening {} read-write for force feedback",
                node.path.display()
            )
        })?;
    let (strong, weak, duration_ms, pulses): (u16, u16, u16, usize) = match profile {
        "heavy" => (0xffff, 0x0000, 450, 1),
        "sharp" => (0x0000, 0xffff, 350, 1),
        "double" => (0xc000, 0xa000, 160, 2),
        _ => (0xb000, 0x8000, 380, 1),
    };
    // EVIOCSFF = _IOW('E', 0x80, struct ff_effect) = 0x40304580 on x86_64
    // EVIOCRMFF = _IOW('E', 0x81, int) = 0x40044581
    const EVIOCSFF: libc::c_ulong = 0x40304580;
    const EVIOCRMFF: libc::c_ulong = 0x40044581;
    let mut effect = [0u8; 48];
    effect[0..2].copy_from_slice(&0x0050u16.to_ne_bytes()); // FF_RUMBLE
    effect[2..4].copy_from_slice(&(-1i16).to_ne_bytes()); // id = -1 (allocate)
    effect[10..12].copy_from_slice(&duration_ms.to_ne_bytes()); // replay.length
    effect[16..18].copy_from_slice(&strong.to_ne_bytes()); // u.rumble.strong_magnitude
    effect[18..20].copy_from_slice(&weak.to_ne_bytes()); // u.rumble.weak_magnitude
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), EVIOCSFF, effect.as_mut_ptr()) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error())
            .context("Uploading FF_RUMBLE effect via EVIOCSFF");
    }
    let effect_id = i16::from_ne_bytes([effect[2], effect[3]]);
    let mut playback = Ok(());
    for i in 0..pulses {
        let mut ev = [0u8; 24];
        // EV_FF = 0x15, code = effect_id, value = 1 (play)
        ev[16..18].copy_from_slice(&0x0015u16.to_ne_bytes());
        ev[18..20].copy_from_slice(&(effect_id as u16).to_ne_bytes());
        ev[20..24].copy_from_slice(&1i32.to_ne_bytes());
        if let Err(error) = file.write_all(&ev) {
            playback = Err(error);
            break;
        }
        std::thread::sleep(Duration::from_millis(u64::from(duration_ms) + 40));
        if i + 1 < pulses {
            std::thread::sleep(Duration::from_millis(90));
        }
    }
    unsafe {
        let _ = libc::ioctl(file.as_raw_fd(), EVIOCRMFF, libc::c_int::from(effect_id));
    }
    playback.context("Playing evdev force feedback; no successful rumble test result")?;
    let mut group = Group::new(
        format!("rumble:{id}"),
        "Haptic rumble test completed",
        "Force feedback effect played via Linux evdev FF_RUMBLE",
    );
    group.rows.push(Row::info(
        "status",
        format!("Played '{profile}' haptic pattern"),
        format!("Strong motor: 0x{strong:04x}, Weak motor: 0x{weak:04x}, Duration: {duration_ms} ms × {pulses}"),
    ));
    Ok(group)
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
                "Idle / no changed standard input: 0 frames in {:.3} s. No changed-event cadence sample; physical polling is not measured. Move controls throughout measurement. SYN_DROPPED: {}.",
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
            "{} changed frames in {:.3} s ({:.1} frames/s over observation window); {} valid intervals: mean {:.3} ms, median {:.3} ms, p95 {:.3} ms, min {:.3} ms, max {:.3} ms. SYN_DROPPED: {}; invalid/nonmonotonic timestamps: {}. Changed-event cadence only: idle gaps, filtering, radio, kernel batching and application scheduling prevent inferring physical polling.{}",
            self.frames,
            elapsed.as_secs_f64(),
            self.frames as f64 / elapsed.as_secs_f64(),
            self.intervals.len(),
            average,
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
struct StickTrace {
    axes: Option<(AbsoluteAxisCode, AbsoluteAxisCode)>,
    initial: Option<(f64, f64)>,
    last: Option<(f64, f64)>,
    bounds: [f64; 4],
    samples: u64,
    path: f64,
    outer: [Option<f64>; 16],
}
impl StickTrace {
    fn position(
        axes: Option<(AbsoluteAxisCode, AbsoluteAxisCode)>,
        state: &KernelState,
    ) -> Option<(f64, f64)> {
        let (x, y) = axes?;
        Some((
            normalized_stick(state.axes[usize::from(x.0)].as_ref()?)?,
            normalized_stick(state.axes[usize::from(y.0)].as_ref()?)?,
        ))
    }
    fn new(axes: Option<(AbsoluteAxisCode, AbsoluteAxisCode)>, state: &KernelState) -> Self {
        let initial = Self::position(axes, state);
        let (x, y) = initial.unwrap_or((0.0, 0.0));
        Self {
            axes,
            initial,
            last: initial,
            bounds: [x, x, y, y],
            samples: 0,
            path: 0.0,
            outer: [None; 16],
        }
    }
    fn uses(&self, code: u16) -> bool {
        self.axes.is_some_and(|(x, y)| x.0 == code || y.0 == code)
    }
    fn observe(&mut self, state: &KernelState) {
        let Some((x, y)) = Self::position(self.axes, state) else {
            return;
        };
        if self.last == Some((x, y)) {
            return;
        }
        if let Some((px, py)) = self.last {
            self.path += (x - px).hypot(y - py);
        }
        self.last = Some((x, y));
        self.samples += 1;
        self.bounds[0] = self.bounds[0].min(x);
        self.bounds[1] = self.bounds[1].max(x);
        self.bounds[2] = self.bounds[2].min(y);
        self.bounds[3] = self.bounds[3].max(y);
        let radius = x.hypot(y);
        if radius >= 0.65 {
            let angle = y.atan2(x).rem_euclid(std::f64::consts::TAU);
            let sector = ((angle / std::f64::consts::TAU * 16.0) as usize).min(15);
            self.outer[sector] = Some(self.outer[sector].unwrap_or(0.0).max(radius));
        }
    }
    fn description(&self, state: &KernelState) -> String {
        if self.initial.is_none() {
            return "Unavailable: this node does not advertise a complete, normalizable stick axis pair.".into();
        }
        let end = Self::position(self.axes, state)
            .map(|(x, y)| format!("({x:+.3}, {y:+.3}), {:.2}% from center", x.hypot(y) * 100.0))
            .unwrap_or_else(|| "unknown".into());
        if self.samples == 0 {
            return format!(
                "Idle / no observed stick movement: 0 changed-position samples. Final X/Y: {end}. Center drift requires releasing the stick; release was not verified. Range, path and circularity were not measured."
            );
        }
        let sectors = self.outer.iter().flatten().count();
        let circle = if sectors == 16 {
            let rms = (self
                .outer
                .iter()
                .flatten()
                .map(|radius| (radius - 1.0).powi(2))
                .sum::<f64>()
                / 16.0)
                .sqrt()
                * 100.0;
            format!(
                "Circularity estimate: outer-envelope radial RMS deviation from a unit circle {rms:.2}% (16/16 angular sectors reached; movement-dependent, not a hardware calibration)"
            )
        } else {
            format!(
                "Circularity not measured: outer travel reached {sectors}/16 angular sectors; rotate a full circle"
            )
        };
        format!(
            "{} changed-position samples · observed X {:+.3}..{:+.3}, Y {:+.3}..{:+.3} · path {:.3} normalized units. Final X/Y: {end} (release-to-center unverified). {circle}.",
            self.samples, self.bounds[0], self.bounds[1], self.bounds[2], self.bounds[3], self.path
        )
    }
}

pub fn measure(id: &str, duration: Duration) -> Result<Group> {
    let node = find(id)?;
    let device = open(&node)?;
    let mut state = read_state(&device)?;
    let layout = AxisLayout::from_state(Some(&state));
    let mut left = StickTrace::new(
        Some((AbsoluteAxisCode::ABS_X, AbsoluteAxisCode::ABS_Y)),
        &state,
    );
    let mut right = StickTrace::new(layout.right, &state);
    let mut group = Group::new(
        format!("measurement:{id}"),
        format!("Measurement: {}", node.name),
        "Nonexclusive changed-input measurement; not USB/sensor physical polling",
    );
    // Use kernel monotonic timestamps if supported. Failure leaves default realtime
    // timestamps; nonmonotonic intervals are rejected rather than fabricated.
    let clock = libc::CLOCK_MONOTONIC;
    let monotonic =
        unsafe { libc::ioctl(device.as_raw_fd(), 0x400445a0 as libc::c_ulong, &clock) } >= 0;
    let start = Instant::now();
    let mut cadence = Cadence::default();
    let mut events = [evdev::InputEvent::new(0, 0, 0); 64];
    let mut lost = false;
    let mut stick_changed = [false; 2];
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
        let count = match read_events(&device, &mut events) {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(error).context("Reading changed input; device may have been removed");
            }
        };
        for event in &events[..count] {
            cadence.feed(event);
            if event.event_type().0 == 0 && event.code() == 3 {
                lost = true;
                stick_changed = [false; 2];
                left.last = None;
                right.last = None;
                continue;
            }
            if lost {
                if event.event_type().0 == 0 && event.code() == 0 {
                    state = read_state(&device)
                        .context("Resynchronizing measurement after dropped events")?;
                    lost = false;
                }
                continue;
            }
            if state.apply(event) && event.event_type().0 == 3 {
                stick_changed[0] |= left.uses(event.code());
                stick_changed[1] |= right.uses(event.code());
            }
            if event.event_type().0 == 0 && event.code() == 0 {
                if stick_changed[0] {
                    left.observe(&state);
                }
                if stick_changed[1] {
                    right.observe(&state);
                }
                stick_changed = [false; 2];
            }
        }
    }
    let elapsed = start.elapsed();
    let final_state = read_state(&device)?;
    let summary = Row::info(
        "last-measurement",
        "Last stick movement & event cadence test",
        format!(
            "Left: {} movement samples · Right: {} movement samples · {} changed evdev frames / {:.1} s ({:.1} frames/s observed). {}Physical polling is not measured.",
            left.samples,
            right.samples,
            cadence.frames,
            elapsed.as_secs_f64(),
            cadence.frames as f64 / elapsed.as_secs_f64().max(f64::EPSILON),
            if left.samples == 0 && right.samples == 0 {
                "No stick movement observed; rotate both sticks during the test. "
            } else {
                ""
            }
        ),
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
    group.rows.insert(
        0,
        Row::info(
            "left-stick-analysis",
            "Left stick · observed movement",
            left.description(&final_state),
        ),
    );
    group.rows.insert(
        1,
        Row::info(
            "right-stick-analysis",
            "Right stick · observed movement",
            right.description(&final_state),
        ),
    );
    if cadence.drops > 0 {
        group.rows.insert(2, Row::info("incomplete-movement", "Incomplete movement sample", "Events were dropped. State was resynchronized; path skips lost segments and range/circularity may be incomplete. Repeat the test under lower load."));
    }
    crate::cache::store("measurement", id, &group, &summary)?;
    Ok(group)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_gamepad_bitmap_excludes_auxiliary_nodes() {
        assert!(capability(
            "7fff000000000000 0 8000000000 0 0",
            KeyCode::BTN_SOUTH.0
        ));
        assert!(!capability("1f0000 0 0 0 0", KeyCode::BTN_SOUTH.0));
        assert!(capability("30627", AbsoluteAxisCode::ABS_BRAKE.0));
        assert!(!capability("30627", AbsoluteAxisCode::ABS_RX.0));
    }

    #[test]
    fn hid_pc_axes_initialize_both_sticks_and_unpulled_triggers() {
        let mut state = KernelState {
            keys: Some(AttributeSet::new()),
            axes: [None; 64],
        };
        for code in [0, 1, 2, 5] {
            state.axes[code] = Some(AbsInfo::new(32757, 0, 65535, 255, 4095, 0));
        }
        for code in [9, 10] {
            state.axes[code] = Some(AbsInfo::new(1, 0, 1023, 3, 63, 0));
        }
        let layout = AxisLayout::from_state(Some(&state));
        assert_eq!(
            layout.right,
            Some((AbsoluteAxisCode::ABS_Z, AbsoluteAxisCode::ABS_RZ))
        );
        assert_eq!(layout.left_trigger, Some(AbsoluteAxisCode::ABS_BRAKE));
        assert_eq!(layout.right_trigger, Some(AbsoluteAxisCode::ABS_GAS));
        let mut inputs = controller_inputs(None, Some(&state), true);
        for id in ["left_stick", "right_stick"] {
            let input = inputs.iter().find(|input| input.id == id).unwrap();
            assert!(input.x.unwrap().abs() < 0.001 && input.y.unwrap().abs() < 0.001);
        }
        for id in ["lt", "rt"] {
            assert!(
                inputs
                    .iter()
                    .find(|input| input.id == id)
                    .unwrap()
                    .value
                    .unwrap()
                    < 0.002
            );
        }
        assert!(state.apply(&evdev::InputEvent::new(3, 2, 65535)));
        assert!(state.apply(&evdev::InputEvent::new(3, 10, 1023)));
        assert!(!state.apply(&evdev::InputEvent::new(3, 3, 42)));
        update_inputs(&mut inputs, None, &state, layout);
        assert_eq!(
            inputs
                .iter()
                .find(|input| input.id == "right_stick")
                .unwrap()
                .x,
            Some(1.0)
        );
        assert_eq!(
            inputs.iter().find(|input| input.id == "lt").unwrap().value,
            Some(1.0)
        );
        assert!(state.axes[3].is_none());
    }

    #[test]
    fn xpad_axes_keep_separate_right_stick_and_triggers() {
        let layout = AxisLayout::advertised(|axis| matches!(axis.0, 0..=5));
        assert_eq!(
            layout.right,
            Some((AbsoluteAxisCode::ABS_RX, AbsoluteAxisCode::ABS_RY))
        );
        assert_eq!(layout.left_trigger, Some(AbsoluteAxisCode::ABS_Z));
        assert_eq!(layout.right_trigger, Some(AbsoluteAxisCode::ABS_RZ));
        assert!(AxisLayout::advertised(|_| false).right.is_none());
    }

    #[test]
    fn stick_analysis_requires_movement_and_full_outer_coverage() {
        let mut state = KernelState {
            keys: None,
            axes: [None; 64],
        };
        for code in [0, 1] {
            state.axes[code] = Some(AbsInfo::new(0, -1000, 1000, 0, 0, 0));
        }
        let mut trace = StickTrace::new(
            Some((AbsoluteAxisCode::ABS_X, AbsoluteAxisCode::ABS_Y)),
            &state,
        );
        assert!(
            trace
                .description(&state)
                .contains("Idle / no observed stick movement")
        );
        for sector in 0..16 {
            let angle = (f64::from(sector) + 0.5) / 16.0 * std::f64::consts::TAU;
            state.apply(&evdev::InputEvent::new(
                3,
                0,
                (angle.cos() * 1000.0).round() as i32,
            ));
            state.apply(&evdev::InputEvent::new(
                3,
                1,
                (angle.sin() * 1000.0).round() as i32,
            ));
            trace.observe(&state);
        }
        assert_eq!(trace.samples, 16);
        assert!(trace.path > 6.0);
        assert!(trace.description(&state).contains("Circularity estimate"));
        assert!(
            trace
                .description(&state)
                .contains("release-to-center unverified")
        );
    }

    #[test]
    fn live_button_edges_update_initialized_inputs_without_rebuilding_metadata() {
        let supported: AttributeSet<KeyCode> = [KeyCode::BTN_SOUTH].into_iter().collect();
        let mut state = KernelState {
            keys: Some(AttributeSet::new()),
            axes: [None; 64],
        };
        let layout = AxisLayout::from_state(Some(&state));
        let mut inputs = controller_inputs(Some(&supported), Some(&state), true);
        assert_eq!(inputs[0].pressed, Some(false));
        assert!(state.apply(&evdev::InputEvent::new(1, KeyCode::BTN_SOUTH.0, 1)));
        update_inputs(&mut inputs, Some(&supported), &state, layout);
        assert_eq!(inputs[0].pressed, Some(true));
        assert!(!state.apply(&evdev::InputEvent::new(1, KeyCode::BTN_SOUTH.0, 2)));
        assert!(state.apply(&evdev::InputEvent::new(1, KeyCode::BTN_SOUTH.0, 0)));
        update_inputs(&mut inputs, Some(&supported), &state, layout);
        assert_eq!(inputs[0].pressed, Some(false));
        assert!(inputs[17..].iter().all(|input| input.pressed.is_none()));
    }
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
