#!/usr/bin/env python3
"""Reference ingest server for the conversation-recorder firmware.

Receives START/AUDIO/END records over TCP, writes one 16-bit PCM WAV per
utterance, and acknowledges received sample counts so the device can resume
after a network loss.
"""

import argparse
import asyncio
import logging
import socket
import struct
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import BinaryIO, Dict, Optional, Tuple

HDR = struct.Struct("<IBBHI")  # magic, version, type, flags, length
START = struct.Struct("<IIIHHQQ16s")
END = struct.Struct("<IIB3xI")
ACK = struct.Struct("<II")

MAGIC = 0x43455243
VERSION = 1

TYPE_START = 1
TYPE_AUDIO = 2
TYPE_END = 3
TYPE_ACK = 0x81

FINAL_ACK_FLAG = 0x0001
FINALIZE_IDLE_SECONDS = 300.0
CLEANUP_INTERVAL_SECONDS = 60.0

log = logging.getLogger("ingest")


def wav_header(rate: int, channels: int, bits: int, data_bytes: int) -> bytes:
    block_align = channels * bits // 8
    byte_rate = rate * block_align
    return struct.pack(
        "<4sI4s4sIHHIIHH4sI",
        b"RIFF",
        36 + data_bytes,
        b"WAVE",
        b"fmt ",
        16,
        1,
        channels,
        rate,
        byte_rate,
        block_align,
        bits,
        b"data",
        data_bytes,
    )


@dataclass
class Device:
    device_id: str
    state: str
    queue_bytes: int
    uptime_ms: int
    address: Tuple[str, int]


def parse_beacon(data: bytes, address: Tuple[str, int]) -> Optional[Device]:
    try:
        text = data.decode("ascii", "replace").strip()
    except UnicodeDecodeError:
        return None
    parts = text.split()
    if len(parts) != 6 or parts[0] != "CREC/1" or parts[1] != "BEACON":
        return None
    try:
        queue_bytes = int(parts[4])
        uptime_ms = int(parts[5])
    except ValueError:
        return None
    return Device(
        device_id=parts[2],
        state=parts[3],
        queue_bytes=queue_bytes,
        uptime_ms=uptime_ms,
        address=address,
    )


def list_devices(discovery_port: int, timeout: float) -> list[Device]:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("", discovery_port))
    sock.settimeout(0.5)
    deadline = time.monotonic() + timeout
    devices: Dict[str, Device] = {}
    try:
        while time.monotonic() < deadline:
            try:
                data, address = sock.recvfrom(2048)
            except socket.timeout:
                continue
            device = parse_beacon(data, address)
            if device is not None:
                devices[device.device_id] = device
    finally:
        sock.close()
    return list(devices.values())


def _local_ip_for(remote_ip: str) -> str:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.connect((remote_ip, 1))
        return sock.getsockname()[0]
    finally:
        sock.close()


async def offer_loop(
    target: str,
    token: str,
    tcp_port: int,
    discovery_port: int,
    advertise_ip: Optional[str],
) -> None:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        sock.bind(("", discovery_port))
    except OSError as exc:
        log.error("discovery: cannot bind UDP %d: %s", discovery_port, exc)
        sock.close()
        return
    sock.setblocking(False)
    loop = asyncio.get_running_loop()
    log.info(
        "discovery: listening on UDP %d, offering to %s",
        discovery_port,
        target,
    )
    try:
        while True:
            data, address = await loop.sock_recvfrom(sock, 2048)
            device = parse_beacon(data, address)
            if device is None:
                continue
            if target not in ("any", device.device_id):
                continue
            host_ip = advertise_ip or _local_ip_for(address[0])
            offer = (
                f"CREC/1 OFFER {device.device_id} {host_ip} {tcp_port} {token}\n"
            ).encode("utf-8")
            try:
                await loop.sock_sendto(sock, offer, address)
            except OSError as exc:
                log.warning("discovery: offer to %s failed: %s", device.device_id, exc)
                continue
            log.info(
                "discovery: offered %s to %s:%d",
                device.device_id,
                host_ip,
                tcp_port,
            )
    finally:
        sock.close()


@dataclass
class Session:
    key: Tuple[str, int]
    path: Path
    file: BinaryIO
    rate: int
    channels: int
    bits: int
    received: int = 0
    last_activity: float = field(default_factory=time.monotonic)
    active: bool = True
    finalized: bool = False

    @property
    def sample_bytes(self) -> int:
        return self.channels * self.bits // 8


class IngestServer:
    def __init__(self, out_dir: Path):
        self.out_dir = out_dir
        self.out_dir.mkdir(parents=True, exist_ok=True)
        self.sessions: Dict[Tuple[str, int], Session] = {}
        self.server: Optional[asyncio.AbstractServer] = None
        self._cleanup_task: Optional[asyncio.Task] = None

    async def start(self, host: str = "127.0.0.1", port: int = 0) -> int:
        self.server = await asyncio.start_server(self.handle, host, port)
        self._cleanup_task = asyncio.create_task(self._cleanup_loop())
        sock = self.server.sockets[0]
        return sock.getsockname()[1]

    async def stop(self) -> None:
        if self._cleanup_task is not None:
            self._cleanup_task.cancel()
            try:
                await self._cleanup_task
            except asyncio.CancelledError:
                pass
            self._cleanup_task = None
        if self.server is not None:
            self.server.close()
            await self.server.wait_closed()
            self.server = None
        for session in list(self.sessions.values()):
            if not session.finalized:
                self._finalize(session)

    async def _cleanup_loop(self) -> None:
        while True:
            await asyncio.sleep(CLEANUP_INTERVAL_SECONDS)
            now = time.monotonic()
            for session in list(self.sessions.values()):
                if (
                    not session.finalized
                    and not session.active
                    and now - session.last_activity > FINALIZE_IDLE_SECONDS
                ):
                    session.last_activity = now
                    self._finalize(session)

    def _finalize(self, session: Session) -> None:
        if session.finalized:
            return
        data_bytes = session.received * session.sample_bytes
        try:
            session.file.seek(0)
            session.file.write(
                wav_header(session.rate, session.channels, session.bits, data_bytes)
            )
            session.file.flush()
            session.file.close()
        except OSError as exc:
            log.error("finalize %s failed: %s", session.path, exc)
        session.active = False
        session.finalized = True
        duration = session.received / session.rate if session.rate else 0.0
        log.info("finalized %s (%.2f s, %d samples)", session.path, duration, session.received)
        print(f"{session.path} ({duration:.2f} s)")

    async def _send_ack(
        self,
        writer: asyncio.StreamWriter,
        session_id: int,
        acked_samples: int,
        final: bool = False,
    ) -> None:
        flags = FINAL_ACK_FLAG if final else 0
        writer.write(
            HDR.pack(MAGIC, VERSION, TYPE_ACK, flags, ACK.size)
            + ACK.pack(session_id, acked_samples)
        )
        await writer.drain()

    async def handle(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        peer = writer.get_extra_info("peername")
        log.info("connection from %s", peer)
        current: Optional[Session] = None
        try:
            while True:
                raw_hdr = await reader.readexactly(HDR.size)
                magic, version, typ, flags, length = HDR.unpack(raw_hdr)
                if magic != MAGIC or version != VERSION:
                    log.error("bad header from %s: magic=%08x version=%d", peer, magic, version)
                    break
                payload = await reader.readexactly(length) if length else b""

                if typ == TYPE_START:
                    if len(payload) != START.size:
                        log.error("bad START length %d", len(payload))
                        break
                    (
                        session_id,
                        first_sample,
                        rate,
                        channels,
                        bits,
                        start_unix_ms,
                        device_uptime_ms,
                        device_id_raw,
                    ) = START.unpack(payload)
                    device = device_id_raw.split(b"\0", 1)[0].decode("ascii", "replace")
                    key = (device, session_id)
                    log.info(
                        "START %s session=%08x first=%d rate=%d ch=%d bits=%d uptime=%d",
                        device,
                        session_id,
                        first_sample,
                        rate,
                        channels,
                        bits,
                        device_uptime_ms,
                    )

                    if (
                        current is not None
                        and current.key != key
                        and current.active
                        and not current.finalized
                    ):
                        self._finalize(current)
                        current = None

                    existing = self.sessions.get(key)
                    if existing is not None and existing.finalized:
                        log.info("START for finalized session %s", key)
                        await self._send_ack(
                            writer, session_id, existing.received, final=True
                        )
                        current = None
                        continue
                    if existing is not None and existing.active:
                        log.error("START for already-active session %s", key)
                        break

                    if existing is None:
                        if first_sample != 0:
                            log.error(
                                "reject START gap for new session %s: first=%d",
                                key,
                                first_sample,
                            )
                            break
                        stamp = time.strftime("%Y%m%d-%H%M%S")
                        path = self.out_dir / f"{stamp}_{device}_{session_id:08x}.wav"
                        file = open(path, "r+b") if path.exists() else open(path, "wb+")
                        file.write(b"\0" * 44)
                        file.flush()
                        session = Session(
                            key=key,
                            path=path,
                            file=file,
                            rate=rate,
                            channels=channels,
                            bits=bits,
                            received=0,
                            active=True,
                        )
                        self.sessions[key] = session
                        log.info("new recording %s", path)
                    else:
                        if first_sample > existing.received:
                            log.error(
                                "reject START resume gap for %s: first=%d received=%d",
                                key,
                                first_sample,
                                existing.received,
                            )
                            break
                        offset = 44 + first_sample * existing.sample_bytes
                        existing.file.seek(offset)
                        existing.file.truncate(offset)
                        existing.received = first_sample
                        existing.active = True
                        existing.last_activity = time.monotonic()
                        session = existing
                        log.info(
                            "resume %s at sample %d", session.path, first_sample
                        )

                    current = session
                    await self._send_ack(writer, session_id, session.received)

                elif typ == TYPE_AUDIO:
                    if current is None or current.finalized:
                        log.error("AUDIO without active session from %s", peer)
                        break
                    if not current.active:
                        log.error("AUDIO for inactive session %s", current.key)
                        break
                    if len(payload) % current.sample_bytes != 0:
                        log.warning("AUDIO payload not sample-aligned: %d bytes", len(payload))
                    current.file.write(payload)
                    current.file.flush()
                    current.received += len(payload) // current.sample_bytes
                    current.last_activity = time.monotonic()
                    await self._send_ack(writer, current.key[1], current.received)

                elif typ == TYPE_END:
                    if len(payload) != END.size:
                        log.error("bad END length %d", len(payload))
                        break
                    session_id, total_samples, reason, _crc = END.unpack(payload)
                    log.info(
                        "END session=%08x total=%d reason=%d",
                        session_id,
                        total_samples,
                        reason,
                    )
                    session = None
                    if current is not None and current.key[1] == session_id:
                        session = current
                    if session is None:
                        for candidate in self.sessions.values():
                            if candidate.key[1] == session_id:
                                session = candidate
                                break
                    if session is None:
                        log.error("END for unknown session %08x", session_id)
                        continue
                    if session.finalized:
                        await self._send_ack(
                            writer, session_id, session.received, final=True
                        )
                        continue
                    if session.received != total_samples:
                        log.warning(
                            "END total %d disagrees with received %d",
                            total_samples,
                            session.received,
                        )
                    self._finalize(session)
                    await self._send_ack(
                        writer, session_id, session.received, final=True
                    )
                    current = session

                else:
                    log.error("unsupported record type 0x%02x", typ)
                    break
        except asyncio.IncompleteReadError:
            log.info("connection %s closed", peer)
        except (ConnectionError, OSError) as exc:
            log.info("connection %s error: %s", peer, exc)
        finally:
            if current is not None and current.active:
                current.active = False
                current.last_activity = time.monotonic()
            writer.close()
            try:
                await writer.wait_closed()
            except (ConnectionError, OSError):
                pass


async def _list_devices(discovery_port: int, timeout: float) -> None:
    devices = await asyncio.to_thread(list_devices, discovery_port, timeout)
    if not devices:
        print("no devices found")
        return
    for device in devices:
        print(
            f"{device.device_id}  state={device.state}  "
            f"queue={device.queue_bytes} B  uptime={device.uptime_ms // 1000}s  "
            f"address={device.address[0]}"
        )


async def _run(
    host: str,
    port: int,
    out_dir: Path,
    auto_accept: Optional[str],
    token: str,
    discovery_port: int,
    advertise_ip: Optional[str],
) -> None:
    server = IngestServer(out_dir)
    bound = await server.start(host, port)
    log.info("listening on %s:%d, writing to %s", host, bound, out_dir)
    offer_task: Optional[asyncio.Task] = None
    if auto_accept is not None:
        offer_task = asyncio.create_task(
            offer_loop(
                auto_accept,
                token,
                bound,
                discovery_port,
                advertise_ip,
            )
        )
    try:
        await asyncio.Event().wait()
    finally:
        if offer_task is not None:
            offer_task.cancel()
            try:
                await offer_task
            except asyncio.CancelledError:
                pass
        await server.stop()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", default=8000, type=int)
    parser.add_argument("--out", default="./recordings", type=Path)
    parser.add_argument(
        "--auto-accept",
        nargs="?",
        const="any",
        default="any",
        help="discover devices and offer; 'any' accepts all (default: any)",
    )
    parser.add_argument(
        "--no-discovery",
        action="store_true",
        help="disable discovery and wait for a static device",
    )
    parser.add_argument(
        "--list-devices",
        action="store_true",
        help="list discovered devices and exit",
    )
    parser.add_argument("--discover-timeout", default=5.0, type=float)
    parser.add_argument("--discovery-port", default=8001, type=int)
    parser.add_argument("--token", default="")
    parser.add_argument(
        "--advertise-ip",
        default=None,
        help="IP address to put in offers (default: auto-detect)",
    )
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
    )
    try:
        if args.list_devices:
            asyncio.run(_list_devices(args.discovery_port, args.discover_timeout))
            return
        auto_accept = None if args.no_discovery else args.auto_accept
        if auto_accept is not None and not args.token:
            parser.error(
                "--token is required for discovery; "
                "set it to the device PAIRING_TOKEN"
            )
        asyncio.run(
            _run(
                args.host,
                args.port,
                args.out,
                auto_accept,
                args.token,
                args.discovery_port,
                args.advertise_ip,
            )
        )
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
