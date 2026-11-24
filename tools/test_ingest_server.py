#!/usr/bin/env python3
"""Integration tests for tools/ingest_server.py."""

import asyncio
import math
import os
import select
import socket
import struct
import tempfile
import threading
import time
import unittest
import wave
from pathlib import Path

import ingest_server as srv


def _read_exact_fd(fd: int, size: int, timeout: float = 5.0) -> bytes:
    buf = b""
    deadline = time.monotonic() + timeout
    while len(buf) < size:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"wanted {size} bytes, got {len(buf)}")
        ready, _, _ = select.select([fd], [], [], remaining)
        if not ready:
            raise TimeoutError(f"wanted {size} bytes, got {len(buf)}")
        chunk = os.read(fd, size - len(buf))
        if not chunk:
            raise EOFError("pty closed")
        buf += chunk
    return buf


def free_udp_port() -> int:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


def pcm_440(count: int) -> bytes:
    out = bytearray()
    for i in range(count):
        sample = int(32767 * 0.5 * math.sin(2 * math.pi * 440 * i / 16000))
        out += struct.pack("<h", sample)
    return bytes(out)


async def send_start(writer, session_id: int, first: int = 0, device: bytes = b"TESTDEVICE"):
    payload = srv.START.pack(
        session_id, first, 16000, 1, 16, 0, 0, device.ljust(16, b"\0")
    )
    writer.write(
        srv.HDR.pack(srv.MAGIC, srv.VERSION, srv.TYPE_START, 0, len(payload)) + payload
    )
    await writer.drain()


async def send_audio(writer, payload: bytes):
    writer.write(
        srv.HDR.pack(srv.MAGIC, srv.VERSION, srv.TYPE_AUDIO, 0, len(payload)) + payload
    )
    await writer.drain()


async def send_end(writer, session_id: int, total: int, reason: int = 0):
    payload = srv.END.pack(session_id, total, reason, 0)
    writer.write(
        srv.HDR.pack(srv.MAGIC, srv.VERSION, srv.TYPE_END, 0, len(payload)) + payload
    )
    await writer.drain()


async def read_record(reader):
    header = await asyncio.wait_for(reader.readexactly(srv.HDR.size), 5)
    magic, version, typ, flags, length = srv.HDR.unpack(header)
    assert magic == srv.MAGIC
    assert version == srv.VERSION
    payload = await asyncio.wait_for(reader.readexactly(length), 5) if length else b""
    return typ, flags, payload


async def read_ack(reader):
    typ, flags, payload = await read_record(reader)
    assert typ == srv.TYPE_ACK
    session_id, acked = srv.ACK.unpack(payload)
    return session_id, acked, flags


class IngestTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.out = Path(self.tmp.name)
        self.server = srv.IngestServer(self.out)
        self.port = await self.server.start()

    async def asyncTearDown(self):
        await self.server.stop()
        self.tmp.cleanup()

    async def connect(self):
        return await asyncio.open_connection("127.0.0.1", self.port)

    async def test_basic_session(self):
        reader, writer = await self.connect()
        sid = 0x1234ABCD
        await send_start(writer, sid)
        assert await read_ack(reader) == (sid, 0, 0)

        pcm = pcm_440(16000)
        for offset in range(0, len(pcm), 3200):
            chunk = pcm[offset : offset + 3200]
            await send_audio(writer, chunk)
            acked = (offset + len(chunk)) // 2
            assert await read_ack(reader) == (sid, acked, 0)

        await send_end(writer, sid, 16000)
        assert await read_ack(reader) == (sid, 16000, srv.FINAL_ACK_FLAG)

        writer.close()
        await writer.wait_closed()

        files = list(self.out.glob("*.wav"))
        self.assertEqual(len(files), 1)
        with wave.open(str(files[0]), "rb") as wf:
            self.assertEqual(wf.getframerate(), 16000)
            self.assertEqual(wf.getnchannels(), 1)
            self.assertEqual(wf.getsampwidth(), 2)
            self.assertEqual(wf.getnframes(), 16000)
            self.assertEqual(wf.readframes(16000), pcm)

    async def test_resume(self):
        reader, writer = await self.connect()
        sid = 0x0BADF00D
        pcm = pcm_440(16000)

        await send_start(writer, sid)
        assert await read_ack(reader) == (sid, 0, 0)

        sent = 0
        while sent < 8000:
            chunk = pcm[sent * 2 : (sent + 1600) * 2]
            await send_audio(writer, chunk)
            sent += 1600
            assert await read_ack(reader) == (sid, sent, 0)

        writer.close()
        await writer.wait_closed()
        await asyncio.sleep(0.2)

        reader, writer = await self.connect()
        await send_start(writer, sid, first=8000)
        assert await read_ack(reader) == (sid, 8000, 0)

        while sent < 16000:
            chunk = pcm[sent * 2 : (sent + 1600) * 2]
            await send_audio(writer, chunk)
            sent += 1600
            assert await read_ack(reader) == (sid, sent, 0)

        await send_end(writer, sid, 16000)
        assert await read_ack(reader) == (sid, 16000, srv.FINAL_ACK_FLAG)

        writer.close()
        await writer.wait_closed()

        files = list(self.out.glob("*.wav"))
        self.assertEqual(len(files), 1)
        with wave.open(str(files[0]), "rb") as wf:
            self.assertEqual(wf.getnframes(), 16000)
            self.assertEqual(wf.readframes(16000), pcm)

    async def test_reject_gap(self):
        reader, writer = await self.connect()
        await send_start(writer, 0xDEADBEEF, first=1000)
        data = await asyncio.wait_for(reader.read(1), 2)
        self.assertEqual(data, b"")
        writer.close()
        await writer.wait_closed()
        await asyncio.sleep(0.1)
        self.assertEqual(list(self.out.glob("*.wav")), [])

    async def test_discovery_list(self):
        port = free_udp_port()
        sender = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sender.bind(("127.0.0.1", 0))
        beacon = b"CREC/1 BEACON ABC123 rec 1024 5000\n"

        async def send():
            for _ in range(3):
                sender.sendto(beacon, ("127.0.0.1", port))
                await asyncio.sleep(0.05)

        task = asyncio.create_task(send())
        devices = await asyncio.wait_for(
            asyncio.to_thread(srv.list_devices, port, 1.0), 3
        )
        await task
        sender.close()
        self.assertEqual(len(devices), 1)
        self.assertEqual(devices[0].device_id, "ABC123")
        self.assertEqual(devices[0].state, "rec")
        self.assertEqual(devices[0].queue_bytes, 1024)

    async def test_discovery_offer(self):
        tcp_port = self.port
        discovery_port = free_udp_port()
        task = asyncio.create_task(
            srv.offer_loop("any", "secret", tcp_port, discovery_port, "127.0.0.1")
        )
        await asyncio.sleep(0.1)

        sender = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sender.bind(("127.0.0.1", 0))
        sender.settimeout(2)
        sender.sendto(
            b"CREC/1 BEACON ABC123 idle 0 1000\n",
            ("127.0.0.1", discovery_port),
        )
        data, _ = await asyncio.to_thread(sender.recvfrom, 2048)
        sender.close()
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass

        text = data.decode("ascii").strip()
        self.assertEqual(text, f"CREC/1 OFFER ABC123 127.0.0.1 {tcp_port} secret")


class SerialTests(unittest.IsolatedAsyncioTestCase):
    """USB mode: records over a tty, console text interleaved, ACKs back."""

    async def asyncSetUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.out = Path(self.tmp.name)
        self.server = srv.IngestServer(self.out)
        self.server.start_cleanup()
        self.master, slave = os.openpty()
        self.path = os.ttyname(slave)
        os.close(slave)
        self.link = None
        await self.open_link()

    async def asyncTearDown(self):
        await self.close_link()
        await self.server.stop()
        try:
            os.close(self.master)
        except OSError:
            pass
        self.tmp.cleanup()

    async def open_link(self):
        fd = srv.open_serial(self.path)
        reader = asyncio.StreamReader()
        stop = threading.Event()
        thread = threading.Thread(
            target=srv._pump,
            args=(fd, asyncio.get_running_loop(), reader, stop),
            daemon=True,
        )
        thread.start()
        task = asyncio.create_task(
            self.server.handle(reader, srv.SerialBytes(fd, self.path), resync=True)
        )
        self.link = (fd, stop, thread, task)

    async def close_link(self):
        if self.link is None:
            return
        fd, stop, thread, task = self.link
        self.link = None
        stop.set()
        for _ in range(100):
            if not thread.is_alive():
                break
            await asyncio.sleep(0.01)
        try:
            await asyncio.wait_for(asyncio.shield(task), 2.0)
        except (asyncio.TimeoutError, asyncio.CancelledError):
            pass
        os.close(fd)

    def send(self, typ: int, payload: bytes) -> None:
        os.write(
            self.master,
            srv.HDR.pack(srv.MAGIC, srv.VERSION, typ, 0, len(payload)) + payload,
        )

    def send_text(self, text: bytes) -> None:
        os.write(self.master, text)

    async def read_ack(self):
        raw = await asyncio.to_thread(_read_exact_fd, self.master, srv.HDR.size + srv.ACK.size)
        magic, version, typ, flags, length = srv.HDR.unpack(raw[: srv.HDR.size])
        assert (magic, version, typ, length) == (
            srv.MAGIC,
            srv.VERSION,
            srv.TYPE_ACK,
            srv.ACK.size,
        )
        session_id, acked = srv.ACK.unpack(raw[srv.HDR.size :])
        return session_id, acked, flags

    def start_payload(self, session_id: int, first: int = 0) -> bytes:
        return srv.START.pack(
            session_id, first, 16000, 1, 16, 0, 0, b"TESTDEVICE".ljust(16, b"\0")
        )

    async def test_session(self):
        sid = 0x51A1
        pcm = pcm_440(16000)
        self.send(srv.TYPE_START, self.start_payload(sid))
        self.assertEqual(await self.read_ack(), (sid, 0, 0))

        for offset in range(0, len(pcm), 3200):
            chunk = pcm[offset : offset + 3200]
            self.send(srv.TYPE_AUDIO, chunk)
            acked = (offset + len(chunk)) // 2
            self.assertEqual(await self.read_ack(), (sid, acked, 0))

        self.send(srv.TYPE_END, srv.END.pack(sid, 16000, 0, 0))
        self.assertEqual(
            await self.read_ack(), (sid, 16000, srv.FINAL_ACK_FLAG)
        )

        files = list(self.out.glob("*.wav"))
        self.assertEqual(len(files), 1)
        with wave.open(str(files[0]), "rb") as wf:
            self.assertEqual(wf.getnframes(), 16000)
            self.assertEqual(wf.readframes(16000), pcm)

    async def test_resync_over_console_text(self):
        sid = 0x51A2
        pcm = pcm_440(1600)
        self.send_text(b"I (312) app: boot\r\nI (313) usb: host attached\r\n")
        self.send(srv.TYPE_START, self.start_payload(sid))
        self.assertEqual(await self.read_ack(), (sid, 0, 0))
        self.send_text(b"W (900) uploader: a noisy log line\r\n")
        self.send(srv.TYPE_AUDIO, pcm)
        self.assertEqual(await self.read_ack(), (sid, 1600, 0))
        self.send(srv.TYPE_END, srv.END.pack(sid, 1600, 0, 0))
        self.assertEqual(await self.read_ack(), (sid, 1600, srv.FINAL_ACK_FLAG))

        files = list(self.out.glob("*.wav"))
        self.assertEqual(len(files), 1)
        with wave.open(str(files[0]), "rb") as wf:
            self.assertEqual(wf.getnframes(), 1600)
            self.assertEqual(wf.readframes(1600), pcm)

    async def test_resume_after_link_drop(self):
        sid = 0x51A3
        pcm = pcm_440(16000)
        self.send(srv.TYPE_START, self.start_payload(sid))
        self.assertEqual(await self.read_ack(), (sid, 0, 0))
        for offset in range(0, 8000 * 2, 3200):
            chunk = pcm[offset : offset + 3200]
            self.send(srv.TYPE_AUDIO, chunk)
            self.assertEqual(
                await self.read_ack(), (sid, (offset + len(chunk)) // 2, 0)
            )

        await self.close_link()
        await self.open_link()

        self.send(srv.TYPE_START, self.start_payload(sid, first=8000))
        self.assertEqual(await self.read_ack(), (sid, 8000, 0))
        for offset in range(8000 * 2, len(pcm), 3200):
            chunk = pcm[offset : offset + 3200]
            self.send(srv.TYPE_AUDIO, chunk)
            self.assertEqual(
                await self.read_ack(), (sid, (offset + len(chunk)) // 2, 0)
            )
        self.send(srv.TYPE_END, srv.END.pack(sid, 16000, 0, 0))
        self.assertEqual(
            await self.read_ack(), (sid, 16000, srv.FINAL_ACK_FLAG)
        )

        files = list(self.out.glob("*.wav"))
        self.assertEqual(len(files), 1)
        with wave.open(str(files[0]), "rb") as wf:
            self.assertEqual(wf.getnframes(), 16000)
            self.assertEqual(wf.readframes(16000), pcm)


if __name__ == "__main__":
    unittest.main()
