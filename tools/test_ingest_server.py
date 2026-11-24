#!/usr/bin/env python3
"""Integration tests for tools/ingest_server.py."""

import asyncio
import math
import socket
import struct
import tempfile
import unittest
import wave
from pathlib import Path

import ingest_server as srv


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


if __name__ == "__main__":
    unittest.main()
