use crate::{Group, Row, string_arg};
#[path = "openrazer_controls.rs"]
mod controls;
use anyhow::{Context, Result, bail};
pub use controls::{configure, dpi_presets, product_groups};
use futures_util::{StreamExt, stream};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::time::Duration;
use zbus::{
    Connection,
    zvariant::{Array, Dict, Signature, Structure, StructureBuilder, Value},
};

const BUS: &str = "org.razer";
#[derive(Clone, Debug)]
struct Argument {
    name: String,
    signature: String,
}
#[derive(Clone, Debug)]
pub(crate) struct Method {
    pub(crate) interface: String,
    pub(crate) name: String,
    input: Vec<Argument>,
    output: Vec<Argument>,
}
impl Method {
    fn id(&self) -> String {
        format!("{}.{}", self.interface, self.name)
    }
    fn reads(&self) -> bool {
        !self.output.is_empty()
            && ["get", "is", "has", "available", "max", "version"]
                .iter()
                .any(|prefix| self.name.starts_with(prefix))
    }
    fn risky(&self) -> bool {
        if self.reads() {
            return false;
        }
        // Only ordinary device controls avoid the extra typed confirmation.
        // Pairing, reset, macros, game mode/key suppression and unknown operations do not.
        !(self.interface.starts_with("razer.device.lighting.")
            || self.interface == "razer.device.dpi"
            || matches!(
                self.name.as_str(),
                "setPollRate" | "setIdleTime" | "setLowBatteryThreshold"
            ))
    }
    fn arguments(&self) -> String {
        self.input
            .iter()
            .map(|arg| format!("{}:{}", arg.name, arg.signature))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub struct DeviceSnapshot {
    pub serial: String,
    methods: Vec<Method>,
    values: BTreeMap<String, Result<Json, String>>,
    pub diagnostics: Group,
}

impl DeviceSnapshot {
    pub fn value(&self, interface: &str, member: &str) -> Option<&Json> {
        self.values
            .get(&format!("{interface}.{member}"))?
            .as_ref()
            .ok()
    }

    pub fn product_id(&self) -> Option<u16> {
        let ids = self.value("razer.device.misc", "getVidPid")?.as_array()?;
        if ids.first()?.as_u64()? != 0x1532 {
            return None;
        }
        u16::try_from(ids.get(1)?.as_u64()?).ok()
    }

    pub fn battery(&self) -> Option<crate::Battery> {
        let percentage = self.value("razer.device.power", "getBattery")?.as_f64()?;
        if !percentage.is_finite() || !(0.0..=100.0).contains(&percentage) {
            return None;
        }
        Some(crate::Battery {
            percentage: Some(percentage),
            charging: self
                .value("razer.device.power", "isCharging")
                .and_then(Json::as_bool),
            source: "OpenRazer getBattery / isCharging".into(),
        })
    }
}

pub struct Snapshot {
    pub devices: Vec<DeviceSnapshot>,
    pub status: Option<Group>,
}
fn parse_methods(xml: &str) -> Result<Vec<Method>> {
    let document = roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
    .context("Parsing OpenRazer introspection")?;
    let mut methods = Vec::new();
    for interface in document
        .descendants()
        .filter(|node| node.has_tag_name("interface"))
    {
        let Some(name) = interface.attribute("name") else {
            continue;
        };
        if !name.starts_with("razer.device.") {
            continue;
        }
        for method in interface
            .children()
            .filter(|node| node.has_tag_name("method"))
        {
            let mut item = Method {
                interface: name.into(),
                name: method
                    .attribute("name")
                    .context("Unnamed D-Bus method")?
                    .into(),
                input: Vec::new(),
                output: Vec::new(),
            };
            for (index, argument) in method
                .children()
                .filter(|node| node.has_tag_name("arg"))
                .enumerate()
            {
                let argument_value = Argument {
                    name: argument
                        .attribute("name")
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("arg{index}")),
                    signature: argument
                        .attribute("type")
                        .context("Missing argument signature")?
                        .into(),
                };
                if argument.attribute("direction") == Some("out") {
                    item.output.push(argument_value);
                } else {
                    item.input.push(argument_value);
                }
            }
            methods.push(item);
        }
    }
    methods.sort_by_key(Method::id);
    Ok(methods)
}
async fn introspect(connection: &Connection, path: &str) -> Result<Vec<Method>> {
    let message = tokio::time::timeout(
        Duration::from_secs(2),
        connection.call_method(
            Some(BUS),
            path,
            Some("org.freedesktop.DBus.Introspectable"),
            "Introspect",
            &(),
        ),
    )
    .await
    .context("Introspection timeout")??;
    parse_methods(&message.body().deserialize::<String>()?)
}
async fn connection() -> Result<Connection> {
    tokio::time::timeout(Duration::from_secs(2), Connection::session())
        .await
        .context("Session bus connection timed out")?
        .context("Connecting to user session bus")
}
async fn serials(connection: &Connection) -> Result<Vec<String>> {
    let reply = tokio::time::timeout(
        Duration::from_secs(2),
        connection.call_method(
            Some(BUS),
            "/org/razer",
            Some("razer.devices"),
            "getDevices",
            &(),
        ),
    )
    .await
    .context("OpenRazer discovery timeout")??;
    Ok(reply.body().deserialize()?)
}
fn path(serial: &str) -> Result<String> {
    let path = format!("/org/razer/device/{serial}");
    zbus::zvariant::ObjectPath::try_from(path.as_str())
        .context("Invalid daemon device object path")?;
    Ok(path)
}
async fn call(
    connection: &Connection,
    path: &str,
    method: &Method,
    arguments: &[Json],
) -> Result<Json> {
    if arguments.len() != method.input.len() {
        bail!(
            "{} expects {} arguments ({})",
            method.id(),
            method.input.len(),
            method.arguments()
        );
    }
    let reply = if arguments.is_empty() {
        tokio::time::timeout(
            Duration::from_secs(2),
            connection.call_method(
                Some(BUS),
                path,
                Some(method.interface.as_str()),
                method.name.as_str(),
                &(),
            ),
        )
        .await
        .context("D-Bus method timeout")??
    } else {
        let mut builder = StructureBuilder::new();
        for (argument, value) in method.input.iter().zip(arguments) {
            builder.push_value(
                typed_value(&argument.signature, value)
                    .with_context(|| format!("{} argument {}", method.id(), argument.name))?,
            );
        }
        let body = builder.build()?;
        tokio::time::timeout(
            Duration::from_secs(2),
            connection.call_method(
                Some(BUS),
                path,
                Some(method.interface.as_str()),
                method.name.as_str(),
                &body,
            ),
        )
        .await
        .context("D-Bus method timeout")??
    };
    if reply.body().is_empty() {
        return Ok(Json::Null);
    }
    let body = reply.body();
    let structure: Structure<'_> = body.deserialize()?;
    let fields: Vec<Json> = structure.fields().iter().map(to_json).collect();
    Ok(if fields.len() == 1 {
        fields.into_iter().next().unwrap()
    } else {
        Json::Array(fields)
    })
}
pub fn to_json(value: &Value<'_>) -> Json {
    match value {
        Value::U8(value) => json!(value),
        Value::Bool(value) => json!(value),
        Value::I16(value) => json!(value),
        Value::U16(value) => json!(value),
        Value::I32(value) => json!(value),
        Value::U32(value) => json!(value),
        Value::I64(value) => json!(value),
        Value::U64(value) => json!(value),
        Value::F64(value) => json!(value),
        Value::Str(value) => json!(value.as_str()),
        Value::ObjectPath(value) => json!(value.as_str()),
        Value::Signature(value) => json!(value.to_string()),
        Value::Value(value) => to_json(value),
        Value::Array(value) => Json::Array(value.inner().iter().map(to_json).collect()),
        Value::Structure(value) => Json::Array(value.fields().iter().map(to_json).collect()),
        Value::Dict(value) => Json::Array(
            value
                .iter()
                .map(|(key, value)| json!([to_json(key), to_json(value)]))
                .collect(),
        ),
        _ => json!(format!("Unsupported returned D-Bus value: {value:?}")),
    }
}

// Parse one complete D-Bus type at a time, including empty typed arrays and
// nested structures. This is required by actual setDPIStages/custom-frame APIs.
fn type_end(signature: &[u8], start: usize) -> Result<usize> {
    let byte = *signature.get(start).context("Incomplete D-Bus signature")?;
    match byte {
        b'a' => type_end(signature, start + 1),
        b'(' | b'{' => {
            let closer = if byte == b'(' { b')' } else { b'}' };
            let mut index = start + 1;
            while signature.get(index) != Some(&closer) {
                index = type_end(signature, index)?;
            }
            Ok(index + 1)
        }
        b'y' | b'b' | b'n' | b'q' | b'i' | b'u' | b'x' | b't' | b'd' | b's' | b'o' | b'g'
        | b'v' => Ok(start + 1),
        _ => bail!(
            "Unsupported D-Bus type {} (file descriptors are intentionally not accepted)",
            byte as char
        ),
    }
}
fn signatures(signature: &str) -> Result<Vec<&str>> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < signature.len() {
        let end = type_end(signature.as_bytes(), index)?;
        result.push(&signature[index..end]);
        index = end;
    }
    Ok(result)
}
fn signed_integer(value: &Json) -> Result<i64> {
    if let Some(value) = value.as_i64() {
        return Ok(value);
    }
    let number = value
        .as_f64()
        .filter(|number| {
            number.is_finite()
                && number.fract() == 0.0
                && *number >= i64::MIN as f64
                && *number < (i64::MAX as f64)
        })
        .context("Expected in-range signed integer (fractional numbers rejected)")?;
    Ok(number as i64)
}
fn unsigned_integer(value: &Json) -> Result<u64> {
    if let Some(value) = value.as_u64() {
        return Ok(value);
    }
    let number = value
        .as_f64()
        .filter(|number| {
            number.is_finite()
                && number.fract() == 0.0
                && *number >= 0.0
                && *number < (u64::MAX as f64)
        })
        .context("Expected in-range unsigned integer (fractional numbers rejected)")?;
    Ok(number as u64)
}
fn signature_type(value: &str) -> Result<Signature> {
    Signature::try_from(value)
        .map_err(|error| anyhow::anyhow!("Invalid D-Bus signature {value:?}: {error}"))
}

fn typed_value(signature: &str, value: &Json) -> Result<Value<'static>> {
    if type_end(signature.as_bytes(), 0)? != signature.len() {
        bail!("Expected one complete argument type");
    }
    macro_rules! signed {
        ($variant:ident, $ty:ty) => {
            Value::$variant(
                <$ty>::try_from(signed_integer(value)?).context("Integer out of range")?,
            )
        };
    }
    macro_rules! unsigned {
        ($variant:ident, $ty:ty) => {
            Value::$variant(
                <$ty>::try_from(unsigned_integer(value)?).context("Integer out of range")?,
            )
        };
    }
    Ok(match signature {
        "y" => unsigned!(U8, u8),
        "n" => signed!(I16, i16),
        "q" => unsigned!(U16, u16),
        "i" => signed!(I32, i32),
        "u" => unsigned!(U32, u32),
        "x" => signed!(I64, i64),
        "t" => unsigned!(U64, u64),
        "b" => Value::Bool(value.as_bool().context("Expected boolean")?),
        "d" => Value::F64(
            value
                .as_f64()
                .filter(|v| v.is_finite())
                .context("Expected finite number")?,
        ),
        "s" => Value::Str(value.as_str().context("Expected string")?.to_owned().into()),
        "o" => Value::ObjectPath(zbus::zvariant::ObjectPath::try_from(
            value
                .as_str()
                .context("Expected object path string")?
                .to_owned(),
        )?),
        "g" => Value::Signature(signature_type(
            value.as_str().context("Expected signature string")?,
        )?),
        "v" => {
            let signature = value
                .get("signature")
                .and_then(Json::as_str)
                .context("Variant requires {signature, value}")?;
            Value::Value(Box::new(typed_value(
                signature,
                value.get("value").context("Variant missing value")?,
            )?))
        }
        _ if signature.starts_with("a{") => {
            let types = signatures(&signature[2..signature.len() - 1])?;
            if types.len() != 2 {
                bail!("Dictionary requires key and value types");
            }
            let mut dict = Dict::new(&signature_type(types[0])?, &signature_type(types[1])?);
            for pair in value
                .as_array()
                .context("Dictionary must be JSON array of [key, value] pairs")?
            {
                let pair = pair
                    .as_array()
                    .filter(|pair| pair.len() == 2)
                    .context("Dictionary entry must be [key, value]")?;
                dict.append(
                    typed_value(types[0], &pair[0])?,
                    typed_value(types[1], &pair[1])?,
                )?;
            }
            Value::Dict(dict)
        }
        _ if signature.starts_with('a') => {
            let element = &signature[1..];
            let mut array = Array::new(&signature_type(element)?);
            for value in value.as_array().context("Expected JSON array")? {
                array.append(typed_value(element, value)?)?;
            }
            Value::Array(array)
        }
        _ if signature.starts_with('(') => {
            let types = signatures(&signature[1..signature.len() - 1])?;
            let values = value
                .as_array()
                .filter(|values| values.len() == types.len())
                .context("Structure requires JSON array matching fields")?;
            let mut structure = StructureBuilder::new();
            for (signature, value) in types.iter().zip(values) {
                structure.push_value(typed_value(signature, value)?);
            }
            Value::Structure(structure.build()?)
        }
        _ => bail!("Unsupported D-Bus signature {signature}"),
    })
}
fn available_signature(method: &Method) -> Result<()> {
    for argument in &method.input {
        let _ = signature_type(argument.signature.as_str())?;
        type_end(argument.signature.as_bytes(), 0)?;
    }
    Ok(())
}
fn native_control(
    serial: &str,
    method: &Method,
    values: &BTreeMap<String, Result<Json, String>>,
) -> Option<Json> {
    if method.input.len() != 1 || method.risky() || !method.name.starts_with("set") {
        return None;
    }
    let getter = format!("{}.get{}", method.interface, &method.name[3..]);
    let current = values.get(&getter)?.as_ref().ok()?;
    let args =
        json!({"device": serial,"interface":method.interface,"member":method.name,"mode":"scalar"});
    let signature = method.input[0].signature.as_str();
    if signature == "b" && current.is_boolean() {
        return Some(
            json!({"kind":"switch","value":current,"action":"openrazer-call","args":args}),
        );
    }
    if method.name == "setPollRate" {
        let supported = values
            .get("razer.device.misc.getSupportedPollRates")?
            .as_ref()
            .ok()?
            .as_array()?;
        let options: Vec<Json> = supported
            .iter()
            .filter_map(|value| value.as_u64())
            .map(|value| json!({"value":value.to_string(),"label":format!("{value} Hz")}))
            .collect();
        if options.is_empty() {
            return None;
        }
        let mut args = args;
        args["mode"] = json!("choice-number");
        return Some(
            json!({"kind":"choice","value":current.to_string(),"options":options,"action":"openrazer-call","args":args}),
        );
    }
    let (min, max, step) =
        if method.name.contains("Brightness") || method.name == "setLowBatteryThreshold" {
            (0, 100, 1)
        } else {
            return None;
        };
    if current.is_number() {
        Some(
            json!({"kind":"number","value":current,"min":min,"max":max,"step":step,"action":"openrazer-call","args":args}),
        )
    } else {
        None
    }
}
async fn device_snapshot(connection: &Connection, serial: &str) -> Result<DeviceSnapshot> {
    let path = path(serial)?;
    let methods = introspect(connection, &path).await?;
    let reads = stream::iter(
        methods
            .iter()
            .filter(|method| method.reads() && method.input.is_empty())
            .map(|method| {
                let path = &path;
                async move {
                    (
                        method.id(),
                        call(connection, path, method, &[])
                            .await
                            .map_err(|error| format!("{error:#}")),
                    )
                }
            }),
    )
    .buffer_unordered(16);
    let values: BTreeMap<String, Result<Json, String>> = reads.collect().await;
    let name = values
        .get("razer.device.misc.getDeviceName")
        .and_then(|result| result.as_ref().ok())
        .and_then(Json::as_str)
        .unwrap_or(serial);
    let mut group = Group::new(
        format!("openrazer:{serial}"),
        format!("OpenRazer: {name}"),
        format!(
            "Daemon serial {serial}; {} exported methods discovered live. Getter results may be daemon/persistence state, not hardware readback. Unsupported methods are never invented.",
            methods.len()
        ),
    );
    for method in &methods {
        let id = method.id();
        if let Some(result) = values.get(&id) {
            group.rows.push(Row::info(&id, &method.name, match result {
                Ok(value) if method.name == "getBattery" && value.as_f64().is_some_and(|value| value < 0.0) => "Battery unavailable (OpenRazer reports the unknown -1 sentinel), not negative charge".into(),
                Ok(value) => format!("{} = {value}", method.interface),
                Err(error) => format!("Unavailable: {error}"),
            }));
            continue;
        }
        let signature = method.arguments();
        let subtitle = format!(
            "{}({signature}) → {}. {}",
            method.interface,
            method
                .output
                .iter()
                .map(|arg| arg.signature.as_str())
                .collect::<Vec<_>>()
                .join(","),
            if method.risky() {
                "May change input/configuration: submit {\"arguments\":[...],\"confirm\":true} only after reviewing upstream method semantics."
            } else if method.reads() {
                "Explicit query requiring arguments; enter a JSON argument array."
            } else {
                "Explicit write only. Enter a JSON argument array; no default values or writes on refresh."
            }
        );
        let mut row = Row::info(&id, &method.name, subtitle);
        match available_signature(method) {
            Err(error) => {
                row.subtitle
                    .push_str(&format!(" Unavailable signature: {error:#}"));
            }
            Ok(()) => {
                if let Some(control) = native_control(serial, method, &values) {
                    row.control = Some(control);
                } else if method.input.is_empty() && !method.risky() {
                    row.control = Some(
                        json!({"kind":"button","label":method.name,"action":"openrazer-call","args":{"device":serial,"interface":method.interface,"member":method.name,"arguments":[]},"destructive":false}),
                    );
                } else {
                    row.control = Some(
                        json!({"kind":"text","value":"","action":"openrazer-call","args":{"device":serial,"interface":method.interface,"member":method.name,"mode":"json"}}),
                    );
                }
            }
        }
        group.rows.push(row);
    }
    if methods.is_empty() {
        group.rows.push(Row::info("empty", "No exported device controls", "Daemon object exists but exports no razer.device.* capabilities; check driver/daemon compatibility."));
    }
    Ok(DeviceSnapshot {
        serial: serial.into(),
        methods,
        values,
        diagnostics: group.collapsed(),
    })
}
async fn collect() -> Result<Snapshot> {
    let connection = connection().await?;
    let names = zbus::fdo::DBusProxy::new(&connection).await?;
    if !names.name_has_owner(BUS.try_into()?).await? {
        bail!(
            "OpenRazer daemon is not running; start it after completing device permissions setup"
        );
    }
    let serials = serials(&connection)
        .await
        .context("org.razer discovery; OpenRazer daemon may not be running")?;
    let queries = stream::iter(serials.iter().map(|serial| {
        let connection = &connection;
        async move { device_snapshot(connection, serial).await }
    }))
    .buffer_unordered(4);
    let mut devices = Vec::new();
    let mut issues = Group::new(
        "openrazer",
        "OpenRazer status",
        "User-session device capability service",
    );
    for result in queries.collect::<Vec<_>>().await {
        match result {
            Ok(device) => devices.push(device),
            Err(error) => issues.rows.push(Row::info(
                format!("error-{}", issues.rows.len()),
                "Device query unavailable",
                format!("{error:#}"),
            )),
        }
    }
    devices.sort_by(|a, b| a.serial.cmp(&b.serial));
    if devices.is_empty() && issues.rows.is_empty() {
        issues.rows.push(Row::info("none", "No supported daemon devices", "Daemon is running, but no supported peripheral is exposed. Check modules and OpenRazer device permissions. Wolverine configuration is not supplied by OpenRazer."));
    }
    Ok(Snapshot {
        devices,
        status: (!issues.rows.is_empty()).then_some(issues),
    })
}
pub async fn state() -> Snapshot {
    match tokio::time::timeout(Duration::from_secs(3), collect())
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("OpenRazer state timed out after 3 seconds")))
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let mut group = Group::new(
                "openrazer",
                "Mouse configuration setup",
                "Native USB and controller input remain available",
            );
            group.rows.push(Row::info("daemon", "OpenRazer unavailable", format!("{error:#}. Install matching OpenRazer kernel modules and openrazer-daemon, follow its device permissions, then run the daemon in your desktop session. No automatic installation or hardware writes.")).controlled(json!({"kind":"button","label":"Open setup instructions","action":"openrazer-setup","args":{},"destructive":false})));
            Snapshot {
                devices: Vec::new(),
                status: Some(group),
            }
        }
    }
}

pub async fn setup() -> Result<()> {
    let connection = connection().await?;
    let options: BTreeMap<&str, zbus::zvariant::Value<'_>> = BTreeMap::new();
    let reply = tokio::time::timeout(
        Duration::from_secs(3),
        connection.call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.OpenURI"),
            "OpenURI",
            &("", "https://openrazer.github.io/#download", options),
        ),
    )
    .await
    .context("Desktop portal setup request timed out")??;
    let _: zbus::zvariant::OwnedObjectPath = reply.body().deserialize()?;
    Ok(())
}
pub async fn action(args: &Json) -> Result<Group> {
    let serial = string_arg(args, "device")?;
    let connection = connection().await?;
    if !serials(&connection)
        .await?
        .iter()
        .any(|current| current == serial)
    {
        bail!("OpenRazer device removed or unknown serial {serial:?}");
    }
    let path = path(serial)?;
    let interface = string_arg(args, "interface")?;
    let member = string_arg(args, "member")?;
    let methods = introspect(&connection, &path).await?;
    let method = methods
        .iter()
        .find(|method| method.interface == interface && method.name == member)
        .context("Method is not exported by this connected OpenRazer device")?;
    available_signature(method)?;
    let mut confirmed = args.get("confirm").and_then(Json::as_bool).unwrap_or(false);
    let arguments = match args.get("mode").and_then(Json::as_str) {
        Some("scalar") => vec![args.get("value").context("Missing control value")?.clone()],
        Some("choice-number") => vec![json!(
            string_arg(args, "value")?
                .parse::<u64>()
                .context("Choice must be a numeric poll rate")?
        )],
        Some("json") => {
            let value: Json = serde_json::from_str(string_arg(args, "value")?).context(
                "Enter a JSON argument array, or {arguments:[...],confirm:true} for risky methods",
            )?;
            if value.is_object() {
                confirmed = value.get("confirm").and_then(Json::as_bool) == Some(true);
                value
                    .get("arguments")
                    .and_then(Json::as_array)
                    .context("Missing arguments array")?
                    .clone()
            } else {
                value
                    .as_array()
                    .context("Arguments must be a JSON array")?
                    .clone()
            }
        }
        None => args
            .get("arguments")
            .and_then(Json::as_array)
            .context("Missing arguments array")?
            .clone(),
        _ => bail!("Unknown control value mode"),
    };
    if method.risky() && !confirmed {
        bail!(
            "{} may modify input/configuration; explicit confirm:true is required",
            method.id()
        );
    }
    let result = call(&connection, &path, method, &arguments).await?;
    let mut group = Group::new(
        "openrazer-result",
        "OpenRazer operation completed",
        "Real introspected D-Bus call; device state refreshed after success",
    );
    group.rows.push(Row::info("result", method.id(), format!("Return: {result}. A successful setter is daemon acceptance, not independent hardware readback.")));
    Ok(group)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compound_values_and_numeric_boundaries() {
        let value = typed_value("a(qq)", &json!([[800, 800], [1600, 1600]])).unwrap();
        assert_eq!(to_json(&value), json!([[800, 800], [1600, 1600]]));
        assert!(typed_value("y", &json!(256)).is_err());
        assert!(typed_value("q", &json!(-1)).is_err());
        assert!(typed_value("b", &json!(1)).is_err());
        assert!(typed_value("a(qq)", &json!([[800]])).is_err());
        assert!(typed_value("h", &json!(3)).is_err());
        assert_eq!(to_json(&typed_value("y", &json!(10.0)).unwrap()), json!(10));
        assert!(typed_value("y", &json!(10.5)).is_err());
    }
    #[test]
    fn introspection_controls_only_exported_device_methods() {
        let methods = parse_methods("<node><interface name='razer.device.dpi'><method name='setDPI'><arg name='x' type='q' direction='in'/><arg name='y' type='q' direction='in'/></method></interface><interface name='org.freedesktop.DBus.Introspectable'><method name='Introspect'/></interface></node>").unwrap();
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].input.len(), 2);
        assert_eq!(methods[0].input[0].name, "x");
        assert_eq!(methods[0].input[0].signature, "q");
        let mut reset = methods[0].clone();
        reset.interface = "razer.device.misc".into();
        reset.name = "reset".into();
        assert!(reset.risky());
    }
}
