#!/usr/bin/env python3
"""Keep one selected Wolverine V4 Pro attached across USB mode changes.

QEMU's container-local libusb discovery can miss host re-enumeration. Open the
current usbfs node instead; do not detach unrelated USB devices or kernel drivers.
"""

import argparse
import json
import logging
import pathlib
import signal
import subprocess
import threading
import time

VID = "1532"
PIDS = {0x0A76, 0x0A78}
DEVICE_ID = "hyprdeck-wolverine"
STOP = threading.Event()
# Docker provides access to the existing monitor; it does not install a guest
# command server or expose another network listener.
MONITOR = r'''
import json, socket, sys
s = socket.socket(socket.AF_UNIX)
s.settimeout(5)
s.connect('/run/shm/monitor.sock')
def response():
    data = b''
    while not data.endswith(b'(qemu) '):
        chunk = s.recv(65536)
        if not chunk:
            raise RuntimeError('QEMU monitor closed')
        data += chunk
    return data.decode(errors='replace')
response()
for command in json.load(sys.stdin):
    s.sendall((command + '\n').encode())
    data = response()
    if command == 'device_del hyprdeck-wolverine' and 'not found' in data:
        continue  # A previous run may already have released our owned handle.
    if any(word in data for word in ['Error:', 'failed', 'not found', 'not support', 'Duplicate ID']):
        raise RuntimeError(data)
s.close()
'''


def run(*args, **kwargs):
    result = subprocess.run(args, capture_output=True, text=True, timeout=12, **kwargs)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or result.stdout.strip() or f"{args[0]} failed")
    return result.stdout


def monitor(container, commands):
    run("docker", "exec", "-i", container, "python3", "-c", MONITOR,
        input=json.dumps(commands))


def selected_device(config):
    selected = PIDS if config is None else {
        item.get("productId")
        for item in json.loads(config.read_text()).get("passedThroughDevices", [])
        if item.get("vendorId") == int(VID, 16) and item.get("productId") in PIDS
    }
    devices = []
    for path in pathlib.Path("/sys/bus/usb/devices").iterdir():
        try:
            if path.joinpath("idVendor").read_text().strip() != VID:
                continue
            pid = int(path.joinpath("idProduct").read_text().strip(), 16)
            if pid not in selected:
                continue
            bus = int(path.joinpath("busnum").read_text())
            address = int(path.joinpath("devnum").read_text())
            devices.append((path.name, pid, f"/dev/bus/usb/{bus:03}/{address:03}"))
        except FileNotFoundError:
            continue  # Interface entries and devices removed during this snapshot.
    if len(devices) > 1:
        raise RuntimeError("Multiple selected V4 devices; refusing an ambiguous handoff")
    return devices[0] if devices else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=pathlib.Path,
                        help="Optionally follow a WinBoat-format passedThroughDevices selection")
    parser.add_argument("--container", default="WinBoat")
    parser.add_argument("--capture-dir", type=pathlib.Path,
                        help="Explicitly record guest USB traffic here for protocol investigation")
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(message)s")
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, lambda *_: STOP.set())
    attached = None
    owner = None
    error = None
    logging.info("Watching selected Wolverine V4 USB personalities")
    try:
        while not STOP.is_set():
            try:
                device = selected_device(args.config)
                info = json.loads(run("docker", "inspect", "--format", "{{json .State}}", args.container))
                current_owner = info["Pid"] if info["Running"] else None
                if current_owner != owner:
                    if current_owner:
                        monitor(args.container, [f"device_del {DEVICE_ID}"])
                    attached = None
                    owner = current_owner
                if device != attached and owner:
                    if attached:
                        monitor(args.container, [f"device_del {DEVICE_ID}"])
                        logging.info("Released %s to the host", attached[2])
                        attached = None
                    if device:
                        command = f"device_add usb-host,id={DEVICE_ID},hostdevice={device[2]}"
                        if args.capture_dir:
                            directory = args.capture_dir.expanduser().resolve()
                            # Only the shared home path is addressable inside this VM.
                            relative = directory.relative_to(pathlib.Path.home())
                            if any(c in str(relative) for c in ',\n\r'):
                                raise ValueError("Capture directory contains QEMU option separators")
                            directory.mkdir(parents=True, exist_ok=True, mode=0o700)
                            filename = f"{device[0]}-{device[1]:04x}-{time.time_ns()}.pcap"
                            capture = pathlib.PurePosixPath("/shared") / relative / filename
                            command += f",pcap={capture}"
                        monitor(args.container, [command])
                        attached = device
                        logging.info("Attached 1532:%04x at %s to %s", device[1], device[2], args.container)
                error = None
            except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.SubprocessError) as exc:
                message = str(exc)
                if message != error:
                    logging.warning("USB handoff: %s", message)
                    error = message
            STOP.wait(2)
    finally:
        if attached:
            try:
                monitor(args.container, [f"device_del {DEVICE_ID}"])
                logging.info("Released Wolverine to the host")
            except (RuntimeError, OSError, subprocess.SubprocessError) as exc:
                logging.warning("USB release: %s", exc)


if __name__ == "__main__":
    main()
