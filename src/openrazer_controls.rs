use super::*;
#[path = "dpi_presets.rs"]
mod presets;
pub use presets::action as dpi_presets;

const DPI: &str = "razer.device.dpi";
const MISC: &str = "razer.device.misc";
const POWER: &str = "razer.device.power";

fn method<'a>(
    device: &'a DeviceSnapshot,
    interface: &str,
    member: &str,
    signature: &str,
) -> Option<&'a Method> {
    device.methods.iter().find(|method| {
        method.interface == interface
            && method.name == member
            && method
                .input
                .iter()
                .map(|arg| arg.signature.as_str())
                .collect::<String>()
                == signature
    })
}
fn number_field(id: &str, label: &str, value: &Json, min: f64, max: f64) -> Json {
    json!({"id":id,"label":label,"kind":"number","value":value,"min":min,"max":max,"step":1,"options":[]})
}
fn choice_field(id: &str, label: &str, value: &str, options: Vec<Json>) -> Json {
    json!({"id":id,"label":label,"kind":"choice","value":value,"options":options})
}
fn form(device: &DeviceSnapshot, operation: &str, interface: &str, fields: Vec<Json>) -> Json {
    json!({"kind":"form","fields":fields,"action":"openrazer-configure","args":{"device":device.serial,"operation":operation,"interface":interface},"label":"Apply"})
}
fn numeric(
    device: &DeviceSnapshot,
    interface: &str,
    member: &str,
    current: &Json,
    min: f64,
    max: f64,
) -> Json {
    json!({"kind":"number","value":current,"min":min,"max":max,"step":1,"action":"openrazer-configure","args":{"device":device.serial,"operation":"scalar","interface":interface,"member":member}})
}
fn color_at(colors: &Json, offset: usize) -> Option<String> {
    let colors = colors.as_array()?;
    let r = u8::try_from(colors.get(offset)?.as_u64()?).ok()?;
    let g = u8::try_from(colors.get(offset + 1)?.as_u64()?).ok()?;
    let b = u8::try_from(colors.get(offset + 2)?.as_u64()?).ok()?;
    Some(format!("#{r:02X}{g:02X}{b:02X}"))
}
fn rgb(value: &Json) -> Result<Vec<Json>> {
    let value = value.as_str().context("Color must be #RRGGBB")?;
    if value.len() != 7
        || !value.starts_with('#')
        || !value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
    {
        bail!("Color must be exactly #RRGGBB");
    }
    (1..7)
        .step_by(2)
        .map(|index| Ok(json!(u8::from_str_radix(&value[index..index + 2], 16)?)))
        .collect()
}
fn integer(value: &Json, min: u64, max: u64) -> Result<u64> {
    let value = unsigned_integer(value)?;
    if !(min..=max).contains(&value) {
        bail!("Value must be {min}..={max}");
    }
    Ok(value)
}
fn field<'a>(value: &'a Json, id: &str) -> Result<&'a Json> {
    value
        .as_object()
        .context("Form args.value must be an object")?
        .get(id)
        .with_context(|| format!("Missing form field {id}"))
}
fn max_dpi(device: &DeviceSnapshot) -> Option<u64> {
    device
        .value(DPI, "maxDPI")?
        .as_u64()
        .filter(|max| (1..=65535).contains(max))
}
fn validate_dpi(device: &DeviceSnapshot, x: &Json, y: &Json) -> Result<Vec<Json>> {
    let max = max_dpi(device).context("Current device maximum DPI unavailable")?;
    let x = integer(x, 100, max)?;
    let y = integer(y, 0, max)?;
    if let Some(available) = device
        .value(DPI, "availableDPI")
        .and_then(Json::as_array)
        .filter(|values| !values.is_empty())
    {
        if y != 0 || !available.iter().any(|value| value.as_u64() == Some(x)) {
            bail!("Single-axis DPI must use an advertised DPI and Y=0");
        }
    } else if y < 100 {
        bail!("Two-axis DPI requires X and Y >= 100");
    }
    Ok(vec![json!(x), json!(y)])
}

fn poll_rates(device: &DeviceSnapshot) -> Option<Vec<u64>> {
    method(device, MISC, "getPollRate", "")?;
    method(device, MISC, "setPollRate", "q")?;
    device.value(MISC, "getPollRate")?.as_u64()?;
    let rates = if device
        .methods
        .iter()
        .any(|method| method.interface == MISC && method.name == "getSupportedPollRates")
    {
        device
            .value(MISC, "getSupportedPollRates")?
            .as_array()?
            .iter()
            .map(Json::as_u64)
            .collect::<Option<Vec<_>>>()?
    } else if matches!(device.product_id(), Some(0x0086 | 0x0088)) {
        // OpenRazer device_base supplies these POLL_RATES for Basilisk classes
        // with set_poll_rate but no explicit table or rates D-Bus getter.
        vec![125, 500, 1000]
    } else {
        return None;
    };
    if rates.is_empty() || rates.iter().any(|rate| !(1..=65535).contains(rate)) {
        return None;
    }
    Some(rates)
}

pub fn product_groups(device: &DeviceSnapshot, component: &str) -> Vec<(&'static str, Group)> {
    let mut groups = Vec::new();
    let mut sensitivity = Group::new(
        format!("settings:{}:dpi", device.serial),
        format!("{component} sensitivity"),
        "Device DPI and daemon-configured polling; Apply is explicit",
    );
    let has_device_stages = method(device, DPI, "getDPIStages", "").is_some()
        && method(device, DPI, "setDPIStages", "ya(qq)").is_some();
    let available = device
        .value(DPI, "availableDPI")
        .and_then(Json::as_array)
        .filter(|values| !values.is_empty());
    let host_presets = matches!(device.product_id(), Some(0x0086 | 0x0088))
        && available.is_none()
        && !has_device_stages
        && method(device, DPI, "getDPI", "").is_some()
        && method(device, DPI, "setDPI", "qq").is_some()
        && max_dpi(device).is_some();
    let mut host_control = false;
    if host_presets {
        match presets::control(device) {
            Ok(control) => {
                let subtitle = if control["x"].is_null() {
                    "Current DPI unavailable · editable host presets are not hardware readback"
                        .into()
                } else {
                    format!(
                        "Current {} × {} DPI · maximum {}",
                        control["x"], control["y"], control["max"]
                    )
                };
                sensitivity
                    .rows
                    .push(Row::info("dpi", "Sensitivity", subtitle).controlled(control));
                host_control = true;
            }
            Err(error) => sensitivity.rows.push(Row::info(
                "host-presets-error",
                "Host DPI presets unavailable",
                format!("{error:#}. Existing config is preserved."),
            )),
        }
    }
    if !host_control
        && let (Some(current), Some(max)) = (
            device
                .value(DPI, "getDPI")
                .and_then(Json::as_array)
                .filter(|pair| pair.len() == 2),
            max_dpi(device),
        )
        && method(device, DPI, "setDPI", "qq").is_some()
        && validate_dpi(device, &current[0], &current[1]).is_ok()
    {
        let fields = if let Some(available) = available {
            vec![choice_field(
                "x",
                "DPI",
                &current[0].to_string(),
                available
                    .iter()
                    .filter_map(Json::as_u64)
                    .map(|dpi| json!({"value":dpi.to_string(),"label":format!("{dpi} DPI")}))
                    .collect(),
            )]
        } else {
            vec![
                number_field("x", "Horizontal DPI", &current[0], 100.0, max as f64),
                number_field("y", "Vertical DPI", &current[1], 100.0, max as f64),
            ]
        };
        sensitivity.rows.push(
            Row::info(
                "dpi",
                "Sensitivity",
                format!(
                    "Current {} × {} DPI · maximum {max}",
                    current[0], current[1]
                ),
            )
            .controlled(form(device, "dpi", DPI, fields)),
        );
    }
    if let (Some(current), Some(supported)) =
        (device.value(MISC, "getPollRate"), poll_rates(device))
    {
        let options: Vec<Json> = supported
            .iter()
            .map(|rate| json!({"value":rate.to_string(),"label":format!("{rate} Hz")}))
            .collect();
        let current_rate = current.to_string();
        if options
            .iter()
            .any(|option| option["value"].as_str() == Some(current_rate.as_str()))
        {
            sensitivity.rows.push(Row::info("polling", "Configured polling", "Device-advertised rates, or verified OpenRazer Basilisk defaults; not a controller override.").controlled(json!({"kind":"choice","value":current.to_string(),"options":options,"action":"openrazer-configure","args":{"device":device.serial,"operation":"polling"}})));
        }
    }
    if let (Some(stages), Some(max)) = (
        device
            .value(DPI, "getDPIStages")
            .and_then(Json::as_array)
            .filter(|values| values.len() == 2),
        max_dpi(device),
    ) {
        if let Some(pairs) = stages[1].as_array().filter(|pairs| {
            !pairs.is_empty()
                && pairs.len() <= 5
                && pairs
                    .iter()
                    .all(|pair| pair.as_array().is_some_and(|pair| pair.len() == 2))
        }) && method(device, DPI, "setDPIStages", "ya(qq)").is_some()
            && integer(&stages[0], 1, pairs.len() as u64).is_ok()
            && pairs
                .iter()
                .all(|pair| validate_dpi(device, &pair[0], &pair[1]).is_ok())
        {
            let mut fields = vec![number_field(
                "active",
                "Active stage (one-based)",
                &stages[0],
                1.0,
                pairs.len() as f64,
            )];
            for (index, pair) in pairs.iter().enumerate() {
                fields.push(number_field(
                    &format!("x{}", index + 1),
                    &format!("Stage {} horizontal DPI", index + 1),
                    &pair[0],
                    100.0,
                    max as f64,
                ));
                fields.push(number_field(
                    &format!("y{}", index + 1),
                    &format!("Stage {} vertical DPI", index + 1),
                    &pair[1],
                    100.0,
                    max as f64,
                ));
            }
            sensitivity.rows.push(
                Row::info(
                    "stages",
                    "DPI stages",
                    "Edit the actual device stage table and select the active stage.",
                )
                .controlled(form(device, "stages", DPI, fields)),
            );
        }
    } else if matches!(device.product_id(), Some(0x0086 | 0x0088)) {
        sensitivity.rows.push(Row::info("host-presets", "Host presets, not onboard stages", "Save stores editable presets on this computer only. Apply, activate and next/previous explicitly call setDPI. The physical DPI-cycle button does not use host presets; refresh only reads the current daemon DPI."));
    }
    if !sensitivity.rows.is_empty() {
        groups.push(("sensitivity", sensitivity));
    }
    if let Some(value) = device
        .value("razer.device.lighting.brightness", "getBrightness")
        .filter(|value| {
            value
                .as_f64()
                .is_some_and(|number| (0.0..=100.0).contains(&number))
        })
        && method(
            device,
            "razer.device.lighting.brightness",
            "setBrightness",
            "d",
        )
        .is_some()
    {
        let mut group = Group::new(
            format!("settings:{}:brightness", device.serial),
            format!("{component} brightness"),
            "Actual OpenRazer brightness interface",
        );
        group.rows.push(
            Row::info(
                "brightness",
                "Brightness",
                "Percent · applies to this component, not mouse logo/scroll zones",
            )
            .controlled(numeric(
                device,
                "razer.device.lighting.brightness",
                "setBrightness",
                value,
                0.0,
                100.0,
            )),
        );
        groups.push(("lighting", group));
    }
    for (zone, prefix, title) in [
        ("logo", "Logo", "Mouse logo"),
        ("scroll", "Scroll", "Scroll wheel"),
        ("left", "Left", "Left lighting strip"),
        ("right", "Right", "Right lighting strip"),
        ("backlight", "Backlight", "Backlight"),
        ("chroma", "", "Dock / Chroma lighting"),
    ] {
        let interface = format!("razer.device.lighting.{zone}");
        if !device
            .methods
            .iter()
            .any(|method| method.interface == interface)
        {
            continue;
        }
        let mut group = Group::new(
            format!("settings:{}:{zone}", device.serial),
            format!("{component} · {title}"),
            "OpenRazer zone state; no writes on refresh",
        );
        let brightness = format!("get{prefix}Brightness");
        let setter = format!("set{prefix}Brightness");
        if let Some(value) = device.value(&interface, &brightness).filter(|value| {
            value
                .as_f64()
                .is_some_and(|number| (0.0..=100.0).contains(&number))
        }) && method(device, &interface, &setter, "d").is_some()
        {
            group.rows.push(
                Row::info(
                    "brightness",
                    "Brightness",
                    "Percent · Apply changes only this component and zone",
                )
                .controlled(numeric(device, &interface, &setter, value, 0.0, 100.0)),
            );
        }
        let effect_getter = format!("get{prefix}Effect");
        let colors_getter = format!("get{prefix}EffectColors");
        let speed_getter = format!("get{prefix}EffectSpeed");
        let direction_getter = format!("get{prefix}WaveDir");
        let colors = device.value(&interface, &colors_getter);
        let color1 = colors.and_then(|colors| color_at(colors, 0));
        let color2 = colors.and_then(|colors| color_at(colors, 3));
        let speed = device
            .value(&interface, &speed_getter)
            .filter(|value| value.as_u64().is_some_and(|value| (1..=4).contains(&value)));
        let direction = device
            .value(&interface, &direction_getter)
            .filter(|value| value.as_u64().is_some_and(|value| (1..=2).contains(&value)));
        let mut options = Vec::new();
        for (suffix, effect, label, signature) in effects() {
            let supported = method(
                device,
                &interface,
                &format!("set{prefix}{suffix}"),
                signature,
            )
            .is_some();
            let fields_known = match *effect {
                "static" | "breathSingle" => color1.is_some(),
                "breathDual" => color1.is_some() && color2.is_some(),
                "reactive" => color1.is_some() && speed.is_some(),
                "wave" => {
                    direction.is_some()
                        && matches!(device.product_id(), Some(0x0086 | 0x0088 | 0x007e))
                }
                _ => true,
            };
            if supported && fields_known {
                options.push(json!({"value":effect,"label":label}));
            }
        }
        if !options.is_empty() {
            let current = device
                .value(&interface, &effect_getter)
                .and_then(Json::as_str)
                .unwrap_or("keep");
            let current = if options.iter().any(|option| option["value"] == current) {
                current
            } else {
                options.push(json!({"value":"keep","label":"Select an effect to apply"}));
                "keep"
            };
            let mut fields = vec![choice_field("effect", "Lighting effect", current, options)];
            if let Some(color) = &color1 {
                fields.push(json!({"id":"color","label":"Primary color (colored effects)","kind":"color","value":color,"options":[]}));
            }
            if let Some(color) = &color2 {
                fields.push(json!({"id":"color2","label":"Second color (dual breathing)","kind":"color","value":color,"options":[]}));
            }
            if let Some(value) = speed {
                fields.push(number_field(
                    "speed",
                    "Reactive duration (1–4)",
                    value,
                    1.0,
                    4.0,
                ));
            }
            if let Some(value) =
                direction.filter(|_| matches!(device.product_id(), Some(0x0086 | 0x0088 | 0x007e)))
            {
                fields.push(choice_field(
                    "direction",
                    "Wave direction",
                    &value.to_string(),
                    vec![
                        json!({"value":"1","label":"Direction 1"}),
                        json!({"value":"2","label":"Direction 2"}),
                    ],
                ));
            }
            group.rows.push(Row::info("effect", "Effect & colors", "Only parameters used by the selected effect are sent. Other zones are unchanged.").controlled(form(device, "effect", &interface, fields)));
        }
        if let Some(color) = color1 {
            let setter = format!("set{prefix}Static");
            if method(device, &interface, &setter, "yyy").is_some() {
                group.rows.push(Row::info("color", "Static color", "Apply selects Static and this color for this zone.").controlled(json!({"kind":"color","value":color,"action":"openrazer-configure","args":{"device":device.serial,"operation":"color","interface":interface,"member":setter}})));
            }
        }
        if !group.rows.is_empty() {
            groups.push(("lighting", group));
        }
    }
    if method(
        device,
        "razer.device.lighting.power",
        "setChargeEffect",
        "y",
    )
    .is_some()
        && method(
            device,
            "razer.device.lighting.power",
            "setChargeColour",
            "yyy",
        )
        .is_some()
    {
        let mut charging = Group::new(
            format!("settings:{}:charging", device.serial),
            format!("{component} charging indicator"),
            "Real charging-light setters; no getter is exported for current charging color/effect",
        );
        charging.rows.push(Row::info("charge", "Charging light", "Choose explicitly: follow current mouse effect, or a supplied charging color. The empty color is a manual input, not a guessed current value; no action occurs until Apply.").controlled(form(device, "charging", "razer.device.lighting.power", vec![choice_field("mode", "Charging effect", "choose", vec![json!({"value":"choose","label":"Choose an effect"}), json!({"value":"0","label":"Use current mouse effect"}), json!({"value":"1","label":"Use custom charging color"})]), json!({"id":"color","label":"Custom charging color (#RRGGBB)","kind":"text","value":"","options":[]})])));
        groups.push(("lighting", charging));
    }
    let mut power = Group::new(
        format!("settings:{}:power", device.serial),
        format!("{component} power"),
        "Actual device power controls",
    );
    for (getter, setter, title, min, max, signature) in [
        (
            "getIdleTime",
            "setIdleTime",
            "Sleep after inactivity (seconds)",
            60.0,
            900.0,
            "q",
        ),
        (
            "getLowBatteryThreshold",
            "setLowBatteryThreshold",
            "Low battery warning (%)",
            0.0,
            100.0,
            "y",
        ),
    ] {
        if let Some(value) = device.value(POWER, getter).filter(|value| {
            value
                .as_f64()
                .is_some_and(|number| (min..=max).contains(&number))
        }) && method(device, POWER, setter, signature).is_some()
        {
            power.rows.push(
                Row::info(
                    getter,
                    title,
                    "Read from the current daemon device; native hardware range",
                )
                .controlled(numeric(device, POWER, setter, value, min, max)),
            );
        }
    }
    if !power.rows.is_empty() {
        groups.push(("power", power));
    }
    groups
}
fn effects() -> &'static [(&'static str, &'static str, &'static str, &'static str)] {
    &[
        ("None", "none", "Off", ""),
        ("Spectrum", "spectrum", "Spectrum cycling", ""),
        ("Static", "static", "Static color", "yyy"),
        (
            "BreathRandom",
            "breathRandom",
            "Breathing · random colors",
            "",
        ),
        (
            "BreathSingle",
            "breathSingle",
            "Breathing · one color",
            "yyy",
        ),
        (
            "BreathDual",
            "breathDual",
            "Breathing · two colors",
            "yyyyyy",
        ),
        ("Reactive", "reactive", "Reactive", "yyyy"),
        ("Wave", "wave", "Wave", "i"),
    ]
}
fn arguments(device: &DeviceSnapshot, args: &Json) -> Result<(String, String, Vec<Json>)> {
    let operation = string_arg(args, "operation")?;
    let value = args.get("value").context("Missing args.value")?;
    match operation {
        "dpi" => {
            let x = field(value, "x")?;
            let x = if let Some(value) = x.as_str() {
                json!(value.parse::<u64>().context("DPI choice must be numeric")?)
            } else {
                x.clone()
            };
            let y = value.get("y").cloned().unwrap_or(json!(0));
            Ok((DPI.into(), "setDPI".into(), validate_dpi(device, &x, &y)?))
        }
        "stages" => {
            let current = device
                .value(DPI, "getDPIStages")
                .and_then(Json::as_array)
                .and_then(|values| values.get(1))
                .and_then(Json::as_array)
                .context("Current DPI stages unavailable")?;
            if current.is_empty() || current.len() > 5 {
                bail!("Unsupported stage table length");
            }
            let active = integer(field(value, "active")?, 1, current.len() as u64)?;
            let pairs: Result<Vec<Json>> = (1..=current.len())
                .map(|index| {
                    Ok(Json::Array(validate_dpi(
                        device,
                        field(value, &format!("x{index}"))?,
                        field(value, &format!("y{index}"))?,
                    )?))
                })
                .collect();
            Ok((
                DPI.into(),
                "setDPIStages".into(),
                vec![json!(active), json!(pairs?)],
            ))
        }
        "polling" => {
            let rate = value
                .as_str()
                .context("Poll rate must be a choice string")?
                .parse::<u64>()?;
            let rates = poll_rates(device).context("Supported poll rates unavailable")?;
            if !rates.contains(&rate) {
                bail!("Poll rate is not supported by this device");
            }
            Ok((MISC.into(), "setPollRate".into(), vec![json!(rate)]))
        }
        "scalar" => {
            let interface = string_arg(args, "interface")?;
            let member = string_arg(args, "member")?;
            let (min, max) = if interface == POWER && member == "setIdleTime" {
                (60, 900)
            } else if (interface.starts_with("razer.device.lighting.")
                && member.ends_with("Brightness"))
                || (interface == POWER && member == "setLowBatteryThreshold")
            {
                (0, 100)
            } else {
                bail!("Unsupported product scalar control");
            };
            Ok((
                interface.into(),
                member.into(),
                vec![json!(integer(value, min, max)?)],
            ))
        }
        "color" => {
            let interface = string_arg(args, "interface")?;
            let member = string_arg(args, "member")?;
            if !interface.starts_with("razer.device.lighting.") || !member.ends_with("Static") {
                bail!("Not a static lighting control");
            }
            Ok((interface.into(), member.into(), rgb(value)?))
        }
        "effect" => {
            let interface = string_arg(args, "interface")?;
            let prefix = match interface.strip_prefix("razer.device.lighting.") {
                Some("logo") => "Logo",
                Some("scroll") => "Scroll",
                Some("left") => "Left",
                Some("right") => "Right",
                Some("backlight") => "Backlight",
                Some("chroma") => "",
                _ => bail!("Unsupported RGB zone"),
            };
            let effect = field(value, "effect")?
                .as_str()
                .context("Effect must be a choice string")?;
            let (suffix, _, _, signature) = effects()
                .iter()
                .find(|(_, name, _, _)| *name == effect)
                .context("Choose a supported effect; no unchanged/default write")?;
            let member = format!("set{prefix}{suffix}");
            method(device, interface, &member, signature)
                .context("Effect is not exported with the supported signature")?;
            let mut values = match effect {
                "static" | "breathSingle" | "reactive" | "breathDual" => {
                    rgb(field(value, "color")?)?
                }
                _ => Vec::new(),
            };
            if effect == "breathDual" {
                values.extend(rgb(field(value, "color2")?)?);
            }
            if effect == "reactive" {
                values.push(json!(integer(field(value, "speed")?, 1, 4)?));
            }
            if effect == "wave" {
                if !matches!(device.product_id(), Some(0x0086 | 0x0088 | 0x007e)) {
                    bail!("Wave directions for this model have not been established");
                }
                let direction = field(value, "direction")?
                    .as_str()
                    .context("Direction must be a choice string")?
                    .parse::<u64>()?;
                values.push(json!(integer(&json!(direction), 1, 2)?));
            }
            Ok((interface.into(), member, values))
        }
        _ => bail!("Unknown product control operation"),
    }
}
pub async fn configure(args: &Json) -> Result<Group> {
    let serial = string_arg(args, "device")?;
    let connection = connection().await?;
    if !serials(&connection)
        .await?
        .iter()
        .any(|current| current == serial)
    {
        bail!("OpenRazer device removed or unknown");
    }
    let device = device_snapshot(&connection, serial).await?;
    if args.get("operation").and_then(Json::as_str) == Some("charging") {
        let value = args.get("value").context("Missing form args.value")?;
        let mode = field(value, "mode")?
            .as_str()
            .context("Charging mode must be a choice string")?
            .parse::<u64>()
            .context("Choose a charging effect")?;
        if mode > 1 {
            bail!("Charging effect must be 0 or 1");
        }
        let interface = "razer.device.lighting.power";
        let effect = method(&device, interface, "setChargeEffect", "y")
            .context("Charging effect no longer supported")?;
        let color = if mode == 1 {
            Some((
                method(&device, interface, "setChargeColour", "yyy")
                    .context("Charging color no longer supported")?,
                rgb(field(value, "color")?)?,
            ))
        } else {
            None
        };
        if let Some((method, color)) = color {
            call(&connection, &path(serial)?, method, &color).await?;
        }
        call(&connection, &path(serial)?, effect, &[json!(mode)]).await?;
        let mut group = Group::new(
            "openrazer-result",
            "Charging indicator applied",
            "Explicit real OpenRazer charging-light calls",
        );
        group.rows.push(Row::info("result", "Charging-light configuration accepted", "Current charging effect/color cannot be independently read back: this device exports setters only."));
        return Ok(group);
    }
    let (interface, member, values) = arguments(&device, args)?;
    let method = device
        .methods
        .iter()
        .find(|method| method.interface == interface && method.name == member)
        .context("Control no longer exported by this connected device")?;
    if method.risky() {
        bail!("Product controls cannot invoke risky expert operations");
    }
    let result = call(&connection, &path(serial)?, method, &values).await?;
    let mut group = Group::new(
        "openrazer-result",
        "Configuration applied",
        "Actual OpenRazer call accepted; refreshed state follows",
    );
    group.rows.push(Row::info(
        "result",
        method.id(),
        format!("Return: {result}. Daemon acceptance is not independent hardware readback."),
    ));
    Ok(group)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn device() -> DeviceSnapshot {
        DeviceSnapshot {
            serial: "real-identity".into(),
            methods: Vec::new(),
            values: BTreeMap::from([
                ("razer.device.dpi.maxDPI".into(), Ok(json!(20000))),
                (
                    "razer.device.misc.getSupportedPollRates".into(),
                    Ok(json!([125, 500, 1000])),
                ),
            ]),
            diagnostics: Group::new("api", "API", ""),
        }
    }
    #[test]
    fn typed_forms_reject_invalid_domains_before_call() {
        let device = device();
        assert_eq!(
            arguments(
                &device,
                &json!({"operation":"dpi","value":{"x":800.0,"y":1600}})
            )
            .unwrap()
            .2,
            vec![json!(800), json!(1600)]
        );
        for value in [
            json!({"x":20001,"y":800}),
            json!({"x":800.5,"y":800}),
            json!({"x":800,"y":0}),
            json!("[800,800]"),
        ] {
            assert!(arguments(&device, &json!({"operation":"dpi","value":value})).is_err());
        }
        assert!(arguments(&device, &json!({"operation":"polling","value":"2000"})).is_err());
        assert_eq!(
            rgb(&json!("#aB02fF")).unwrap(),
            vec![json!(171), json!(2), json!(255)]
        );
        for value in [
            json!("#123"),
            json!("#GG0000"),
            json!([0, 0, 0]),
            json!("#00000000"),
        ] {
            assert!(rgb(&value).is_err());
        }
        assert!(arguments(&device, &json!({"operation":"scalar","interface":"razer.device.power","member":"setLowBatteryThreshold","value":101})).is_err());
        assert!(arguments(&device, &json!({"operation":"scalar","interface":"razer.device.power","member":"setIdleTime","value":59})).is_err());
        assert!(arguments(&device, &json!({"operation":"scalar","interface":"razer.device.power","member":"setIdleTime","value":901})).is_err());
        assert!(
            arguments(
                &device,
                &json!({"operation":"dpi","value":{"x":99,"y":800}})
            )
            .is_err()
        );
    }
    fn polling_device(pid: u16) -> DeviceSnapshot {
        DeviceSnapshot {
            serial: "stable-serial".into(),
            methods: parse_methods("<node><interface name='razer.device.misc'><method name='getPollRate'><arg type='i' direction='out'/></method><method name='setPollRate'><arg type='q' direction='in'/></method></interface></node>").unwrap(),
            values: BTreeMap::from([
                ("razer.device.misc.getVidPid".into(), Ok(json!([0x1532,pid]))),
                ("razer.device.misc.getPollRate".into(), Ok(json!(500))),
            ]), diagnostics: Group::new("api", "API", ""),
        }
    }
    #[test]
    fn basilisk_polling_defaults_are_model_and_capability_specific() {
        for pid in [0x0086, 0x0088] {
            let device = polling_device(pid);
            assert_eq!(poll_rates(&device), Some(vec![125, 500, 1000]));
            assert_eq!(
                arguments(&device, &json!({"operation":"polling","value":"125"}))
                    .unwrap()
                    .2,
                vec![json!(125)]
            );
            assert!(arguments(&device, &json!({"operation":"polling","value":"2000"})).is_err());
            let groups = product_groups(&device, "Mouse");
            let control = groups[0]
                .1
                .rows
                .iter()
                .find(|row| row.id == "polling")
                .unwrap()
                .control
                .as_ref()
                .unwrap();
            assert_eq!(
                control["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|option| option["value"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec!["125", "500", "1000"]
            );
        }
        for pid in [0x007e, 0x0a3f, 0xffff] {
            assert!(poll_rates(&polling_device(pid)).is_none());
        }
        let mut device = polling_device(0x0088);
        device.methods.retain(|method| method.name != "getPollRate");
        assert!(poll_rates(&device).is_none());
        device = polling_device(0x0088);
        device.methods.retain(|method| method.name != "setPollRate");
        assert!(poll_rates(&device).is_none());
        device = polling_device(0x0088);
        device.values.insert(
            "razer.device.misc.getVidPid".into(),
            Ok(json!([0x0000, 0x0088])),
        );
        assert!(poll_rates(&device).is_none());
    }
    #[test]
    fn advertised_poll_rates_take_precedence_even_when_unavailable() {
        let mut device = polling_device(0x0088);
        device.methods.extend(parse_methods("<node><interface name='razer.device.misc'><method name='getSupportedPollRates'><arg type='ai' direction='out'/></method></interface></node>").unwrap());
        device.values.insert(
            "razer.device.misc.getSupportedPollRates".into(),
            Ok(json!([500, 2000])),
        );
        assert_eq!(poll_rates(&device), Some(vec![500, 2000]));
        assert!(arguments(&device, &json!({"operation":"polling","value":"125"})).is_err());
        assert_eq!(
            arguments(&device, &json!({"operation":"polling","value":"2000"}))
                .unwrap()
                .2,
            vec![json!(2000)]
        );
        for value in [
            Ok(json!([])),
            Ok(json!([500, 65536])),
            Err("getter unavailable".into()),
        ] {
            device
                .values
                .insert("razer.device.misc.getSupportedPollRates".into(), value);
            assert!(poll_rates(&device).is_none());
        }
    }
}
