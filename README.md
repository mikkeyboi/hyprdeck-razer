# hyprdeck-razer

Standalone Rust backend for [Hyprdeck](https://github.com/mikkeyboi/hyprdeck). No Razer code is linked into Hyprdeck. The plugin executes as an explicitly trusted, unsandboxed subprocess and also provides a diagnostic CLI.

## Capabilities and limitations

| Path | Available behavior | Limits |
| --- | --- | --- |
| Linux USB sysfs | Enumerates every connected vendor `1532` device, including Wolverine `0a3f`, Basilisk Ultimate `0088`, Mouse Dock `007e`, and other Razer peripherals; product/serial, driver, speed, descriptor revision, interrupt-IN scheduling and runtime power state | USB enumeration does not establish controller radio connection or proprietary capabilities. No sysfs writes or kernel-driver replacement. IDs use USB port topology; refresh after moving ports. |
| Read-only, nonexclusive evdev | Current supported controller buttons and absolute axes initialized with kernel ioctls; compact keyboard/mouse button state, relative-axis capabilities; explicit timed changed-input-frame cadence measurement | Never grabs, injects input, sets force feedback or opens devices for writing. Relative mouse movement has no persistent absolute position. Steam/games keep receiving input. Removal and permission failures are explicit. |
| OpenRazer user-session D-Bus | Live introspection of **every exported `razer.device.*` method** per supported daemon device. Available firmware/driver/serial/device metadata, battery/charging, DPI/stages/max/available DPI, polling/supported rates, brightness, each lighting zone/effect/color/speed/direction, power settings, keyboard/device-specific controls, custom frame methods and other exported methods | Exhaustive exported API is not exhaustive proprietary hardware support. Controls only exist if introspected on the connected daemon object. Getter values may be OpenRazer's cached/persistent state, not hardware readback. Unknown battery sentinel is explicitly unavailable. Compound controls use real typed arguments, not guessed USB packets. |
| Original Wolverine firmware discovery | Explicit HTTPS HEAD check of the verified official shortlink, validated official redirect host/path and updater filename version; official support-page action through the desktop portal | Windows updater discovery only, **not Linux flashing**, not a comparison to installed firmware. Works only for verified original-model `1532:0a3f`. No `.exe` download/execution, Wine or USB firmware packet implementation. |
| fwupd/LVFS | Queries running fwupd for Razer USB firmware devices; manual CLI firmware update only after explicit confirmation, current device-specific vendor identity, supported/updatable metadata flags and nonempty `GetUpgrades` | No claim that Wolverine/Basilisk/Dock is supported. No generic whole-system update, forced firmware, downgrade or fabricated firmware path. The 15-second plugin protocol is deliberately never used to flash firmware. |

Wolverine onboard profiles, independent paddle assignments, trigger-lock positions, proprietary battery and installed firmware are unavailable via standard xpad/evdev. You can see the resulting mapped standard inputs, not independently read these physical/proprietary settings. OpenRazer does not become a Wolverine configuration backend just because it is installed.

The xpad [kernel support patch](https://git.kernel.org/pub/scm/linux/kernel/git/stable/stable-queue.git/plain/queue-6.19/input-xpad-add-support-for-razer-wolverine-v3-pro.patch?id=29da4200730c8c1565729677fb61d23daa174e7f) adds `1532:0a57` wired and `1532:0a59` wireless Xbox 360-style input support. The observed original wireless USB identity here is `1532:0a3f` (Xbox One-style xpad). Do not infer original/8K physical rates from the other IDs alone. The plugin supports native input discovery for all of them without replacing xpad. The [SteamOS-specific community wired paddle driver](https://github.com/Netruk44/wolverine-v3-pro-linux) is not a general configuration/firmware protocol and is not installed by this plugin.

## Build and install

Linux x86-64, a current stable Rust toolchain (edition 2024), and the normal Rust linker/build environment are sufficient. No GTK, libusb, Python binding or system OpenSSL library is needed by this binary. HTTPS uses Rustls. OpenRazer, the user-session bus/desktop portal, and fwupd are optional at runtime.

```sh
cargo build --locked --release
cargo test --locked
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
```

For a local Hyprdeck install, prepare the exact installed layout in a staging directory, then let Hyprdeck validate and install it:

```sh
mkdir -p dist/local
install -m 0755 target/release/hyprdeck-razer dist/local/hyprdeck-razer
install -m 0644 plugin.json dist/local/plugin.json
hyprdeck plugins install ./dist/local
hyprdeck plugins enable razer
hyprdeck plugins state razer
```

Or install the separately published release:

```sh
hyprdeck plugins install mikkeyboi/hyprdeck-razer
hyprdeck plugins enable razer
```

Installation does **not** imply enablement/trust. Hyprdeck installs `plugin.json` and the regular executable (not a symlink) under `$XDG_DATA_HOME/hyprdeck/plugins/razer/`, defaulting to `~/.local/share/hyprdeck/plugins/razer/`. Enabled IDs are recorded under `$XDG_CONFIG_HOME/hyprdeck/plugins.toml`. GUI management/controls are on the Plugins page. Disable with `hyprdeck plugins disable razer`; release checks/updates use `hyprdeck plugins check razer` and `hyprdeck plugins update razer`.

Trust this repository/binary before enabling: it runs as your user without a sandbox. Normal state reads sysfs, opens only connected Razer input nodes read-only and talks to optional OpenRazer/fwupd D-Bus services. Only an explicit control action changes settings. Explicit measurements/vendor checks save device-bound historical results in `$XDG_STATE_HOME/hyprdeck-razer/` (default `~/.local/state/hyprdeck-razer/`), using atomic mode-0600 files. Reconnection identity changes invalidate cached results. No background network request or automatic package installation occurs. An explicit vendor check contacts only allowed official Razer HTTPS hosts; opening instructions uses the user desktop portal. Manual `firmware-update` runs device-specific `fwupdmgr`, which may request Polkit authentication.

### Optional daemon and input permissions

When `org.razer` is absent, the plugin explains the daemon dependency and still shows native controller diagnostics. Follow [OpenRazer's distribution instructions](https://openrazer.github.io/#download) for its kernel modules, daemon, and its **own** device permissions; run `openrazer-daemon` in the desktop user session. `python-openrazer` is not required by this Rust plugin. On Arch/CachyOS the packaged runtime commands include `sudo pacman -S openrazer-daemon fwupd`; install the matching OpenRazer kernel module package from the distribution instructions, and remember fwupd is optional and does not imply Wolverine support. A failed administrative prompt is not a successful install.

For evdev, an active local session normally obtains `uaccess` ACLs. Diagnose permission errors using `getfacl /dev/input/eventN`, your login/seat state and distribution udev policy. **Do not use input-group membership as the default fix:** it grants access to unrelated keyboard input. Do not run Hyprdeck as root, apply broad `chmod`, or detach xpad. OpenRazer sysfs permissions and evdev ACLs are separate.

## CLI and version-1 subprocess API

```sh
hyprdeck-razer state
# Use the input ID shown by state, not a guessed event number:
hyprdeck-razer measure input:usb:1-11:event256 4
hyprdeck-razer firmware-check usb:1-11
hyprdeck-razer action vendor-page '{"device":"usb:1-11"}'
```

The manual CLI emits the same response envelope as the host, pretty-printed. `state` discovers current IDs and controls. `--request` reads one JSON document from stdin, writes exactly one JSON response to stdout, and sends diagnostics only to stderr:

```sh
printf '%s\n' '{"api_version":1,"method":"state","action":null,"args":{}}' | hyprdeck-razer --request
printf '%s\n' '{"api_version":1,"method":"action","action":"measure","args":{"device":"input:usb:1-11:event256","seconds":4}}' | hyprdeck-razer --request
```

Responses are `{ "api_version": 1, "error": null, "state": { "title": ..., "description": ..., "groups": [...] } }`. Errors return a string and `state: null`. Actions return refreshed state and their completed result. One request/process, no shell evaluation. The host timeout is 15 seconds; the backend deadline is 13 seconds. Measurements accept 1–8 seconds; normal state does not measure or perform vendor network checks. OpenRazer reads are bounded and concurrent; its 3-second state budget cannot prevent native USB/input diagnostics. Stale cached measurements are labeled historical with their timestamp and are never called current polling.

### OpenRazer control arguments

Common Boolean settings with real getters use switches. Brightness and low-battery thresholds use documented percentage number controls. Polling uses the device's `getSupportedPollRates` choices, never a guessed model list. Every other exported device method is available via a button (no arguments) or a text **Apply** control showing the actual interface, member, argument names and D-Bus signatures. Empty fields do not issue default writes. Read-only getters requiring arguments are explicit queries, not automatic configuration writes.

Text controls accept a JSON array in positional introspected argument order. Examples **only if those methods appear for your current device**:

- `setDPI` (`qq`): `[800,800]`; for a single-DPI device exposing `availableDPI`, upstream requires `[800,0]` and an advertised DPI value.
- `setStatic` (`yyy`): `[0,255,0]`.
- `setDPIStages` (`ya(qq)`): `[1,[[800,800],[1600,1600]]]`; active stages are one-based in the upstream client API.
- `setWave` (`i`): `[1]`; follow the model's available upstream directions.
- `setIdleTime` (`q`, seconds): `[300]`.

JSON arrays encode D-Bus arrays/structs; dictionaries use `[[key,value],...]`, and variants use `{ "signature": "s", "value": "example" }`. Exact integer width/range and shape are checked, integral UI numbers are accepted, fractional integers are rejected. Remaining model-specific semantic constraints are enforced by the real daemon, not fabricated by this plugin. File-descriptor arguments are deliberately unavailable and labeled as such.

Potentially input-changing/destructive methods (reset, pairing, macros, key suppression/game mode and unknown nonlighting/device-specific operations) require an explicit JSON confirmation object:

```json
{"arguments":[true],"confirm":true}
```

For manual or host CLI calls, use:

```sh
hyprdeck-razer action openrazer-call '{"device":"SERIAL_FROM_STATE","interface":"razer.device.dpi","member":"setDPI","arguments":[800,800]}'
```

The daemon serial comes from the OpenRazer group, not the USB/input ID. The method is re-introspected before invocation; stale or unexported operations fail. A successful write means daemon acceptance, not independent device readback. Do not apply a method without reading its upstream semantics; custom matrices/macros are real APIs and can alter behavior.

## Polling: four different quantities

1. **Physical limits:** original Wolverine V3 Pro documentation specifies up to 250 Hz wireless and 1000 Hz wired PC Tournament Mode (`Function + Menu + A`): [original model PC Tournament Mode](https://www.razer.com/console-controllers/razer-wolverine-v3-pro). The Wolverine V3 Pro 8K PC is different hardware. Use a working USB port; changing ports can restore native input without any software workaround.
2. **USB endpoint schedule:** at low/full speed, interrupt `bInterval` counts 1 ms USB frames; at high/super speed it encodes `2^(bInterval-1)` 125 μs microframes ([USB 2.0 specification](https://www.usb.org/document-library/usb-20-specification), §9.6.6). Thus full-speed `bInterval=4` means 4 ms scheduling opportunities (~250/s), not a measured sensor poll. Interface endpoint descriptors are observed, not overridden.
3. **Configured OpenRazer poll rate:** `getPollRate` can reflect daemon/persistent state. An accepted `setPollRate` only exists for a supported daemon device; it is not a Wolverine xpad polling switch.
4. **Observed input cadence:** move sticks/triggers or mouse continuously during explicit measurement. Count only standard-input-bearing `SYN_REPORT` frames, use kernel timestamps, and report elapsed duration, count, valid interval mean/median/p95/min/max and lost frames. Zero changed frames is **idle/no estimate**, not “0 Hz physical polling.” One frame is insufficient. Radio, input filtering/deadzones, timestamps, load and batching affect the sample; this cannot prove every USB report or internal sensor rate.

Never force the `xpad` USB interval, detach a working driver, or write guessed Razer controller USB configuration. Read-only diagnostics coexist with Steam/Big Picture.

## Firmware safety

The official original-model [Windows firmware instructions](https://mysupport.razer.com/app/answers/detail/a_id/14630/~/razer-wolverine-v3-pro-firmware-updater-%7C-rz06-0520) were verified on 2026-10-09: page updated August 14, 2026; updater `v2.04.00_r1`, dated March 12, 2026, “General bug fixes.” The official [download shortlink](https://rzr.to/b556gr) resolved to the `WolverineV3Pro_0A3F_FirmwareUpdater_v2.04.00_r1.exe` Windows binary under `dl.razerzone.com/drivers/WolverineV3Pro/win/`. The explicit check resolves this link live; this dated observation is not a hardcoded “latest” fallback. Failure reports unavailable network/check status. Installed proprietary firmware remains unknown, and USB `bcdDevice` is labeled **descriptor revision**, never installed firmware.

Use a Windows PC and the vendor's exact updater prompts for dongle/controller stages. No native Linux flashing protocol has been established here. The plugin does not silently execute downloads, use Wine, compare unrelated versions, or claim fwupd support merely because it exists.

For a different Razer device that fwupd actually enumerates with current metadata support/updatable flags and upgrades, the deliberately manual route is:

```sh
hyprdeck-razer firmware-update FWUPD_DEVICE_ID_FROM_STATE --confirm
```
Historical, inhibited, wildcard-install, unreachable and emulated fwupd targets are rejected. A wildcard-capable target may update other devices and is not eligible for this explicitly device-scoped command.


Only that validated 40-hex-character device ID reaches `fwupdmgr update`. No force/downgrade/wildcard flags; fwupd's own requirements and interactive authentication still apply. Maintain power and do not interrupt. Never use a host plugin action or a 15-second subprocess supervisor for firmware flashing.

## CI and publishing

CI checks formatting, Clippy, real protocol/type/cadence error-boundary tests, independent release compilation and headless subprocess envelopes. It does not pretend to test connected hardware or an absent OpenRazer daemon. Release builds use Ubuntu 22.04 x86-64 for a conservative glibc baseline rather than distributing an Arch-local binary.

The release workflow runs on `v*` tags, or manual dispatch with an **existing** tag. Keep `Cargo.toml` and `plugin.json` versions identical, then create/push the corresponding tag (for example `v0.1.0`). It checks exact repository/manifest/tag identity and publishes three standalone stable-release assets:

- `hyprdeck-razer-linux-x86_64` — the single executable
- `plugin.json` — exact version-1 host manifest
- `hyprdeck-razer-linux-x86_64.sha256` — SHA-256 for the executable asset

Published release assets are immutable in this workflow: rerunning for an existing release refuses replacement. Fixes require a new matching Cargo/manifest version and tag.

Hyprdeck's installer renames the platform binary to the manifest's local `hyprdeck-razer` filename, validates the manifest/checksum and enables only on explicit user trust. Checksums protect integrity, not trust in the publisher. No installer script, auto-publication during local work or automatic firmware update is included.

## Primary API references

This implementation follows live D-Bus introspection and these upstream definitions, not copied firmware/USB command guesses:

- [OpenRazer daemon discovery](https://github.com/openrazer/openrazer/blob/master/daemon/openrazer_daemon/daemon.py) and [object/service naming](https://github.com/openrazer/openrazer/blob/master/daemon/openrazer_daemon/dbus_services/service.py).
- [Base device/current effect APIs](https://github.com/openrazer/openrazer/blob/master/daemon/openrazer_daemon/hardware/device_base.py).
- [Mouse battery, DPI/stages, polling and power methods](https://github.com/openrazer/openrazer/blob/master/daemon/openrazer_daemon/dbus_services/dbus_methods/mamba.py).
- [Device/firmware metadata](https://github.com/openrazer/openrazer/blob/master/daemon/openrazer_daemon/dbus_services/dbus_methods/all.py), [shared lighting/keyboard methods](https://github.com/openrazer/openrazer/blob/master/daemon/openrazer_daemon/dbus_services/dbus_methods/chroma_keyboard.py), and [device-specific methods directory](https://github.com/openrazer/openrazer/tree/master/daemon/openrazer_daemon/dbus_services/dbus_methods).
- [fwupd D-Bus API](https://github.com/fwupd/fwupd/blob/main/src/org.freedesktop.fwupd.xml) and [supported/updatable device flags](https://github.com/fwupd/fwupd/blob/main/libfwupd/fwupd-device.rs).
- [Linux input event semantics](https://docs.kernel.org/input/event-codes.html), including `SYN_DROPPED` recovery.

Licensed MIT; API references remain their upstream licenses. Razer and OpenRazer are independent projects; this is not a vendor-endorsed firmware tool.
