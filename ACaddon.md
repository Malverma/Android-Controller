# ACaddon: the Android Controller game debug add-on

**Protocol version 1.** This file is for Claude (or a developer) adding the add-on to a game or app project. It is self-contained: read it all before implementing, then follow §7's checklist.

The add-on is a small debug-only server inside the game. Android Controller (a Linux PC app that mirrors and controls Android phones) connects to it. Claude then talks to the game through Android Controller's MCP server (`android-controller-mcp`), using the tools `game_info`, `game_state`, `game_command` and `game_logs`. With it, Claude can test a game it is developing on real phones without guessing from pixels. It can:

- read the game's real state (scene, level, player, score, currency…) instead of squinting at screenshots;
- jump straight to what it wants to test (`load_level 7`, `give_coins 500`, `skip_tutorial`) instead of playing there by tapping;
- see the game's own logs, and every exception with its stack trace, even ones that don't crash the game.

The reference implementations are in the Android Controller repository, under `game-addon/`:

| Engine | Files | Status |
|---|---|---|
| Unity (any version with .NET 4.x / Standard 2.x) | `game-addon/unity/ACDebugAddon.cs` | compile-checked in debug and release builds; protocol-tested |
| Godot 4.x | `game-addon/godot/addons/ac_debug/ac_debug.gd`, `ac_logger.gd` (4.5+) | written against the Godot 4 API; check it in the editor |
| Native Android (Java, Kotlin) | `game-addon/android/ACDebugAddon.java` | compile-checked against android-35 |
| Anything else | implement §4 (an hour of work in most engines) | — |
| Test double | `game-addon/tools/fake_game.py` | a fake game for checking the PC side |

On the PC this was built on, the repository is at `~/Projects/Android Controller`. Upstream: https://github.com/Malverma/Android-Controller.

---

## 1. How it fits together

```
 Claude (Cursor / Claude Code)
   │  MCP tools: game_info, game_state, game_command, game_logs (+ tap, screenshot, crashes...)
   ▼
 android-controller-mcp ──► Android Controller (PC app; control socket)
                               │
                               ├─ phone with ADB:    adb "tcp:27191" stream ─────────┐
                               └─ phone without ADB: helper app relays on its link ──┤
                                                                                     ▼
                                                     the game: 127.0.0.1:27191 (debug builds only)
```

- The game is the **server**. It listens on **`127.0.0.1:27191`** on the phone. Android Controller is the client.
- Android Controller checks every phone for the add-on about every 3 s and connects when the game runs. When the game closes, it reconnects next time the game starts. The game never needs to know the PC exists.
- Over ADB, Android Controller opens the port through adb. Phones without ADB run the Wireless Debugging Keeper helper (0.7+), which connects locally and relays.

## 2. Rules (must)

1. **Debug builds only.** The add-on must not exist in release builds: no listening socket, and ideally no code. Use the engine's debug flag: Unity `DEVELOPMENT_BUILD || UNITY_EDITOR`, Godot `OS.is_debug_build()`, Android `FLAG_DEBUGGABLE` (or `BuildConfig.DEBUG`). Commands like `give_coins` are cheats; they must never reach players.
2. **Loopback only.** Bind `127.0.0.1`, never `0.0.0.0`. Only things on the phone itself (adb's forward, the helper) should reach it.
3. **Never block the game.** Network I/O runs on background threads or non-blocking polling. Game work (state, commands) runs on the game's main thread, a few requests per frame at most.
4. **No secrets.** The state, logs and events go to the PC unencrypted (inside adb or the helper's authenticated link). Do not include passwords, tokens or other players' personal data.
5. **Survive anything from the socket.** Malformed JSON, unknown types, huge lines, a client vanishing mid-request: ignore or answer with an error, and never crash or stall the game.
6. **Keep the protocol exact** (§4). Android Controller and Claude rely on the field names.

## 3. What to expose

### Commands

Commands are how Claude gets to the thing it wants to test. Register the ones the game can support, named in `snake_case`, with short `help` text and documented `args`. Good ones for games:

| Command | Args | Purpose |
|---|---|---|
| `load_level` | `level:int` (or `name:string`) | jump to a level |
| `give_coins` / `give_item` | `amount:int` / `item:string, count:int` | test shops, upgrades, inventory |
| `set_health` / `god_mode` | `value:int` / `on:bool` | test late-game content without dying |
| `skip_tutorial` | — | get past onboarding |
| `unlock_all` | — | test every level, skin or feature |
| `reset_progress` | — | test first-run flows (destructive, so say so in `help`) |
| `win_level` / `lose_level` | — | test end screens and rewards |
| `spawn` | `what:string, x:float, y:float` | set up a situation |
| `set_time_scale` / `pause` | `scale:float` / `on:bool` | slow down, speed up or freeze gameplay |
| `seed` | `value:int` | deterministic randomness for repeatable tests |
| `teleport` | `x:float, y:float[, z:float]` | move the player |

The reference implementations already include engine-level built-ins: Unity has `time_scale`, `load_scene` and `scenes`; Godot has `pause`, `time_scale`, `change_scene` and `reload_scene`. Return something useful from every command (the new value, or a short message). Fail with a clear error message rather than doing nothing silently.

### State

`state` should answer: *where is the player, and what would a tester want to check?* Keep it small (well under 10 KB) and flat enough to read at a glance. For example:

```json
{"scene": "Level3", "level": 3, "player": {"pos": [12.5, 0, -3], "health": 80, "lives": 2},
 "score": 1190, "coins": 61, "paused": false, "tutorial_done": true, "enemies_alive": 4}
```

### Logs and events

- Forward the engine's log lines with their level, and **every exception with its stack trace** (level `exception`). Android Controller shows exceptions as a crash badge on the phone's tile and lists them in `crashes`.
- Send `event`s for game milestones: `level_started`, `level_complete`, `player_died`, `purchase`, `achievement`, `ad_shown`… Claude uses them to confirm that an action did what it should.

## 4. Protocol v1 reference

### Transport

- TCP, the game listening on `127.0.0.1:27191`. It accepts several clients at once and sends every log and event to all of them.
- **Newline-delimited JSON**: each message is one JSON object, UTF-8, on one line ending in `\n`. There is no other framing. Never put a raw newline inside a message (JSON escapes it as `\n`).
- Limits: a line is at most 1 MiB. Cut `message` and `stack` to about 8000 characters.
- Unknown fields must be ignored by both sides. Fields may be added within version 1.

### Game → PC

**`hello`**: sent at once to every client on connect.

```json
{"type": "hello", "protocol": 1, "game": "Stacker 3D", "version": "1.4.2", "engine": "Unity 6000.0.23f1",
 "package": "com.stacker3d.game",
 "commands": [
   {"name": "load_level", "args": {"level": "int"}, "help": "Load a level by number (1-40)"},
   {"name": "give_coins", "args": {"amount": "int"}, "help": "Add coins to the wallet"}
 ]}
```

- `protocol`: always `1` for this version.
- `game`, `version`, `engine`: free text for people and Claude.
- `package`: the Android application id. Use `""` if unknown.
- `commands`: every command, with `args` mapping each argument's name to a type hint (`int`, `float`, `string`, `bool`, `any`, or prose like `"string (res://...)"`).

If commands are registered later (after a scene loads), new clients see them in their `hello`. Clients that are already connected see them after they reconnect.

**`log`**: a log line, sent when it happens.

```json
{"type": "log", "level": "exception", "message": "NullReferenceException: Object reference not set",
 "stack": "Player.Jump () (at Assets/Player.cs:42)\nPlayer.Update () ..."}
```

- `level` is one of `debug`, `info`, `warn`, `error`, `exception`.
- `exception` means an exception the game threw. It does not matter whether it was caught or crashed the game; each one becomes a crash entry.
- `stack` may be `""` or `null`.

**`event`**: a game milestone.

```json
{"type": "event", "name": "level_complete", "data": {"level": 3, "stars": 2, "time_s": 41.5}}
```

`data` is any JSON value, or `null`.

**`result`**: the answer to one request, with its `id` copied back exactly.

```json
{"id": 7, "type": "result", "ok": true, "result": {"coins": 561}}
{"id": 8, "type": "result", "ok": false, "error": "no level 99 (1-40)"}
```

### PC → game

Every request carries an `id` (a number, unique per connection) and gets exactly one `result`. Answer in any order. Requests that arrive while a command runs wait their turn.

| Request | Answer `result` |
|---|---|
| `{"id": 1, "type": "state"}` | the state object (§3). The reference implementations wrap the game's own state: `{"scene", "fps", ..., "game": {...}}` |
| `{"id": 2, "type": "command", "name": "load_level", "args": {"level": 3}}` | whatever the command returns |
| `{"id": 3, "type": "ping"}` | `"pong"` |

Rules for answering requests:

- **Unknown `type` or command:** answer with `"ok": false` and an error that names the problem, e.g. `"unknown command fly (see game_info)"`.
- **Bad arguments:** fail with a helpful message, like `"level must be 1-40"`. Don't guess.
- **Timing:** Android Controller waits up to 10 s by default (Claude can raise it to 60 s). On phones without ADB the wait is capped at 10 s. Answer quickly. For a long command (loading a level), start it, answer `"loading"`, and let Claude poll `state` or wait for an `event`.

### Example session

```
← {"type":"hello","protocol":1,"game":"Fake Game","version":"0.1.0","engine":"python","package":"com.example.fakegame","commands":[{"name":"give_coins","args":{"amount":"int"},"help":"Add coins"}]}
→ {"id":1,"type":"command","name":"give_coins","args":{"amount":50}}
← {"type":"event","name":"coins_changed","data":{"coins":50}}
← {"id":1,"type":"result","ok":true,"result":{"coins":50}}
→ {"id":2,"type":"state"}
← {"id":2,"type":"result","ok":true,"result":{"scene":"Menu","coins":50}}
← {"type":"log","level":"exception","message":"RuntimeError: boom","stack":"Traceback ..."}
```

## 5. Engine guides

### Unity

1. Copy `game-addon/unity/ACDebugAddon.cs` anywhere under `Assets/` (e.g. `Assets/Scripts/Debug/`). It boots itself through `[RuntimeInitializeOnLoadMethod]`, so no scene setup is needed.
2. Build with **Development Build** ticked (Build Settings) to include it. Release builds compile it out completely, and the `ACDebug.*` calls become no-ops, so you can leave them in game code.
3. Register commands and state from your own code, e.g. in a bootstrap `MonoBehaviour.Awake`:

   ```csharp
   ACDebug.Command("load_level", "Load a level by number (1-40)", new[] { "level:int" }, args => {
       int n = ACDebug.Int(args, "level", 1);
       if (n < 1 || n > 40) throw new ArgumentException("level must be 1-40");
       LevelLoader.Load(n);
       return "loading level " + n;
   });
   ACDebug.State = () => new Dictionary<string, object> {
       ["level"] = LevelLoader.Current, ["coins"] = Wallet.Coins, ["player"] = Player.Instance?.transform.position,
   };
   ACDebug.Event("level_complete", new Dictionary<string, object> { ["level"] = n, ["stars"] = stars });
   ```

4. Handlers and `State` run on the main thread, so the Unity API is safe in them. Throw to fail a command.
5. `Debug.Log*` output, errors and exceptions (with stacks) are forwarded automatically through `Application.logMessageReceivedThreaded`.
6. Values can be dictionaries, lists, strings, numbers, bools, enums, `Vector2` and `Vector3`, or `[Serializable]` objects (through `JsonUtility`).
7. IL2CPP is fine (no reflection on user types). Android needs the `INTERNET` permission, which Unity adds by default.

### Godot 4

1. Copy `game-addon/godot/addons/ac_debug/` into the project's `res://addons/`.
2. Project › Project Settings › Globals › Autoload: add `res://addons/ac_debug/ac_debug.gd` named **`ACDebug`**.
3. It only runs in debug builds (`OS.is_debug_build()`: the editor and debug exports). Export the Android test build with **Export With Debug** checked. Release exports do not open the port.
4. Register from any script:

   ```gdscript
   func _ready():
       ACDebug.command("load_level", "Load a level by number (1-40)", {"level": "int"}, _load_level)
       ACDebug.state_provider = func(): return {"level": current_level, "coins": coins, "player": $Player.position}

   func _load_level(args: Dictionary):
       var n := int(args.get("level", 1))
       if n < 1 or n > 40:
           return ACDebug.fail("level must be 1-40")
       get_tree().call_deferred("change_scene_to_file", "res://levels/level_%d.tscn" % n)
       return "loading level %d" % n
   ```

5. Events: `ACDebug.event("level_complete", {"level": 3})`. Your own log lines: `ACDebug.log_line("warn", "slow frame", "")`.
6. **Godot 4.5+** forwards engine errors, warnings, `print` output and GDScript runtime errors (as `exception`, with backtraces) through `ac_logger.gd`. On Godot 4.0–4.4, delete `ac_logger.gd`, which uses the 4.5 `Logger` class, and call `log_line` yourself where it matters.
7. The Android export needs the **INTERNET** permission (Export › Permissions) for the socket.

### Native Android (Java or Kotlin)

1. Copy `game-addon/android/ACDebugAddon.java` into the app and change its `package` line.
2. In `Application.onCreate()`, call `ACDebugAddon.start(this)`. It does nothing unless the app is debuggable (debug build type).
3. Register commands and state:

   ```kotlin
   ACDebugAddon.command("give_coins", "Add coins", "amount:int") { args -> wallet.add(args.optInt("amount", 100)) }
   ACDebugAddon.state { JSONObject().put("screen", nav.currentRoute).put("coins", wallet.coins) }
   ACDebugAddon.event("purchase", JSONObject().put("sku", "coins_500"))
   ```

4. Handlers run on the main thread; throw to fail. Uncaught exceptions are sent (as `exception`) before Android's crash dialog. Return `JSONObject`, `JSONArray`, strings, numbers, booleans, or `null`.
5. Needs `<uses-permission android:name="android.permission.INTERNET" />`. If R8 or ProGuard strips the class in debug builds, keep it with `-keep class **.ACDebugAddon { *; }`.

### Other engines (Unreal, Defold, Cocos, Flutter, React Native, C++/NDK…)

Implement §4 directly. The minimum is:

- a loopback TCP listener on 27191, polled or on a thread, debug builds only;
- a line reader;
- `hello` on connect;
- `state`, `command` and `ping` answered by `id`;
- engine logs and exceptions forwarded as `log` messages.

About 150 lines in most languages. Suggested hooks:

- **Unreal:** an `FRunnable` or `FTSTicker` with `FSocket`, and `GLog->AddOutputDevice` for logs; guard with `#if !UE_BUILD_SHIPPING`.
- **Flutter:** `ServerSocket.bind(InternetAddress.loopbackIPv4, 27191)`, guarded by `kDebugMode`; `FlutterError.onError` and `PlatformDispatcher.instance.onError` for exceptions.
- **React Native:** do it natively (the Android Java file above), because JS has no server sockets; forward JS errors with `ErrorUtils.setGlobalHandler` and a native module.
- **C++ / NDK:** POSIX sockets on a thread, and a `std::set_terminate` or signal handler that sends what it can.

Copy the reference implementations' behaviour: JSON-safe values, cut long text, main-thread dispatch.

## 6. Testing the add-on

**Talk to it directly** (no Android Controller needed):

```bash
adb forward tcp:27191 tcp:27191          # PC port → the phone's loopback port
python3 - <<'EOF'
import socket, json
s = socket.create_connection(("127.0.0.1", 27191)); f = s.makefile("rwb")
print(json.loads(f.readline()))                                   # hello
f.write(b'{"id":1,"type":"state"}\n'); f.flush(); print(f.readline())
f.write(b'{"id":2,"type":"command","name":"give_coins","args":{"amount":5}}\n'); f.flush(); print(f.readline())
EOF
adb forward --remove tcp:27191
```

If you test with `adb forward`, remove it afterwards; it does not block Android Controller, which opens its own stream.

**Through Android Controller:** with the game running, the MCP tool `phones` shows `"game": {"game": ..., "version": ..., "engine": ...}` for that phone within about 3 s. Then check, in order:

1. `game_info`: the `hello` you sent, with all commands.
2. `game_state`: your state.
3. `game_command`: try a good command and a bad one. The bad one must fail with a clear message.
4. `game_logs`: your log lines.
5. Throw a test exception. It must appear in `game_logs` as `EXCEPTION` with its stack, and in `crashes` as `kind: "Exception"`.

**Checking the PC side without a game:** `game-addon/tools/fake_game.py` speaks the protocol. Run it on the PC and route a phone's port to it with `adb -s <phone> reverse tcp:27191 tcp:27191`. Remove the route afterwards with `adb reverse --remove tcp:27191`.

## 7. Implementation checklist

- [ ] Compiled or active **only** in debug or development builds; release builds verified to not listen. On a release build, connecting with `adb forward tcp:27191 tcp:27191` + `nc 127.0.0.1 27191` must fail.
- [ ] Binds `127.0.0.1:27191`; several clients at once; a client leaving never affects the game.
- [ ] Sends `hello` first, with `protocol: 1`, game name, version, engine, package and every command with `args` and `help`.
- [ ] Answers `state`, `command` and `ping` with `{"id", "type": "result", "ok", ...}`, using the exact request `id`.
- [ ] Unknown command or type, bad arguments, or a thrown handler gives `ok: false` with a useful `error`; the game keeps running.
- [ ] Game work runs on the main or game thread; network on background threads or non-blocking.
- [ ] Engine logs forwarded with levels; exceptions as `exception` with stack traces.
- [ ] Events for the game's key moments.
- [ ] Commands cover getting anywhere quickly (levels, currency, unlocks, skip tutorial) and their arguments are validated.
- [ ] State is small and answers "where am I and what matters now".
- [ ] Android: `INTERNET` permission present.
- [ ] Tested with §6 (direct, then through Android Controller).

## 8. How Claude should use it while developing a game

The tools come from `android-controller-mcp`. Cursor's `~/.cursor/mcp.json`:

```json
{"mcpServers": {"android-controller": {"command": "android-controller-mcp"}}}
```

A good loop for testing a change:

1. **Build and install:** `install_apk {path}` (debug build, so the add-on is in it), then `app {action: "launch", package}`.
2. Use `phones` to confirm the game shows up (`game` is set), then `game_info` to learn its commands.
3. **Get to the feature:** use `game_command` (e.g. `load_level`, `give_coins`) rather than tapping through menus.
4. **Act like a player:** `screenshot` to see the frame (a game draws pixels, so `ui_tree` sees nothing). Then `tap` and `swipe` in phone pixels; the screenshot says how to scale. Use `wait_for` and `ui_tree` only for native Android UI, such as permission dialogs and ads.
5. **Check the result:** `game_state` for the truth, `game_logs {since}` for what happened, and `crashes {since}` for exceptions and native crashes. Use `perf` to watch frame rate and memory.
6. With several phones, pass `phone: "all"` for the same step everywhere (different Android versions and screen sizes). Use `test_run` to keep logs, crashes, stats and screenshots in one folder.

## 9. Troubleshooting

| Symptom | Check |
|---|---|
| `phones` shows `"game": null` | Is it a debug or development build? Is the game in the foreground and past its first frame? Is the `INTERNET` permission present? Is another process holding port 27191 on the phone? Is Automation on in Android Controller (Preferences › Testing)? |
| `game_command` times out | The handler blocks the main thread, or the main thread is frozen. Answer fast, and do long work over several frames. |
| No exceptions in `crashes` | Forward exceptions as `level: "exception"` (not `error`). The same message within 10 s counts once. |
| Works over ADB, not on phones without ADB | The phone needs Wireless Debugging Keeper **0.7+**; Android Controller offers the update on the tile. |
| Garbled data | One JSON object per line, UTF-8, `\n` at the end; no pretty-printed JSON. |

## 10. Versioning

This is **protocol 1**. Fields may be added without changing the number; both sides ignore unknown fields. A breaking change would bump `protocol` in `hello`, and Android Controller would keep speaking 1 to games that say 1. Note the protocol version in the game's code next to the add-on.
