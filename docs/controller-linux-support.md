# Wolverine V3 Pro & V4 Pro Linux driver evidence

Observed USB identities:
- **Wolverine V3 Pro**: `1532:0a3f` (original 2.4 GHz receiver / wired), `1532:0a57` (8K PC wired), `1532:0a59` (8K PC wireless receiver).
- **Wolverine V4 Pro (`RZ06-0606`)**:
  - `1532:0a76` (`bcdDevice 1.01`): Xbox / default mode (Full-Speed USB 12 Mbps, `bInterval = 4` → 250 Hz schedule; Interface 0 GIP `ff/47/d0` bound to `xpad`, Interface 1 GIP audio `ff/47/d0` unbound).
  - `1532:0a78` (`bcdDevice 1.00`): PC High-Speed 8K mode (High-Speed USB 480 Mbps, up to 8000 Hz HyperPolling; Interface 0 bound to `xpad`, Interfaces 1 & 2 bound to `hid-generic` keyboard/mouse/vendor HID).
  - `1532:0a79`: PC HID mode (**Function + Menu + Y**). High-Speed USB; interface 1 exposes feature report ID 10, with 90 data bytes. Native configuration uses that report without detaching the kernel input driver.
  - Mode switching transitions briefly (~1 s) through `0e8d:0003` (MediaTek MT6877/wireless SoC bootloader) before re-enumerating.
## Steam name

The running CachyOS 7.2.9 `xpad` binding creates an input device named **Generic X-Box pad**. This is the kernel input name Steam receives, not a separate product or an indication that the controller is disconnected. The plugin identifies the physical controller using USB identity and keeps the raw input name in Diagnostics.

The [OpenRazer device issue](https://github.com/openrazer/openrazer/issues/2364) includes a proposed named `xpad` table entry for `1532:0a3f`. The [kernel patch supplied during setup](https://git.kernel.org/pub/scm/linux/kernel/git/stable/stable-queue.git/plain/queue-6.19/input-xpad-add-support-for-razer-wolverine-v3-pro.patch?id=29da4200730c8c1565729677fb61d23daa174e7f) adds `0a57` and `0a59`; those are the separate 8K PC personalities, not this `0a3f` receiver. Renaming the plugin label does not change Steam's kernel device identity. No global kernel-driver replacement or fake virtual gamepad is installed just to change the name.

## Headphone jack and controller battery

On the observed `xpad` binding:

- USB interface 0 is bound to `xpad`; interface 1 is unbound.
- No Wolverine ALSA playback/capture device exists.
- No controller `power_supply` or UPower battery object exists.

The V4 Pro in **PC HID mode** now supplies battery percentage and charging status directly through native HID queries; it does not require a kernel power-supply object. This does not establish battery support in Xbox/XInput mode or headset-audio support.

For the **V3 Pro**, [Razer's official FAQ](https://mysupport.razer.com/app/answers/detail/a_id/14587) documents its headset jack, RGB settings through **Razer Controller Setup for Xbox**, and lack of onboard macro recording. These V3 findings do not establish V4 lighting capabilities or a working Linux configuration API. The [V4 product page](https://www.razer.com/console-controllers/razer-wolverine-v4-pro) documents Synapse customization, capacitive stick sensors and up to 8K PC polling.

### Scoped GIP trial (2026-10-09)

The user authorized an experimental [xone](https://github.com/dlundqvist/xone) trial. It was built against the exact running kernel with LLVM, and its USB match table was restricted to `1532:0a3f`, interface 0. Only this controller was rebound for 30 seconds. `xpad` remained loaded for other devices; no blacklist, persistent module installation or receiver firmware was installed.

Observed result: no GIP controller input, no new ALSA output, and no battery object. The controller was restored automatically to `xpad`; working input was preserved after rollback. This trial does **not** establish xone compatibility with the original Razer receiver. A verified device-specific GIP/receiver implementation is still required for headset audio and battery reporting in that mode; the V4 PC HID battery query below is separate.

## RGB and onboard assignments

The V3 vendor app controls its RGB brightness, effect and color. The [V3 community reverse-engineering notes](https://github.com/Netruk44/wolverine-v3-pro-linux/blob/main/docs/reverse-engineering.md) distinguish a partly understood wired vendor protocol from an unresolved wireless receiver envelope. Neither those notes nor opcode strings in a vendor DLL prove a V4 receiver command. The plugin does not send guessed configuration packets.

V4 PC HID polling, battery, installed firmware, M1–M6 controller-button assignments and single-key/modifier shortcuts are implemented natively. Synapse's `AvailableDevices.json` marks V4 wired/dongle identities, including `0a79`, `isNotChromaDevice: true`; generic quick-effect defaults do not establish RGB capability. Its mode/charging status indicator is not an addressable Chroma zone. Read-only lighting diagnostics returned Command Failure (status 3); unsupported queries are not repeated on refresh. Linux software macros use actual advertised events and never infer independent paddles.

## Native V4 PC HID controls

Switch the controller with **Function + Menu + Y** and confirm `1532:0a79`. Leave it owned by Linux, not passed through to WinBoat. In Hyprdeck → Razer peripherals → Wolverine V4 Pro:

- **Sensitivity** reads the current onboard profile's configured polling rate and offers 250, 500, 1000, 2000, 4000 and 8000 Hz. This is configuration readback, not measured USB/input cadence.
- **Buttons** reads M1/M2 (extra shoulder buttons) and M3–M6 (rear paddles), then changes one selected controller assignment, firmware default or disable. **Keep current** preserves an existing assignment without rewriting it. A target-zero record means firmware default, not a guessed A-button assignment.
- **Power** reads the controller battery and charging status. Failed reads remain unavailable, not zero/full by inference.
- **Keys & Macros** saves one native key/chord (letters, digits, F1–F24, named keys and modifiers), or creates explicitly applied Linux software macros through Input Remapper. These are separate storage/execution paths.
- **Tools** reports visible timed per-stick movement/range/circularity and changed-frame cadence results. Idle/incomplete motion remains explicitly unmeasured. HID mode has no evdev force feedback; native motor scaling is read-only Diagnostics, not a playback test with guessed duration units.
- **Firmware** reads native `00/87` major/minor/internal fields, retaining reserved/trailing bytes only in Diagnostics. The attached controller returned **1.0.22**. The official V4 support article lists a pairing utility, not a verified firmware image/update transport; no native V4 flash or V3 updater is offered.

For active-seat permissions, install the supplied rule as an administrator, reload rules, then reconnect the controller:

```sh
sudo install -m 0644 data/70-hyprdeck-wolverine.rules /etc/udev/rules.d/70-hyprdeck-wolverine.rules
sudo udevadm control --reload-rules
```

The standalone release also includes `70-hyprdeck-wolverine.rules`; use its downloaded path instead of `data/…`. Active-seat access is scoped to this model's **HID interface 1** (native configuration), its **gamepad HID interface 0** and controller input nodes. Interface 0 access lets Steam Input and Proton read the gamepad as raw HID, as `steam-devices` does for supported controllers; Hyprdeck only opens interface 1. Other keyboards/mice are not granted access and Hyprdeck does not run as root. Input-node access permits the controller's native keyboard assignments to be read and used as software macro triggers.

Protocol evidence comes from Synapse's SagePC implementation: polling get/set `00/c0` / `00/40` with `[profile, period]`; battery `07/80`; charging `07/84`; active profile `05/84`; button list `02/84`; single-assignment get/set `02/8c` / `02/0c`. Assignment records are `[profile, source, mode, function, size, data[5]]`; controller function 16 uses vendor target IDs (A = 32); keyboard function 2 uses `[modifiers, USB HID key]` with size 2. Applying rejects stale profiles, writes only the selected record and verifies fresh readback. No whole-profile write, mode switch or automatic reset occurs.

Hardware verification: the Linux plugin set 8000 Hz and read it back, then restored the original 1000 Hz. M3 → A read back as `01 12 00 10 01 20 00 00 00 00`; M1, M2 and M4–M6 were unchanged byte-for-byte, and the exact original M3 record was restored afterward. Battery read 75%, not charging. This verifies onboard configuration transactions, not physical paddle output, sustained 8-kHz cadence, persistence across a power cycle, or cold-boot wireless recovery.

Additional verification: M3 → **Ctrl+Shift+F24** read back while the other five assignments remained unchanged; the original firmware-default assignment was restored. During backend investigation, an isolated F24-trigger macro emitted two F23 strokes (`down/up/down/up`) on the real virtual keyboard, then stopped. The shipped integration saves real presets and opens the upstream editor for explicit apply/stop; it never controls root injectors through ambiguous display-name keys. This does not claim a physical paddle press, onboard recorded macros or USB polling measurement.


## Cold-boot / resume wireless receiver recovery

The user reports that the receiver can enumerate after reboot while the controller remains unusable until a physical replug. A missed GIP initialization exchange is a hypothesis, not an established root cause. At inspection, the V4 receiver already had `power/control=on`; disabling autosuspend alone does not explain or prove a fix for this failure.

- **Manual / UI recovery**: **Reset USB receiver** (`hyprdeck-razer usb-reset usb:<port>`) issues `USBDEVFS_RESET`. The UI offers it only for observed receiver identities `0a3f`/`0a59`/`0a76` when the current USB node is writable. PC HID mode has no receiver-reset button. Completion proves the ioctl, not wireless recovery; reboot/wake plus real input still needs verification.
- Active-seat permissions can enable this explicit reset without running the GUI as root. If desired, an administrator can add a narrowly scoped udev rule for observed models (no unverified V4 PIDs):
  ```udev
  ACTION=="add|change", SUBSYSTEM=="usb", ATTR{idVendor}=="1532", ATTR{idProduct}=="0a3f|0a57|0a59|0a76|0a78", ATTR{power/control}="on", TAG+="uaccess"
  ```

## WinBoat USB diagnostics

In the observed VM, QEMU's container-local `info usbhost` missed the receiver after host re-enumeration even though its current `/dev/bus/usb` node existed. A stale `usb-host` object was not attached. Opening the current node with `hostdevice=` instead of a VID/PID discovery filter restored Windows enumeration: **Xbox Gaming Device**, present, status **OK**, problem code **0**. This establishes guest USB detection, not successful controller configuration.

Run the scoped helper from this repository after starting WinBoat:

```sh
python3 scripts/winboat-wolverine.py --capture-dir "$HOME/Downloads/wolverine-captures"
```

Requirements: Python 3 on host and container, access to the existing Docker `WinBoat` container and its local QEMU monitor, and the standard home-directory `/shared` mount. Disable WinBoat's native Wolverine selection and remove matching static `usb-host` arguments first so only one mechanism owns this controller. The helper handles only observed `1532:0a76` and `1532:0a78` identities, refuses multiple matching devices, and resolves each new USB node after re-enumeration. It does not pass the generic MediaTek bootloader, modify controller firmware, or change other USB devices. Capture is opt-in and local. Stop with Ctrl+C to release the device back to the host; this is not an installed login service.

For a transient user-controlled capture session, the same helper can be launched with `systemd-run --user --unit=hyprdeck-wolverine-capture --collect --property=Type=exec --property=KillMode=mixed --property=TimeoutStopSec=20 /usr/bin/python3 "$PWD/scripts/winboat-wolverine.py" --capture-dir "$HOME/Downloads/wolverine-captures"`. Stop and release the controller with `systemctl --user stop hyprdeck-wolverine-capture`. No service is enabled for future logins.

The Xbox setup app lists V4 Pro as supported but still showed **Connect a controller** during this capture. After launching Synapse 4, continuing as Guest and opening its dashboard, Synapse recognized **Razer Wolverine V4 Pro — Xbox** and showed physical mode-switch shortcuts. Its bundled code maps **Function + Menu + A** to XInput, **Function + Menu + Y** to HID, and **Function + Menu + X** to Xbox. A subsequent physical HID-mode transition exposed the native feature-report channel verified above.

Receiver interrupt-IN traffic included GIP status reports (`03 20 <sequence> 04 80 00 00 00`), not a validated Xbox receiver configuration response. The helper was exercised through explicit deselection/reselection and receiver re-enumeration; stopping/deselecting restored the host `xpad` binding. The subsequent native PC HID settings writes above do not establish a working Xbox-mode configuration protocol or cold-boot recovery.
