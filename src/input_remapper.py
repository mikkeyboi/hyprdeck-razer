"""Small JSON bridge to the installed Input Remapper v2 backend, not an injector.

Uses upstream groups, paths and Mapping/Preset validation, never root injectors.
API reference: sezanzeb/input-remapper 0092e33ed13592dd937297d5032db9d908db8f9e.
Only explicit preset saves mutate state. Apply/stop belongs to the upstream editor.
"""

import contextlib
import json
import os
from pathlib import Path
import signal
import sys


def expired(_signal, _frame):
    # Backend discovery runs an upstream worker thread; sys.exit alone could wait
    # indefinitely for that thread. Terminate only this read/control helper.
    sys.__stdout__.write(json.dumps({"error": "Backend timed out; refresh daemon state before retrying"}) + "\n")
    sys.__stdout__.flush()
    os._exit(1)


signal.signal(signal.SIGALRM, expired)
signal.alarm(12)


def discover_groups(paths):
    from inputremapper.groups import groups

    physical = set(paths)
    groups.refresh()
    matches = []
    for group in groups.get_groups():
        if not physical.intersection(group.paths):
            continue
        if not set(group.paths).issubset(physical):
            raise ValueError(
                "Input Remapper grouped this product with input nodes outside it; "
                "refusing a mapping that could affect another physical device"
            )
        matches.append(group)
    return matches



def inputs(group):
    import evdev
    from inputremapper.utils import get_device_hash

    result = []
    for path in group.paths:
        with contextlib.closing(evdev.InputDevice(path)) as device:
            origin = get_device_hash(device)
            for code in device.capabilities(absinfo=False).get(evdev.ecodes.EV_KEY, []):
                name = evdev.ecodes.bytype[evdev.ecodes.EV_KEY].get(code, str(code))
                if isinstance(name, (list, tuple)):
                    name = name[0]
                result.append({
                    "value": json.dumps({"path": path, "code": code, "origin": origin}),
                    "label": f"{name} — {device.name} ({Path(path).name})",
                })
    return sorted(result, key=lambda item: item["label"])


def preset_directory(group):
    from inputremapper.configs.paths import PathUtils

    root = Path(PathUtils.get_preset_path())
    directory = Path(group.get_preset_path())
    if (
        not root.is_absolute()
        or directory.parent != root
        or directory.name in (".", "..")
        or directory.is_symlink()
        or directory.resolve().parent != root.resolve()
    ):
        raise ValueError("Unsafe device-model preset directory; no file was read or saved")
    return directory


def preset_path(group, name):
    if (
        not isinstance(name, str)
        or not name.strip()
        or name != name.strip()
        or len(name) > 80
        or name in (".", "..")
        or any(character in name for character in '/\\?%*:|"<>\x00')
        or "." in name
        or any(ord(character) < 32 for character in name)
    ):
        raise ValueError("Use a preset name of 1–80 characters without dots, slashes or reserved filename characters")
    directory = preset_directory(group)
    path = Path(group.get_preset_path(name))
    if path.parent != directory or path.resolve().parent != directory.resolve():
        raise ValueError("Preset path escapes its device-model directory")
    return path


def load_preset(path):
    # Preset.load deliberately skips malformed mappings. Validate each entry first
    # instead: applying a partially dropped preset would be misleading.
    from inputremapper.configs.mapping import Mapping
    from inputremapper.configs.preset import Preset

    raw = json.loads(path.read_text())
    if not isinstance(raw, list) or not raw:
        raise ValueError("Preset must be a nonempty Input Remapper v2 mapping array")
    preset = Preset()
    for item in raw:
        mapping = Mapping(**item)
        mapping.assert_strict()  # Includes upstream's macro Parser.parse.
        preset.add(mapping)
    return preset


def describe(group):
    directory = preset_directory(group)
    presets = []
    if directory.is_dir():
        for path in sorted(directory.glob("*.json")):
            # Inventory never executes macro parsers or controls root injectors.
            presets.append(path.stem)
    description = {
        "key": group.key,
        "name": group.name,
        "paths": group.paths,
        "directory": str(directory),
        "presets": presets,
        "inputs": [],
    }
    try:
        description["inputs"] = inputs(group)
    except Exception as error:
        description["input_error"] = str(error)
    return description


def main(request):
    groups = discover_groups(request["paths"])
    operation = request["operation"]
    if operation == "discover":
        return {"groups": [describe(group) for group in groups]}

    group = next((group for group in groups if group.key == request["group"]), None)
    if group is None:
        raise ValueError("Selected Input Remapper group no longer belongs to this connected product; refresh devices")

    if operation == "save":
        import evdev
        from inputremapper.configs.mapping import Mapping
        from inputremapper.configs.preset import Preset
        from inputremapper.utils import get_device_hash

        path = preset_path(group, request["name"])
        selected = json.loads(request["input"])
        if selected["path"] not in group.paths:
            raise ValueError("Macro trigger does not belong to the selected backend group")
        with contextlib.closing(evdev.InputDevice(selected["path"])) as device:
            if selected["origin"] != get_device_hash(device):
                raise ValueError("Input identity changed; refresh devices")
            if selected["code"] not in device.capabilities(absinfo=False).get(evdev.ecodes.EV_KEY, []):
                raise ValueError("Selected input does not advertise this key/button; record it in Input Remapper instead")
        macro = request["macro"].strip()
        if not macro or len(macro) > 16384:
            raise ValueError("Enter a key symbol or programmable macro of at most 16384 characters")
        mapping = Mapping(
            input_combination=[{"type": evdev.ecodes.EV_KEY, "code": selected["code"], "origin_hash": selected["origin"]}],
            target_uinput="keyboard",
            mapping_type="key_macro",
            output_symbol=macro,
            name="Hyprdeck macro",
        )
        mapping.assert_strict()
        preset = Preset()
        preset.add(mapping)
        # Upstream's schema serializer, without Preset.save's truncate/overwrite:
        # creation never replaces another editor's work or activates the mapping.
        value = mapping.dict(exclude_defaults=True)
        value["input_combination"] = mapping.input_combination.to_config()
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("x") as file:
            json.dump([value], file, indent=4)
            file.write("\n")
        load_preset(path)
        return {"saved": str(path)}

    raise ValueError("Unknown backend operation; apply and stop mappings in Input Remapper's editor")


try:
    with contextlib.redirect_stdout(sys.stderr):
        result = main(json.loads(sys.argv[1]))
    print(json.dumps(result))
except Exception as error:
    print(json.dumps({"error": f"{type(error).__name__}: {error}"}))
    sys.exit(1)
