# hyprdeck-razer

Razer peripheral controls for [Hyprdeck](https://github.com/mikkeyboi/hyprdeck), with one page per product—not per USB interface.

## Capabilities

**Basilisk Ultimate + charging dock**
- Linked or independent horizontal/vertical DPI.
- Editable desktop DPI presets, sample stages, and Previous/Next selection.
- Polling choices: **125, 500 and 1000 Hz**.
- Per-zone mouse RGB effects, colors and brightness; separate dock lighting.
- Battery/charging status, sleep timeout and low-battery warning.
- Optional software button mapping and macros through [Input Remapper](https://github.com/sezanzeb/input-remapper).

**Wolverine V3 Pro**
- Live controller diagram with button, stick, trigger and D-pad inspection.
- USB/driver status and input-cadence measurement.
- Official firmware-update discovery and vendor setup instructions.

Other Razer devices expose controls supported by their OpenRazer backend. Technical details stay in **Diagnostics**.

<p align="center">
  <img src="docs/screenshots/razer-products.png" width="49%" alt="Detected Razer products">
  <img src="docs/screenshots/basilisk-sensitivity.png" width="49%" alt="Linked DPI, desktop presets and polling settings">
  <img src="docs/screenshots/basilisk-lighting.png" width="49%" alt="Basilisk RGB controls">
  <img src="docs/screenshots/wolverine-overview.png" width="49%" alt="Wolverine controller overview">
</p>

Screenshots use the actual app, live device state and a neutral libadwaita theme.

## Install

Update Hyprdeck first: plugin **0.4.0** requires its linked-DPI editor and product pages.

```sh
hyprdeck plugins install mikkeyboi/hyprdeck-razer
hyprdeck plugins enable razer
hyprdeck --page plugin:razer
```

Mouse/dock settings require [OpenRazer](https://openrazer.github.io/#download), matching kernel modules, device permissions and a running user daemon. Controller input inspection works independently. Only enable plugins you trust; they run unsandboxed as your user.

**Updates:** `hyprdeck plugins check razer` / `hyprdeck plugins update razer`, or use Hyprdeck’s **Plugins** manager.

## Important limits

- **Desktop DPI presets are not onboard stages.** They do not change the mouse’s physical DPI-cycle button. OpenRazer does not expose Basilisk Ultimate’s onboard stage table, button mapping or macros.
- **Wolverine RGB, onboard mappings, battery and headphone audio lack a verified native Linux control path here.** The hardware supports RGB; use the vendor app for supported settings. [Driver findings](docs/controller-linux-support.md).
- The original Wolverine supports **250 Hz wireless / 1000 Hz wired Tournament Mode**, not 8K. The 8K PC model is separate hardware.
- Firmware discovery does not flash the Wolverine or compare its installed firmware. Its official updater requires Windows.

Settings change only through explicit **Apply**. **Save presets** writes desktop preferences without changing device DPI.

## Development

[Build, CLI, preset storage and release instructions](docs/DEVELOPMENT.md) · [Hyprdeck plugin API](https://github.com/mikkeyboi/hyprdeck/blob/main/docs/PLUGINS.md) · [MIT license](LICENSE)
