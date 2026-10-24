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

Plugin **0.6.0** requires a compatible Hyprdeck host with product pages, native Color/Form/DPI controls, searchable form choices up to 1024 options, and separate controller input streaming. API version 1 alone does not imply those capabilities; publish/update the host before the plugin.

Open the real UI with `hyprdeck --page plugin:razer` after installation/enabling. Smoke entrypoints are the product gallery, Basilisk Sensitivity/Lighting/Power, and Wolverine Overview/Buttons/Keys & Macros/Tools/Firmware. Opening/refreshing is read-only. Native V4 controls need the scoped [interface permissions](controller-linux-support.md#native-v4-pc-hid-controls); plugin installation does not grant hardware permissions.

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

Mouse logo, wheel and side strips are independent lighting zones; dock brightness/effects use separate interfaces. Unified Chroma Sync intersects the exported effect signatures across primary zones, keeps charging-state zones separate and preflights every requested setter before any write. A D-Bus error is propagated; earlier accepted writes may remain, so refresh before applying again. Success means daemon acceptance, not independent hardware readback. Charging-light setters lack readback; their form values are explicitly choices to apply, not current state. Battery uses `getBattery`/`isCharging`; a connected dock does not establish charging. OpenRazer's unread `0.0`/`v0.0` firmware sentinel is not an installed version or evidence of an empty battery.

Desktop battery notifications use `src/battery_notifications.rs`, exposed in **Power** for devices advertising `getBattery`. They are global OpenRazer preferences, separate from the hardware low-battery indicator. The native form submits `battery-notifications` with `operation: "save"` and exactly `value: {"enabled": true, "percent": 20, "seconds": 1200}`. `enabled` must be a JSON boolean; percentage and interval must be integers in 1–100 and 0–43200 respectively. The file is `$XDG_CONFIG_HOME/openrazer/razer.conf`, otherwise `$HOME/.config/openrazer/razer.conf`, with an absolute, nonsymlink directory path required. The `[Startup]` keys are `battery_notifier`, `battery_notifier_percent`, and `battery_notifier_freq`; missing values fall back to disabled, 33%, and 600 seconds. Legacy `mouse_battery_notifier` / `mouse_battery_notifier_freq`, including `[DEFAULT]` inheritance, override current keys in upstream OpenRazer and are removed on save. Saves preserve unrelated text, comments, CRLF and existing file permissions; new files are mode 0600. Writes are locked and atomic. Malformed/duplicate/multiline battery values, invalid ranges, unsafe links and non-user-owned/nonregular files are refused without overwriting the config.

The interval is a check/repeat cadence, not a one-shot cooldown: upstream checks elapsed `timedelta.seconds > frequency`, so exact timing is not guaranteed and day-wrapping intervals are unsuitable. Zero disables notifications even with `enabled: true`. Popups may repeat while charging because the notifier does not consult charging state. Threshold 100 permits a full-battery popup, not an independent full-charge alert. These semantics and defaults were checked against installed OpenRazer 3.12.4's `daemon.py`, `hardware/device_base.py`, and `misc/battery_notifier.py`.

Saving/refreshing never restart OpenRazer or write hardware settings. `operation: "restart"` is a separate destructive button with native host confirmation. It validates the saved file and requires an already-active `openrazer-daemon.service` running the standard `/usr/bin/openrazer-daemon` command with only foreground/verbose flags. The next startup's config path comes from systemd's D-Bus manager environment, unit overrides and final `UnsetEnvironment`, not `/proc`: upstream `setproctitle` overwrites the original process arguments/environment. Environment files, PAM and custom roots are refused because their config path cannot be established by this guard. A mismatched config directory is refused before restarting. Success reports an active service with the verified default config path, not readback of live notifier preferences; all Razer devices briefly lose daemon controls.

Isolated-XDG smoke verification exercised the built plugin's version-1 `--request` save and CLI actions against the connected Basilisk receiver: missing-file form defaults, saved-value refresh, legacy override migration, CRLF/comments/unrelated text and mode-0640 preservation, 0/43200-second boundaries, rejected malformed values and symlink protection. The actual upstream `RazerDaemon.read_config` consumed the saved file as `[false, 100, 0]`. User configuration bytes/permissions/mtime and the running daemon PID were unchanged. This proves saved-file/API behavior, not desktop popup delivery or an applied daemon restart.

The rebuilt guard was subsequently exercised against the real user service without changing saved preferences. Upstream shutdown took six seconds, exceeding the original four-second control deadline; restart now allows ten seconds within the outer request deadline. The corrected action completed in 9.4 seconds and confirmed an active service on the expected default config path. This proves service reload, not desktop popup delivery.

Input Remapper is a separate software experience, not onboard configuration. `src/remapper.rs` embeds `src/input_remapper.py`, importing the distribution-installed v2 package with isolated system Python. Local backend groups must match this product's physical event paths; mixed-product groups are rejected and docks are not offered unusable remapping controls. Device-derived preset directories must be one safe component under the backend preset root; dot/dotdot components, model symlinks and escaping paths are rejected. Presets are shared per backend device model. Missing packages or unreadable inputs remain visible.

The native form creates a new, non-overwriting v2 preset from an advertised EV_KEY trigger, upstream `get_device_hash` metadata and keyboard output. The installed `Mapping.assert_strict()` macro parser and serializer validate the file. Saving does not inject, change onboard mappings, enable autoload, configure the root daemon or start a service. Use the upstream GTK editor to browse/edit existing presets and explicitly choose a device before applying or stopping. It may require administrator authentication for its reader. Its CLI has no device-selection option, so the launch row names the locally discovered group rather than claiming automatic preselection.

No daemon status/start/stop RPC is offered: the installed root API identifies groups by collision-prone display-name keys, with cached hotplug mappings and no physical/injector identity readback. Local path checks cannot prove which physical device those root calls target. Final smoke saved a real preset through the compiled plugin, opened the actual upstream editor, selected the Basilisk and observed its exact programmable macro text; a `Gaming v1.2.json` preset loaded through the real upstream parser. Dot/dotdot/empty model names and a model-directory symlink were rejected. Temporary files/readers were removed and no mappings remained active.

Backend API/schema reference: [Input Remapper revision `0092e33ed13592dd937297d5032db9d908db8f9e`](https://github.com/sezanzeb/input-remapper/tree/0092e33ed13592dd937297d5032db9d908db8f9e), version 2.2.1. The [AUR `input-remapper-git` PKGBUILD](https://aur.archlinux.org/cgit/aur.git/plain/PKGBUILD?h=input-remapper-git) inspected on 2026-10-16 declares GTK3, GtkSourceView4, python-cairo/evdev/gobject/packaging/psutil/pydantic/dasbus, with git and python-pip build dependencies and optional xorg-xmodmap. It fetches floating Git HEAD with `sha256sums=('SKIP')`, so pin and review the actual checkout rather than trusting the old `pkgver` string. Upstream's installer invokes pip with `--no-deps` but build isolation can still fetch setuptools >=77.0.3; translation compilation also requires gettext's `msgfmt`. Build/package unprivileged before inspecting/installing the archive. Do not run the upstream installer directly as root: its `/` mode uninstalls existing pip packages. The package installs a root system-bus injection service, permissive bus policy, polkit action, udev autoload rules and an XDG autostart entry; inspect these effects before installation. Hyprdeck neither installs the package nor starts/enables persistent services. Explicit temporary service startup or GUI authentication and input/uinput permissions are prerequisites for live injection checks.

Controller inspection uses nonexclusive read-only evdev ioctls. Snapshots can miss brief presses; changed-frame cadence does not measure every USB packet or internal sensor poll. Independent rear-button identity is unavailable under ordinary xpad. See [controller Linux support](controller-linux-support.md) for naming, RGB, battery and headphone-audio findings.

Wolverine V4 PC HID (`1532:0a79`) uses `src/wolverine.rs` and dedicated mapping/keyboard/firmware modules. Only HID interface 1 is opened; `hid-generic` and evdev remain bound. Identity, checksums, session deadlines and concurrent transaction locks are enforced. `wolverine-configure` uses top-level string `profile` for `operation: "mapping"` or `"keyboard"`; `value` is respectively `{"button":"m3","target":"default"}` (also `keep`, `disabled`, advertised controller targets) or `{"button":"m3","shortcut":"Ctrl+Shift+F24"}`. Polling uses string `value.profile`/`value.rate`. Writes reject stale active profiles and verify the selected record; no profile-wide writes or automatic resets occur.

## Firmware and releases

Original V3 (`1532:0a3f`) updater discovery validates the official HTTPS redirect/model filename without running downloads or comparing USB `bcdDevice`. V4 native PC HID firmware readback decodes `00/87` major/minor/internal fields; reserved/trailing bytes remain raw Diagnostics, not invented component versions. The attached device read **1.0.22**. Official V4 support article 21268 lists Pairing Utility v1.00.00_r2, not a firmware image. Without a verified V4 image/transport or exact fwupd/LVFS match, no native flashing is offered. Xbox/XInput xpad cannot provide that HID readback; V3's updater is never offered for V4.

For another device genuinely enumerated as supported/updatable by fwupd, the standalone `firmware-update DEVICE_ID --confirm` command verifies vendor identity and upgrades before invoking device-specific `fwupdmgr`. Historical, inhibited, wildcard, unreachable and emulated targets are rejected. Do not interrupt a flash or invoke it through the host request deadline.

CI runs formatting, Clippy, tests, a release build and headless protocol checks. Immutable releases use Ubuntu 22.04 x86-64 and contain:

- `plugin.json`
- `hyprdeck-razer-linux-x86_64`
- `hyprdeck-razer-linux-x86_64.sha256`
- `70-hyprdeck-wolverine.rules` (optional administrator-installed active-seat access to the V4 PC HID configuration and gamepad interfaces)

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
