#!/usr/bin/env python3

import asyncio
import fcntl
import json
import time
from pathlib import Path

import iterm2


STATE_DIR = Path.home() / ".local" / "share" / "checkout"
SOCKET_PATH = STATE_DIR / "iterm-api.sock"
LOCK_PATH = STATE_DIR / "iterm-api.lock"
MAX_REQUEST_BYTES = 8_192


def response(**values):
    return json.dumps(values, separators=(",", ":")).encode() + b"\n"


def find_session(app, session_ids):
    for value in session_ids:
        if not isinstance(value, str) or not value:
            continue
        session = app.get_session_by_id(value)
        if session is not None:
            return session
    return None


async def handle_request(reader, writer, app):
    started = time.perf_counter()
    try:
        line = await asyncio.wait_for(reader.readline(), timeout=1)
        if not line or len(line) > MAX_REQUEST_BYTES:
            raise ValueError("invalid request size")
        request = json.loads(line)
        action = request.get("action")
        if action == "ping":
            writer.write(response(ok=True))
            await writer.drain()
            return
        if action not in {"status", "focus"}:
            raise ValueError("unsupported action")

        session_ids = request.get("sessionIds")
        if not isinstance(session_ids, list) or not session_ids or len(session_ids) > 10:
            raise ValueError("sessionIds must be a short list")
        session = find_session(app, session_ids)
        if session is None:
            writer.write(response(ok=True, exists=False, elapsedMs=round((time.perf_counter() - started) * 1_000, 2)))
            await writer.drain()
            return

        if action == "focus":
            await session.async_activate(select_tab=True, order_window_front=True)
            await app.async_activate(raise_all_windows=False)
        writer.write(response(
            ok=True,
            exists=True,
            sessionId=session.session_id,
            elapsedMs=round((time.perf_counter() - started) * 1_000, 2),
        ))
        await writer.drain()
    except Exception as error:
        writer.write(response(ok=False, error=str(error)[:500]))
        await writer.drain()
    finally:
        writer.close()
        await writer.wait_closed()


async def main(connection):
    STATE_DIR.mkdir(mode=0o700, parents=True, exist_ok=True)
    lock = LOCK_PATH.open("w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        lock.close()
        raise SystemExit(0)

    SOCKET_PATH.unlink(missing_ok=True)
    app = await iterm2.async_get_app(connection)
    server = await asyncio.start_unix_server(
        lambda reader, writer: handle_request(reader, writer, app),
        path=SOCKET_PATH,
    )
    SOCKET_PATH.chmod(0o600)
    try:
        async with server:
            await server.serve_forever()
    finally:
        SOCKET_PATH.unlink(missing_ok=True)
        lock.close()


if __name__ == "__main__":
    iterm2.run_forever(main, retry=True)
