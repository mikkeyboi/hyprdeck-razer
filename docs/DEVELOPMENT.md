# Development

## Build and local installation

Linux x86-64 and a current stable Rust toolchain are required. The binary does not link GTK, OpenRazer’s Python client, libusb or system OpenSSL. OpenRazer and fwupd are optional runtime services.

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release

mkdir -p dist/local
install -m 0755 target/release/hyprdeck-razer dist/local/hyprdeck-razer
install -m 0644 plugin.json dist/local/plugin.json
hyprdeck plugins install ./dist/local
hyprdeck plugins enable razer
```

Use a Hyprdeck build supporting product pages, Color/Form controls and the nullable-current `Dpi` editor before installing plugin 0.4.0. API version 1 alone does not imply support for every control kind.

Installation does not execute or enable a backend. Enabling explicitly trusts user-code. The host installs the manifest and executable under `$XDG_DATA_HOME/hyprdeck/plugins/razer/` and records activation in `$XDG_CONFIG_HOME/hyprdeck/plugins.toml`.

## CLI and subprocess API

```sh
hyprdeck-razer state
hyprdeck-razer measure INPUT_ID_FROM_STATE 4
hyprdeck-razer firmware-check USB_ID_FROM_STATE
printf '%s' '{"api_version":1,"method":"state","action":null,"args":{}}' | hyprdeck-razer --request
```

`--request` reads one JSON document from stdin, returns one response on stdout and writes diagnostics only to stderr. Responses contain `api_version`, `error` and `state`; errors have null state. Actions return refreshed product state. The backend deadline is 13 seconds; the host deadline is 15 seconds. Measurements accept 1–8 seconds. Firmware flashing never runs through this supervisor.

The [host protocol guide](https://github.com/mikkeyboi/hyprdeck/blob/main/docs/PLUGINS.md) defines products, groups, controller visualization and native controls. Ordinary settings use typed `args.value`; raw positional D-Bus calls are confined to expert Diagnostics. Device capabilities, identity and domains are revalidated before each write.

## Desktop DPI presets

Use the actual daemon serial from state:

```sh
hyprdeck-razer action dpi-presets '{"device":"SERIAL","value":{"operation":"apply","x":800,"y":800,"linked":true}}'
hyprdeck-razer action dpi-presets '{"device":"SERIAL","value":{"operation":"save","linked":true,"stages":[{"id":"stage-1","label":"400 DPI","x":400,"y":400},{"id":"stage-2","label":"800 DPI","x":800,"y":800}]}}'
hyprdeck-razer action dpi-presets '{"device":"SERIAL","value":{"operation":"activate","stage":"stage-2"}}'
hyprdeck-razer action dpi-presets '{"device":"SERIAL","value":{"operation":"next"}}'
hyprdeck-razer action dpi-presets '{"device":"SERIAL","value":{"operation":"previous"}}'
```

- **Save** changes host configuration only. Apply/activate/cycle call the real `setDPI`.
- Values are bounded integers, 100 through the device’s actual maximum. Linked pairs must be equal. The 50-DPI UI increment is not a protocol restriction.
- There are 1–8 named stages with unique safe ids. Defaults are in-memory 400/800/1600/3200 samples; state refresh writes nothing.
- Active stage is derived from current X/Y readback. Unknown or unmatched current DPI selects the first saved stage when cycling.
- Null X/Y means unavailable readback, not an invented current sensitivity. The editor distinguishes pending manual values from device state.
- Applying current DPI does not discard pending preset edits. Save explicitly persists the preset table and link preference.

Configuration: `$XDG_CONFIG_HOME/hyprdeck-razer/dpi.json`, defaulting to `~/.config/hyprdeck-razer/dpi.json`:

```json
{"version":1,"devices":{"SERIAL":{"product_id":136,"linked":true,"stages":[{"id":"stage-1","label":"800 DPI","x":800,"y":800}]}}}
```

Identity is guarded by daemon serial and model PID. Basilisk wired `0086` and receiver `0088` may share preferences only with the same serial. Writes use locking and atomic mode-0600 replacements. Malformed/version-invalid configuration is reported and never silently overwritten.

## Device capability boundaries

OpenRazer’s Basilisk class exports `getDPI`/`setDPI`, battery, lighting and power methods, but no onboard stage, mapping or macro API. A supported-polling getter takes precedence; without it, exact Basilisk `0086`/`0088` devices with a readable getter and real setter use OpenRazer’s documented 125/500/1000-Hz defaults. Unknown models receive no guessed fallback.

Mouse logo, wheel and side strips are independent lighting zones; dock brightness/effects use separate interfaces. Charging-light setters lack readback and therefore require explicit manual input. Battery uses `getBattery`/`isCharging`; a connected dock does not establish charging.

Input Remapper integration launches its real editor and applies/stops existing device-specific v2 presets through a current physical event path. It requires installation, service authorization and actual preset creation; command success is not claimed as proof of injection. It is software remapping, not onboard configuration.

Controller inspection uses nonexclusive read-only evdev ioctls. Snapshots can miss brief presses; changed-frame cadence does not measure every USB packet or internal sensor poll. Independent rear-button identity is unavailable under ordinary xpad. See [controller Linux support](controller-linux-support.md) for naming, RGB, battery and headphone-audio findings.

## Firmware and releases

Original-model updater discovery validates the official Razer HTTPS redirect and model-specific filename. It never runs a download or compares `bcdDevice` to installed firmware. Wolverine updating remains a manual Windows/vendor process.

For another device genuinely enumerated as supported/updatable by fwupd, the standalone `firmware-update DEVICE_ID --confirm` command verifies vendor identity and upgrades before invoking device-specific `fwupdmgr`. Historical, inhibited, wildcard, unreachable and emulated targets are rejected. Do not interrupt a flash or invoke it through the host request deadline.

CI runs formatting, Clippy, tests, a release build and headless protocol checks. Immutable releases use Ubuntu 22.04 x86-64 and contain:

- `plugin.json`
- `hyprdeck-razer-linux-x86_64`
- `hyprdeck-razer-linux-x86_64.sha256`

Keep Cargo.toml, Cargo.lock’s root package and plugin.json versions identical; publish the compatible host first, then push the matching `vX.Y.Z` tag. Existing stable releases cannot be overwritten. Checksums verify download integrity, not trust in a publisher.

## Primary references

- [OpenRazer device classes](https://github.com/openrazer/openrazer/tree/master/daemon/openrazer_daemon/hardware)
- [DPI, polling and power methods](https://github.com/openrazer/openrazer/blob/master/daemon/openrazer_daemon/dbus_services/dbus_methods/mamba.py)
- [Base-device defaults and effect state](https://github.com/openrazer/openrazer/blob/master/daemon/openrazer_daemon/hardware/device_base.py)
- [Per-zone lighting APIs](https://github.com/openrazer/openrazer/blob/master/daemon/openrazer_daemon/dbus_services/dbus_methods/lanceheadte.py)
- [Native command bounds](https://github.com/openrazer/openrazer/blob/master/driver/razerchromacommon.c)
- [Input Remapper CLI](https://github.com/sezanzeb/input-remapper/blob/main/inputremapper/bin/input_remapper_control.py)
- [Linux input semantics](https://docs.kernel.org/input/event-codes.html)
- [fwupd D-Bus API](https://github.com/fwupd/fwupd/blob/main/src/org.freedesktop.fwupd.xml)

The plugin is MIT-licensed; upstream APIs/references retain their own licenses. This is not a vendor-endorsed firmware tool.
