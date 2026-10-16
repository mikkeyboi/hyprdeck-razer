"""Small JSON bridge to the installed Input Remapper v2 backend, not an injector.

Uses upstream groups, paths, Mapping/Preset validation and the system-bus daemon.
API reference: sezanzeb/input-remapper 0092e33ed13592dd937297d5032db9d908db8f9e.
Only explicit save/start/stop requests mutate state; discovery never starts services.
"""

import contextlib
import json
import os
from pathlib import Path
import signal
import sys
import time


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


def service():
    # Do not use Daemon.connect(): even a read would then set_config_dir, and its
    # default fallback can run pkexec. No implicit daemon startup or authentication.
    from inputremapper.daemon import DAEMON

    proxy = DAEMON.get_proxy()
    proxy.Introspect(timeout=1000)
    return proxy


def state(proxy, group):
    return {
        "state": str(proxy.get_state(group.key, timeout=1000)),
        "preset": proxy.get_running_preset(group.key, timeout=1000),
        "suspended": proxy.is_suspended(timeout=1000),
    }


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
    return Path(group.get_preset_path(name))


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


def describe(group, proxy, service_error):
    directory = Path(group.get_preset_path())
    presets = []
    if directory.is_dir():
        for path in sorted(directory.glob("*.json")):
            # Validation happens at application, not by executing every macro
            # parser while merely displaying inventory.
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
    try:
        if proxy is None:
            raise RuntimeError(service_error)
        description.update(state(proxy, group))
    except Exception as error:
        description["service_error"] = str(error)
    return description


def main(request):
    groups = discover_groups(request["paths"])
    operation = request["operation"]
    if operation == "discover":
        try:
            proxy, error = service(), None
        except Exception as exception:
            proxy, error = None, str(exception)
        return {"groups": [describe(group, proxy, error) for group in groups]}

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

    proxy = service()
    if operation == "start":
        from inputremapper.configs.paths import PathUtils
        from inputremapper.configs.global_config import INITIAL_CONFIG

        name = request["preset"]
        path = preset_path(group, name)
        load_preset(path)
        if proxy.is_suspended(timeout=1000):
            raise ValueError("Input Remapper is globally suspended; resume it in its editor first. Hyprdeck will not change other device mappings")
        # The daemon refuses a session without config.json, including a first
        # launch that has only saved presets. Create upstream defaults only;
        # never truncate existing preferences or enable autoload.
        config_dir = Path(PathUtils.config_path())
        config_dir.mkdir(parents=True, exist_ok=True)
        try:
            with (config_dir / "config.json").open("x") as file:
                json.dump(INITIAL_CONFIG, file, indent=4)
                file.write("\n")
        except FileExistsError:
            pass
        proxy.set_config_dir(str(config_dir), timeout=2000)
        if not proxy.start_injecting(group.key, name, timeout=3000):
            raise ValueError("Input Remapper daemon rejected this preset; inspect its service log")
    elif operation == "stop":
        proxy.stop_injecting(group.key, timeout=2000)
    else:
        raise ValueError("Unknown backend operation")

    # start_injecting acknowledges process startup, not injection readiness.
    # Poll the daemon's actual per-group state and running preset, never CLI exit.
    deadline = time.monotonic() + 4
    while True:
        observed = state(proxy, group)
        if operation == "start":
            if observed["state"] == "RUNNING" and observed["preset"] == name and not observed["suspended"]:
                return observed
            if observed["state"] in ("FAILED", "NO_GRAB", "UPGRADE_EVDEV"):
                raise RuntimeError(f"Daemon reports {observed}; mapping is not running")
        elif observed["state"] in ("UNKNOWN", "STOPPED") and not observed["preset"]:
            return observed
        if time.monotonic() >= deadline:
            raise RuntimeError(f"Requested {operation}, but daemon has not confirmed it: {observed}. Refresh state or inspect Input Remapper's service log")
        time.sleep(0.1)


try:
    with contextlib.redirect_stdout(sys.stderr):
        result = main(json.loads(sys.argv[1]))
    print(json.dumps(result))
except Exception as error:
    print(json.dumps({"error": f"{type(error).__name__}: {error}"}))
    sys.exit(1)
