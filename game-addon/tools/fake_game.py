#!/usr/bin/env python3
"""A stand-in game speaking the Android Controller debug add-on protocol v1
(ACaddon.md), for checking the PC side without a real game.

Run it on the PC and route the phone's port to it:
    adb -s <phone> reverse tcp:27191 tcp:27191
    python3 game-addon/tools/fake_game.py
Android Controller then sees a game on that phone (game_info, game_state,
game_command, game_logs). Commands: give_coins {amount}, load_level {level},
throw (reports an exception), echo {any}.
"""
import json
import socket
import sys
import threading
import time
import traceback

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 27191
state = {"scene": "Menu", "level": 0, "coins": 0, "paused": False}
lock = threading.Lock()

HELLO = {
    "type": "hello", "protocol": 1, "game": "Fake Game", "version": "0.1.0", "engine": "python",
    "package": "com.example.fakegame",
    "commands": [
        {"name": "give_coins", "args": {"amount": "int"}, "help": "Add coins to the wallet"},
        {"name": "load_level", "args": {"level": "int"}, "help": "Load a level by number"},
        {"name": "throw", "args": {}, "help": "Raise an exception (tests crash reporting)"},
        {"name": "echo", "args": {"any": "any"}, "help": "Returns its arguments"},
    ],
}


def send(conn, obj):
    conn.sendall((json.dumps(obj) + "\n").encode())


def command(conn, name, args):
    with lock:
        if name == "give_coins":
            state["coins"] += int(args.get("amount", 1))
            send(conn, {"type": "event", "name": "coins_changed", "data": {"coins": state["coins"]}})
            return {"coins": state["coins"]}
        if name == "load_level":
            level = int(args.get("level", 1))
            if level < 1 or level > 20:
                raise ValueError(f"no level {level} (1-20)")
            state.update(scene=f"Level{level}", level=level)
            send(conn, {"type": "log", "level": "info", "message": f"loaded level {level}"})
            return {"scene": state["scene"]}
        if name == "throw":
            try:
                raise RuntimeError("boom from the fake game")
            except RuntimeError as e:
                send(conn, {"type": "log", "level": "exception", "message": f"{type(e).__name__}: {e}", "stack": traceback.format_exc()})
            return "thrown"
        if name == "echo":
            return args
    raise KeyError(f"unknown command {name}")


def serve(conn):
    send(conn, HELLO)
    stop = threading.Event()

    def ticker():
        n = 0
        while not stop.wait(5):
            n += 1
            try:
                send(conn, {"type": "log", "level": "debug", "message": f"frame tick {n}"})
            except OSError:
                return

    threading.Thread(target=ticker, daemon=True).start()
    buf = b""
    try:
        while True:
            data = conn.recv(65536)
            if not data:
                break
            buf += data
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                if not line.strip():
                    continue
                msg = json.loads(line)
                rid, kind = msg.get("id"), msg.get("type")
                try:
                    if kind == "state":
                        with lock:
                            result = dict(state)
                    elif kind == "command":
                        result = command(conn, msg.get("name"), msg.get("args") or {})
                    elif kind == "ping":
                        result = "pong"
                    else:
                        raise KeyError(f"unknown request type {kind}")
                    send(conn, {"id": rid, "type": "result", "ok": True, "result": result})
                except Exception as e:  # noqa: BLE001 - every failure is an answer
                    send(conn, {"id": rid, "type": "result", "ok": False, "error": str(e)})
    finally:
        stop.set()
        conn.close()


def main():
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", PORT))
    s.listen()
    print(f"fake game add-on on 127.0.0.1:{PORT}", flush=True)
    while True:
        conn, _ = s.accept()
        print("Android Controller connected", flush=True)
        threading.Thread(target=serve, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    main()
