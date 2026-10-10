# Wolverine V3 Pro Linux driver evidence

The original controller's observed USB identity is `1532:0a3f`. Its wireless receiver exposes a vendor-specific Xbox One-style interface (`ff/47/d0`), not a USB Audio Class interface.

## Steam name

The running CachyOS 7.2.9 `xpad` binding creates an input device named **Generic X-Box pad**. This is the kernel input name Steam receives, not a separate product or an indication that the controller is disconnected. The plugin identifies the physical controller using USB identity and keeps the raw input name in Diagnostics.

The [OpenRazer device issue](https://github.com/openrazer/openrazer/issues/2364) includes a proposed named `xpad` table entry for `1532:0a3f`. The [kernel patch supplied during setup](https://git.kernel.org/pub/scm/linux/kernel/git/stable/stable-queue.git/plain/queue-6.19/input-xpad-add-support-for-razer-wolverine-v3-pro.patch?id=29da4200730c8c1565729677fb61d23daa174e7f) adds `0a57` and `0a59`; those are the separate 8K PC personalities, not this `0a3f` receiver. Renaming the plugin label does not change Steam's kernel device identity. No global kernel-driver replacement or fake virtual gamepad is installed just to change the name.

## Headphone jack and controller battery

On the observed `xpad` binding:

- USB interface 0 is bound to `xpad`; interface 1 is unbound.
- No Wolverine ALSA playback/capture device exists.
- No controller `power_supply` or UPower battery object exists.

The hardware has a 3.5 mm headset jack and RGB lighting. [Razer's official FAQ](https://mysupport.razer.com/app/answers/detail/a_id/14587) documents headset operation on Xbox/PC, lighting brightness/effect/color through **Razer Controller Setup for Xbox**, and no onboard macro recording. These hardware capabilities are not equivalent to a working Linux driver API.

### Scoped GIP trial (2026-10-09)

The user authorized an experimental [xone](https://github.com/dlundqvist/xone) trial. It was built against the exact running kernel with LLVM, and its USB match table was restricted to `1532:0a3f`, interface 0. Only this controller was rebound for 30 seconds. `xpad` remained loaded for other devices; no blacklist, persistent module installation or receiver firmware was installed.

Observed result: no GIP controller input, no new ALSA output, and no battery object. The controller was restored automatically to `xpad`; working input was preserved after rollback. This trial does **not** establish xone compatibility with the original Razer receiver. A verified device-specific GIP/receiver implementation is still required for headset audio and battery reporting.

## RGB and onboard assignments

Razer's official app controls the controller's RGB brightness, effect and color. The controller does not synchronize its Chroma lighting with other PC Chroma peripherals. The [community reverse-engineering notes](https://github.com/Netruk44/wolverine-v3-pro-linux/blob/main/docs/reverse-engineering.md) distinguish a partly understood wired vendor protocol from the still-unknown wireless receiver command envelope. The plugin does not send guessed receiver configuration packets.

Use the vendor app for supported onboard settings. Linux software remapping, if explicitly enabled using Input Remapper, is separate from onboard profiles and cannot identify an independent paddle that the driver does not report.
