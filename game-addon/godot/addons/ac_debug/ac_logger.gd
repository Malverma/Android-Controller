extends Logger
## Godot 4.5+: forwards the engine's errors and warnings and GDScript runtime
## errors (with backtraces) to Android Controller. ac_debug.gd loads this
## only when the Logger class exists. Called from any thread: hands over
## with call_deferred, and never prints (that would log again).

var _ac: Node


func _init(ac: Node) -> void:
	_ac = ac


func _log_error(function: String, file: String, line: int, code: String, rationale: String, _editor_notify: bool, error_type: int, script_backtraces: Array[ScriptBacktrace]) -> void:
	var level := "error"
	if error_type == ERROR_TYPE_WARNING:
		level = "warn"
	elif error_type == ERROR_TYPE_SCRIPT:
		level = "exception"  # a GDScript runtime error: the game's "exception"
	var stack := ""
	for bt in script_backtraces:
		stack += bt.format()
	stack += "%s:%d in %s()" % [file, line, function]
	var message := rationale if not rationale.is_empty() else code
	_ac.call_deferred("log_line", level, message, stack)


func _log_message(message: String, error: bool) -> void:
	_ac.call_deferred("log_line", "error" if error else "info", message.strip_edges(), "")
