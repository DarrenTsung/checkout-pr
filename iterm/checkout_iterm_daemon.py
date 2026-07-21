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


def find_session(app, session_ids, session_name="", legacy_prefix="", aliases=None):
    for value in session_ids:
        if not isinstance(value, str) or not value:
            continue
        session = app.get_session_by_id(value)
        if session is not None:
            return session

    if session_name and aliases and session_name in aliases:
        session = app.get_session_by_id(aliases[session_name])
        if session is not None:
            return session
        del aliases[session_name]

    if not session_name and not legacy_prefix:
        return None
    for window in app.terminal_windows:
        for tab in window.tabs:
            for session in tab.sessions:
                if session.name == session_name or (
                    legacy_prefix and session.name.startswith(legacy_prefix)
                ):
                    return session
    return None


async def activate_session(app, session):
    await session.async_activate(select_tab=True, order_window_front=True)
    await app.async_activate(raise_all_windows=False)


async def open_session(connection, app, session_name, launch_command, focus=True):
    window = app.current_terminal_window
    if window is None:
        if not focus:
            raise RuntimeError("cannot create a background session without an iTerm window")
        window = await iterm2.Window.async_create(connection)
        if window is None:
            raise RuntimeError("iTerm did not create a window")
        session = window.current_tab.current_session
    else:
        tab = await window.async_create_tab()
        if tab is None:
            raise RuntimeError("iTerm did not create a tab")
        session = tab.current_session

    await session.async_set_name(session_name)
    await session.async_send_text(launch_command + "\n")
    if focus:
        await activate_session(app, session)
    return session


async def snapshot_sessions(app):
    sessions = []
    for window in app.terminal_windows:
        for tab in window.tabs:
            for session in tab.sessions:
                sessions.append(
                    {
                        "sessionId": session.session_id,
                        "name": session.name,
                    }
                )
    focused = None
    window = app.current_terminal_window
    if window is not None and window.current_tab is not None:
        current = window.current_tab.current_session
        if current is not None:
            focused = current.session_id
    return sessions, focused


async def handle_request(reader, writer, connection, app, aliases, open_locks):
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
        if action == "snapshot":
            sessions, focused = await snapshot_sessions(app)
            writer.write(
                response(
                    ok=True,
                    sessions=sessions,
                    focusedSessionId=focused,
                    applicationActive=bool(app.app_active),
                    elapsedMs=round((time.perf_counter() - started) * 1_000, 2),
                )
            )
            await writer.drain()
            return
        if action not in {"status", "focus", "open", "rename", "close"}:
            raise ValueError("unsupported action")

        session_ids = request.get("sessionIds", [])
        if not isinstance(session_ids, list) or len(session_ids) > 10:
            raise ValueError("sessionIds must be a short list")
        session_name = request.get("sessionName", "")
        legacy_prefix = request.get("legacyPrefix", "")
        if (
            not isinstance(session_name, str)
            or len(session_name) > 500
            or not isinstance(legacy_prefix, str)
            or len(legacy_prefix) > 500
        ):
            raise ValueError("session names must be strings")

        if action == "open":
            if not session_name:
                raise ValueError("sessionName is required")
            focus = request.get("focus", True)
            if not isinstance(focus, bool):
                raise ValueError("focus must be a boolean")
            async with open_locks.setdefault(session_name, asyncio.Lock()):
                session = find_session(
                    app, session_ids, session_name, legacy_prefix, aliases
                )
                if session is None:
                    launch_command = request.get("launchCommand")
                    if not isinstance(launch_command, str) or not launch_command:
                        raise ValueError("launchCommand is required")
                    prior_session = None
                    if not focus and app.current_terminal_window is not None:
                        prior_session = app.current_terminal_window.current_tab.current_session
                    session = await open_session(
                        connection, app, session_name, launch_command, focus=focus
                    )
                    if not focus and prior_session is not None:
                        await prior_session.async_activate(
                            select_tab=True, order_window_front=False
                        )
                    result_action = "opened"
                else:
                    if focus:
                        await activate_session(app, session)
                        result_action = "focused"
                    else:
                        result_action = "existing"
                aliases[session_name] = session.session_id
            writer.write(
                response(
                    ok=True,
                    exists=True,
                    sessionId=session.session_id,
                    action=result_action,
                    elapsedMs=round((time.perf_counter() - started) * 1_000, 2),
                )
            )
            await writer.drain()
            return

        session = find_session(
            app, session_ids, session_name, legacy_prefix, aliases
        )

        if action == "rename":
            title = request.get("title")
            if not isinstance(title, str) or len(title) > 500:
                raise ValueError("title must be a short string")
            if session is None:
                writer.write(response(ok=True, exists=False))
                await writer.drain()
                return
            await session.async_set_name(title)
            writer.write(response(ok=True, exists=True, sessionId=session.session_id))
            await writer.drain()
            return

        if action == "close":
            if session is None:
                writer.write(response(ok=True, exists=False))
                await writer.drain()
                return
            session_id = session.session_id
            await session.async_close(force=False)
            for alias, aliased_session_id in list(aliases.items()):
                if aliased_session_id == session_id:
                    del aliases[alias]
            writer.write(response(ok=True, exists=True, sessionId=session_id))
            await writer.drain()
            return

        if session is None:
            writer.write(
                response(
                    ok=True,
                    exists=False,
                    elapsedMs=round((time.perf_counter() - started) * 1_000, 2),
                )
            )
            await writer.drain()
            return

        if action == "focus":
            await activate_session(app, session)
        writer.write(
            response(
                ok=True,
                exists=True,
                sessionId=session.session_id,
                elapsedMs=round((time.perf_counter() - started) * 1_000, 2),
            )
        )
        await writer.drain()
    except Exception as error:
        try:
            writer.write(response(ok=False, error=str(error)[:500]))
            await writer.drain()
        except (BrokenPipeError, ConnectionResetError):
            pass
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except (BrokenPipeError, ConnectionResetError):
            pass


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
    aliases = {}
    open_locks = {}
    server = await asyncio.start_unix_server(
        lambda reader, writer: handle_request(
            reader, writer, connection, app, aliases, open_locks
        ),
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
