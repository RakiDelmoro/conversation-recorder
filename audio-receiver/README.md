# Audio receiver

Host-side receiver for the XIAO firmware. It reads `CREC` records from the
USB CDC port, writes one 16 kHz mono WAV per segment, and ACKs received
samples so the firmware can free its PSRAM queue.

All receiving-machine code lives here:

| File | Purpose |
|---|---|
| `receiver.py` | Serial reader, CREC framing, WAV writer, ACK sender |
| `test_receiver.py` | PTY integration tests |
| `requirements.txt` | `pyserial` for Windows |
| `README.md` | This file |

## Windows

```powershell
cd audio-receiver
python -m pip install -r requirements.txt
python receiver.py --serial COM3 --out recordings
```

Close `espflash monitor` first: only one process can own `COM3`.

## Linux / WSL

```sh
cd audio-receiver
python3 receiver.py --serial /dev/ttyACM0 --out recordings
```

Under WSL, attach the XIAO first:

```powershell
usbipd attach --wsl --busid <id>
```

## Container

Everything the receiver needs is in this folder: image, dependency, code,
tests, and output directory.

```sh
cd audio-receiver
docker compose up --build
```

The compose file passes `/dev/ttyACM0` into the container and mounts
`./recordings` so the WAVs are visible on the host filesystem. The receiver
process itself runs only in the container; no Python or pyserial install is
needed on the host.

`/dev/ttyACM0` must exist on the Docker host. On Docker Desktop for Windows,
a USB device attached to Windows is not automatically visible in Docker's
Linux VM; use Docker Engine inside the WSL distro where `usbipd` attached the
XIAO, or Docker Desktop's USB/IP support. Without the device in the
container's kernel, no userspace program can read it.

If the device is not available inside the container, run the receiver on the
host/WSL instead and bind-mount `audio-receiver/` so the WAVs appear here.

## Firmware requirement

Set `LOG_ONLY = false` in `../src/config.rs`, rebuild, and flash. With
`LOG_ONLY = true`, the firmware only logs candidate decisions and simulated
`START/AUDIO/END/ACK`; no audio records are sent.

## Output

Each completed segment becomes:

```text
audio-receiver/recordings/YYYYMMDD-HHMMSS_<device>_<session>.wav
```

`--out` defaults to `audio-receiver/recordings` regardless of the current
directory. The directory is created automatically.

The receiver also sends ACKs over the same port. Without a running receiver,
the firmware queue fills after roughly 128 seconds and the segment ends with
`reason 1`.

## Tests

```sh
cd audio-receiver
python3 test_receiver.py -v
```

The tests use a PTY, so they run on Linux/WSL, not native Windows.

## Troubleshooting

- `SerialException` / access denied: stop `espflash monitor` and any other
  serial terminal.
- No `COM3`: check `Get-CimInstance Win32_SerialPort`.
- No WAV: confirm `LOG_ONLY = false` and look for `session ... started` in
  the firmware log.
- `pyserial` missing: `python -m pip install -r requirements.txt`.
