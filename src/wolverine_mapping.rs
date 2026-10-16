//! Native single-record onboard assignments recovered from Synapse's SagePC implementation.
//!
//! Protocol evidence: `mw-main.js` module 87969 (`rzDevice25`) defines button-list
//! 02/84 (80 bytes), single-assignment get 02/8c and set 02/0c, and active-profile
//! get 05/84 (one byte) / set 05/04 (`[profile]`). Profile-list get is 05/81
//! (80 bytes), with `[count, ids...]` replies. SagePC inherits these methods.
//! Module 10511 (`obmEngineController`) uses a ten-byte assignment packet in both
//! directions: `[profile, button, mode, function, size, data[5]]`. Module 35922
//! defines normal mode 0, disabled function 0 and controller function 16.
//! Module 58701 maps M1–M6 to IDs 16–21 and the controller targets below;
//! module 4442's `controllerGroupToFnDataByte` encodes one target ID followed by
//! four zero bytes, with size 1. The Wolverine V4 Pro device schema explicitly
//! supports `controllerGroup` and `disableGroup`. We never rewrite a profile,
//! switch the active profile, or manufacture default assignment records.
//! Its generic controller encoder deliberately reuses the firmware button IDs
//! for output targets: module 4442 export BP reads module 58701 export uF's
//! `BUTTON_A.fwID` (32), for example. Module 10511 skips controller records
//! whose target equals the source button or zero; zero is a default/no-override
//! sentinel, not an alternate zero-based encoding of A.

use super::{Session, keyboard};
use crate::{Group, Row, usb};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

const NORMAL_MODE: u8 = 0;
const CONTROLLER_FUNCTION: u8 = 16;
const BUTTONS: [(&str, &str, u8); 6] = [
    ("m1", "M1", 16),
    ("m2", "M2", 17),
    ("m3", "M3", 18),
    ("m4", "M4", 19),
    ("m5", "M5", 20),
    ("m6", "M6", 21),
];

// These are the generic controller encoder's actual supported cases, not Xbox
// input bitmasks. In particular View/Menu are 52/53, not Start/Back 42/43.
const TARGETS: [(&str, &str, u8); 22] = [
    ("A", "A", 32),
    ("B", "B", 33),
    ("X", "X", 34),
    ("Y", "Y", 35),
    ("LB", "Left bumper (LB)", 36),
    ("RB", "Right bumper (RB)", 37),
    ("LT", "Left trigger (LT)", 38),
    ("RT", "Right trigger (RT)", 39),
    ("L3", "Left stick press (L3)", 40),
    ("R3", "Right stick press (R3)", 41),
    ("dpad-up", "D-pad up", 44),
    ("dpad-down", "D-pad down", 45),
    ("dpad-left", "D-pad left", 46),
    ("dpad-right", "D-pad right", 47),
    ("view", "View", 52),
    ("menu", "Menu", 53),
    ("M1", "M1", 16),
    ("M2", "M2", 17),
    ("M3", "M3", 18),
    ("M4", "M4", 19),
    ("M5", "M5", 20),
    ("M6", "M6", 21),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Disabled,
    Controller(u8),
    Keyboard { modifiers: u8, key: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Assignment {
    function: u8,
    size: u8,
    data: [u8; 5],
}

impl Assignment {
    fn parse(payload: &[u8], profile: u8, button: u8) -> Result<Self> {
        ensure!(
            payload.len() >= 10,
            "Truncated onboard assignment reply: expected ten bytes, received {}",
            payload.len()
        );
        ensure!(
            payload[..3] == [profile, button, NORMAL_MODE],
            "Onboard assignment reply does not match profile {profile}, button {button}, normal mode"
        );
        ensure!(
            payload[4] <= 5,
            "Onboard assignment function data exceeds the five-byte record"
        );
        let mut data = [0; 5];
        data.copy_from_slice(&payload[5..10]);
        Ok(Self {
            function: payload[3],
            size: payload[4],
            data,
        })
    }

    fn for_target(target: Target) -> Self {
        match target {
            Target::Disabled => Self {
                function: 0,
                size: 0,
                data: [0; 5],
            },
            Target::Controller(id) => Self {
                function: CONTROLLER_FUNCTION,
                size: 1,
                data: [id, 0, 0, 0, 0],
            },
            Target::Keyboard { modifiers, key } => Self {
                function: 2,
                size: 2,
                data: [modifiers, key, 0, 0, 0],
            },
        }
    }

    fn matches(self, other: Self) -> bool {
        self.function == other.function
            && self.size == other.size
            && self.data[..usize::from(self.size)] == other.data[..usize::from(other.size)]
    }

    fn description(self) -> String {
        if self.function == 0 && self.size == 0 {
            return "Disabled".into();
        }
        if self.function == CONTROLLER_FUNCTION && self.size == 1 {
            if self.data[0] == 0 {
                return "Firmware default".into();
            }
            if let Some((_, label, _)) = TARGETS.iter().find(|(_, _, id)| *id == self.data[0]) {
                return (*label).into();
            }
        }
        if self.function == 2 && self.size == 2 {
            return format!(
                "Keyboard: {}",
                keyboard::describe(self.data[0], self.data[1])
            );
        }
        format!(
            "Existing function 0x{:02x}, data {:02x?} (kept unless explicitly replaced)",
            self.function,
            &self.data[..usize::from(self.size)]
        )
    }
}

fn active_profile(session: &mut Session) -> Result<u8> {
    let reply = session.command(0x05, 0x84, &[0])?;
    reply
        .payload()
        .first()
        .copied()
        .context("Active-profile reply contains no profile ID")
}

fn ensure_active_profile(session: &mut Session, expected: u8) -> Result<()> {
    let actual = active_profile(session)?;
    ensure!(
        actual == expected,
        "Active onboard profile changed from {expected} to {actual}; refresh before changing assignments"
    );
    Ok(())
}

fn available_buttons(session: &mut Session) -> Result<[bool; 6]> {
    let reply = session.command(0x02, 0x84, &[0; 80])?;
    let payload = reply.payload();
    let count = usize::from(*payload.first().context("Empty onboard button-list reply")?);
    ensure!(count < payload.len(), "Truncated onboard button-list reply");
    let mut available = [false; 6];
    for id in &payload[1..1 + count] {
        if let Some(index) = BUTTONS.iter().position(|(_, _, button)| button == id) {
            available[index] = true;
        }
    }
    Ok(available)
}

fn read_assignment(session: &mut Session, profile: u8, button: u8) -> Result<Assignment> {
    let mut request = [0; 10];
    request[..3].copy_from_slice(&[profile, button, NORMAL_MODE]);
    let reply = session.command(0x02, 0x8c, &request)?;
    Assignment::parse(reply.payload(), profile, button)
}

fn write_assignment(
    session: &mut Session,
    profile: u8,
    button: u8,
    assignment: Assignment,
) -> Result<()> {
    let mut request = [0; 10];
    request[..5].copy_from_slice(&[
        profile,
        button,
        NORMAL_MODE,
        assignment.function,
        assignment.size,
    ]);
    request[5..].copy_from_slice(&assignment.data);
    session.command(0x02, 0x0c, &request)?;
    let actual = read_assignment(session, profile, button)
        .context("Assignment was sent but could not be read back; refresh before retrying")?;
    ensure!(
        actual.matches(assignment),
        "Assignment was sent but onboard readback differs for profile {profile}, button {button}; refresh before retrying"
    );
    Ok(())
}

pub fn groups(device: &usb::Device, session: &mut Session) -> Result<[Group; 2]> {
    let profile = active_profile(session)?;
    let available = available_buttons(session)?;
    ensure!(
        available.iter().any(|present| *present),
        "Firmware did not advertise any M1–M6 onboard assignments"
    );
    let mut sources = Vec::with_capacity(6);
    let mut summary = String::new();
    for (index, (field, name, button)) in BUTTONS.iter().enumerate() {
        if !available[index] {
            continue;
        }
        let current = read_assignment(session, profile, *button)
            .with_context(|| format!("Reading {name} onboard assignment"))?;
        let description = current.description();
        if !summary.is_empty() {
            summary.push_str(" · ");
        }
        summary.push_str(name);
        summary.push_str(" → ");
        summary.push_str(&description);
        sources.push(json!({"value":field,"label":format!("{name} · {description}")}));
    }
    ensure_active_profile(session, profile)?;
    let selected = if available[2] {
        "m3"
    } else {
        BUTTONS
            .iter()
            .zip(available)
            .find(|(_, present)| *present)
            .map(|((field, _, _), _)| *field)
            .context("No assignable buttons")?
    };
    let mut targets = vec![
        json!({"value":"keep","label":"Keep current"}),
        json!({"value":"default","label":"Firmware default"}),
        json!({"value":"disabled","label":"Disabled"}),
    ];
    targets.extend(
        TARGETS
            .iter()
            .map(|(value, label, _)| json!({"value":value,"label":label})),
    );
    let mut buttons = Group::new(
        format!("wolverine-mapping:{}", device.id),
        "Onboard buttons",
        "M1/M2 are extra shoulder buttons; M3–M6 are rear paddles. Change one assignment at a time, without replacing the active profile.",
    );
    buttons.rows.push(Row::info(
        "assignments",
        format!("Profile {profile} · current assignments"),
        summary,
    ));
    buttons.rows.push(Row::info("mapping", "Assign a controller button", "Changes only the selected button. Firmware default removes its explicit override; Keep current preserves keyboard and other existing functions.").controlled(json!({
        "kind":"form", "label":"Save assignment", "action":"wolverine-configure",
        "args":{"device":device.id,"operation":"mapping","profile":profile.to_string()},
        "fields":[
            {"id":"button","label":"Extra button","kind":"choice","value":selected,"options":sources.clone()},
            {"id":"target","label":"Controller action","kind":"choice","value":"keep","options":targets}
        ]
    })));
    let mut shortcuts = Group::new(
        format!("wolverine-keyboard:{}", device.id),
        "Onboard keyboard shortcut",
        "A single key or chord saved on the controller—not a recorded macro. Physical USB keys follow your desktop keyboard layout.",
    );
    shortcuts.rows.push(Row::info("shortcut", format!("Assign a keyboard shortcut · profile {profile}"), "Examples: F13, Ctrl+Shift+S, Alt+Tab. Letters, digits, F1–F24 and named keys such as Enter are supported. Select the active profile on the controller before editing.").controlled(json!({
        "kind":"form", "label":"Save keyboard shortcut", "action":"wolverine-configure",
        "args":{"device":device.id,"operation":"keyboard","profile":profile.to_string()},
        "fields":[
            {"id":"button","label":"Extra button","kind":"choice","value":selected,"options":sources},
            {"id":"shortcut","label":"Key or shortcut","kind":"text","value":""}
        ]
    })));
    Ok([buttons, shortcuts])
}

fn selected(args: &Value) -> Result<(u8, usize, &Value)> {
    let profile = crate::string_arg(args, "profile")?
        .parse::<u8>()
        .context("Invalid onboard profile")?;
    let value = args
        .get("value")
        .filter(|value| value.is_object())
        .context("Missing assignment form value")?;
    let source = crate::string_arg(value, "button")?;
    let index = BUTTONS
        .iter()
        .position(|(field, _, _)| *field == source)
        .context("Choose an advertised M1–M6 button")?;
    Ok((profile, index, value))
}

fn save(session: &mut Session, profile: u8, index: usize, target: Target) -> Result<()> {
    ensure_active_profile(session, profile)?;
    let (_, name, button) = BUTTONS[index];
    ensure!(
        available_buttons(session)?[index],
        "Firmware does not advertise {name}"
    );
    let desired = Assignment::for_target(target);
    let current = read_assignment(session, profile, button)?;
    if !current.matches(desired) {
        ensure_active_profile(session, profile)?;
        write_assignment(session, profile, button, desired)?;
    }
    ensure_active_profile(session, profile)
}

pub fn action(session: &mut Session, args: &Value) -> Result<()> {
    let (profile, index, value) = selected(args)?;
    let target = match crate::string_arg(value, "target")? {
        "keep" => return ensure_active_profile(session, profile),
        "default" => Target::Controller(0),
        "disabled" => Target::Disabled,
        target => Target::Controller(
            TARGETS
                .iter()
                .find(|(name, _, _)| *name == target)
                .map(|(_, _, id)| *id)
                .with_context(|| format!("Unsupported controller assignment {target:?}"))?,
        ),
    };
    save(session, profile, index, target)
}

pub fn keyboard_action(session: &mut Session, args: &Value) -> Result<()> {
    let (profile, index, value) = selected(args)?;
    let (modifiers, key) = keyboard::parse(crate::string_arg(value, "shortcut")?)?;
    save(session, profile, index, Target::Keyboard { modifiers, key })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignment_readback_rejects_wrong_record_and_truncated_function_data() {
        let record = [2, 18, 0, 16, 1, 32, 0, 0, 0, 0];
        assert!(Assignment::parse(&record, 1, 18).is_err());
        assert!(Assignment::parse(&record, 2, 19).is_err());
        assert!(Assignment::parse(&record[..9], 2, 18).is_err());
        let mut wrong_mode = record;
        wrong_mode[2] = 1;
        assert!(Assignment::parse(&wrong_mode, 2, 18).is_err());
        let mut oversized = record;
        oversized[4] = 6;
        assert!(Assignment::parse(&oversized, 2, 18).is_err());
    }

    #[test]
    fn verification_compares_only_declared_function_data() {
        let expected = Assignment::for_target(Target::Controller(32));
        let padded = Assignment::parse(&[2, 18, 0, 16, 1, 32, 9, 8, 7, 6], 2, 18).unwrap();
        assert!(padded.matches(expected));
        let different = Assignment::parse(&[2, 18, 0, 16, 1, 33, 0, 0, 0, 0], 2, 18).unwrap();
        assert!(!different.matches(expected));
        let different_function =
            Assignment::parse(&[2, 18, 0, 2, 1, 32, 0, 0, 0, 0], 2, 18).unwrap();
        assert!(!different_function.matches(expected));
    }
}
