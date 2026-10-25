extends Node
## Android Controller debug add-on for Godot 4: protocol v1 (see ACaddon.md).
##
## Add this script as an autoload named "ACDebug" (Project › Project Settings ›
## Globals › Autoload). In debug builds it listens on 127.0.0.1:27191 on the
## phone; Android Controller (and Claude through its MCP server) can then read
## the game's state and logs and run the commands you register:
##
##     ACDebug.command("give_coins", "Add coins", {"amount": "int"}, func(args):
##         Wallet.coins += int(args.get("amount", 100))
##         return Wallet.coins)
##     ACDebug.state_provider = func(): return {"level": Level.current, "coins": Wallet.coins}
##     ACDebug.event("level_complete", {"level": 3})
##     ACDebug.log_line("error", "save file corrupt", "")   # your own log lines
##
## A command fails by returning ACDebug.fail("why"). Export templates built
## for release (not debug) never open the port.
## Godot 4.5+ also forwards engine errors and script errors (with their
## backtraces) through ac_logger.gd; on older 4.x call log_line yourself.

const PROTOCOL := 1
const PORT := 27191
const MAX_TEXT := 8000
const MAX_LINE := 1 << 20

## Returns the game's own state (a Dictionary) for game_state.
var state_provider: Callable

var _commands := {}
var _server := TCPServer.new()
var _clients: Array[StreamPeerTCP] = []
var _buffers := {}
var _enabled := false


func _ready() -> void:
	if not OS.is_debug_build():
		set_process(false)
		return
	_enabled = true
	process_mode = Node.PROCESS_MODE_ALWAYS  # keep answering while the tree is paused
	command("pause", "Pause or resume the game", {"on": "bool"}, _cmd_pause)
	command("time_scale", "Set Engine.time_scale (0.1 slow, 1 normal, 4 fast)", {"scale": "float"}, _cmd_time_scale)
	command("change_scene", "Load a scene file", {"path": "string (res://...)"}, _cmd_change_scene)
	command("reload_scene", "Reload the current scene", {}, _cmd_reload_scene)
	var err := _server.listen(PORT, "127.0.0.1")
	if err != OK:
		push_warning("ACDebug: cannot listen on port %d (error %d)" % [PORT, err])
	if ClassDB.class_exists("Logger"):
		var logger_script = load(get_script().resource_path.get_base_dir().path_join("ac_logger.gd"))
		if logger_script:
			OS.call("add_logger", logger_script.new(self))


## Registers a command; `args` documents its arguments ({"name": "type"}).
func command(cmd_name: String, help: String, args: Dictionary, handler: Callable) -> void:
	_commands[cmd_name] = {"help": help, "args": args, "fn": handler}


## Something happened in the game (level started, purchase, boss defeated...).
func event(event_name: String, data = null) -> void:
	_send({"type": "event", "name": event_name, "data": _jsonable(data)})


## A log line: level is debug, info, warn, error or exception.
func log_line(level: String, message: String, stack: String = "") -> void:
	_send({"type": "log", "level": level, "message": message.left(MAX_TEXT), "stack": stack.left(MAX_TEXT)})


## Return this from a command handler to make it fail.
func fail(why: String) -> Dictionary:
	return {"__ac_error": why}


func _process(_delta: float) -> void:
	while _server.is_listening() and _server.is_connection_available():
		var peer := _server.take_connection()
		peer.set_no_delay(true)
		_clients.append(peer)
		_buffers[peer] = PackedByteArray()
		_write(peer, _hello())
	for peer in _clients.duplicate():
		peer.poll()
		if peer.get_status() != StreamPeerTCP.STATUS_CONNECTED:
			_clients.erase(peer)
			_buffers.erase(peer)
			continue
		var n := peer.get_available_bytes()
		if n > 0:
			var got: Array = peer.get_data(n)
			if got[0] == OK:
				_buffers[peer].append_array(got[1])
		var buf: PackedByteArray = _buffers[peer]
		var at := buf.find(10)
		while at >= 0:
			var line := buf.slice(0, at).get_string_from_utf8().strip_edges()
			buf = buf.slice(at + 1)
			if not line.is_empty():
				_handle(peer, line)
			at = buf.find(10)
		if buf.size() > MAX_LINE:
			buf = PackedByteArray()
		_buffers[peer] = buf


func _hello() -> Dictionary:
	var list := []
	for cmd_name in _commands:
		list.append({"name": cmd_name, "args": _commands[cmd_name]["args"], "help": _commands[cmd_name]["help"]})
	return {
		"type": "hello", "protocol": PROTOCOL,
		"game": str(ProjectSettings.get_setting("application/config/name", "Godot game")),
		"version": str(ProjectSettings.get_setting("application/config/version", "")),
		"engine": "Godot " + Engine.get_version_info()["string"],
		"package": "", # Android Controller fills in the app from the phone when empty
		"commands": list,
	}


func _handle(peer: StreamPeerTCP, line: String) -> void:
	var req = JSON.parse_string(line)
	if typeof(req) != TYPE_DICTIONARY:
		return
	var reply := {"id": req.get("id"), "type": "result"}
	var kind: String = str(req.get("type", ""))
	if kind == "state":
		var scene := get_tree().current_scene
		reply["ok"] = true
		reply["result"] = {
			"scene": scene.scene_file_path if scene else "",
			"paused": get_tree().paused,
			"time_scale": Engine.time_scale,
			"fps": Engine.get_frames_per_second(),
			"game": _jsonable(state_provider.call()) if state_provider.is_valid() else null,
		}
	elif kind == "command":
		var cmd_name: String = str(req.get("name", ""))
		if not _commands.has(cmd_name):
			reply["ok"] = false
			reply["error"] = "unknown command %s (see game_info)" % cmd_name
		else:
			var args = req.get("args", {})
			if typeof(args) != TYPE_DICTIONARY:
				args = {}
			var result = _commands[cmd_name]["fn"].call(args)
			if typeof(result) == TYPE_DICTIONARY and result.has("__ac_error"):
				reply["ok"] = false
				reply["error"] = str(result["__ac_error"])
			else:
				reply["ok"] = true
				reply["result"] = _jsonable(result)
	elif kind == "ping":
		reply["ok"] = true
		reply["result"] = "pong"
	else:
		reply["ok"] = false
		reply["error"] = "unknown request type " + kind
	_write(peer, reply)


## Vectors become arrays, objects their names: JSON.stringify would make strings of them.
func _jsonable(v):
	match typeof(v):
		TYPE_VECTOR2, TYPE_VECTOR2I:
			return [v.x, v.y]
		TYPE_VECTOR3, TYPE_VECTOR3I:
			return [v.x, v.y, v.z]
		TYPE_COLOR:
			return v.to_html()
		TYPE_DICTIONARY:
			var d := {}
			for k in v:
				d[str(k)] = _jsonable(v[k])
			return d
		TYPE_ARRAY:
			var a := []
			for x in v:
				a.append(_jsonable(x))
			return a
		TYPE_OBJECT:
			return str(v)
		_:
			return v


func _send(msg: Dictionary) -> void:
	if not _enabled:
		return
	for peer in _clients:
		_write(peer, msg)


func _write(peer: StreamPeerTCP, msg: Dictionary) -> void:
	if peer.get_status() == StreamPeerTCP.STATUS_CONNECTED:
		peer.put_data((JSON.stringify(msg) + "\n").to_utf8_buffer())


func _cmd_pause(args: Dictionary):
	get_tree().paused = bool(args.get("on", true))
	return get_tree().paused


func _cmd_time_scale(args: Dictionary):
	Engine.time_scale = float(args.get("scale", 1.0))
	return Engine.time_scale


func _cmd_change_scene(args: Dictionary):
	var path := str(args.get("path", ""))
	if not ResourceLoader.exists(path):
		return fail("no scene at " + path)
	get_tree().call_deferred("change_scene_to_file", path)
	return "loading " + path


func _cmd_reload_scene(_args: Dictionary):
	get_tree().call_deferred("reload_current_scene")
	return "reloading"
