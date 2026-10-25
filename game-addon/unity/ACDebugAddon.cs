// Android Controller debug add-on for Unity: protocol v1 (see ACaddon.md).
//
// Drop this file anywhere under Assets/. In Development Builds (and the
// Editor) it starts by itself and listens on 127.0.0.1:27191 on the phone;
// Android Controller (and Claude through its MCP server) can then read the
// game's state and logs, see exceptions, and run the commands you register:
//
//     ACDebug.Command("give_coins", "Add coins", new[] { "amount:int" },
//         args => { Wallet.Coins += ACDebug.Int(args, "amount", 100); return Wallet.Coins; });
//     ACDebug.State = () => new Dictionary<string, object> { ["level"] = Level.Current, ["coins"] = Wallet.Coins };
//     ACDebug.Event("level_complete", new Dictionary<string, object> { ["level"] = 3 });
//
// Release builds compile none of it (everything is inside
// DEVELOPMENT_BUILD || UNITY_EDITOR) and the calls above become no-ops.
// Command handlers and State run on Unity's main thread.

using System;
using System.Collections;
using System.Collections.Generic;
using System.Globalization;
using System.Text;
using UnityEngine;
#if DEVELOPMENT_BUILD || UNITY_EDITOR
using System.Collections.Concurrent;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Threading;
using UnityEngine.SceneManagement;
#endif

public static class ACDebug
{
    /// <summary>The game's own state for <c>game_state</c> (main thread). Return a dictionary, list or [Serializable] object.</summary>
    public static Func<object> State;

    /// <summary>Registers a command. <paramref name="args"/> documents its arguments ("name:type").</summary>
    public static void Command(string name, string help, string[] args, Func<Dictionary<string, object>, object> handler)
    {
#if DEVELOPMENT_BUILD || UNITY_EDITOR
        ACDebugAddon.Register(name, help, args, handler);
#endif
    }

    /// <summary>Something happened in the game (level started, purchase, boss defeated...).</summary>
    public static void Event(string name, object data = null)
    {
#if DEVELOPMENT_BUILD || UNITY_EDITOR
        ACDebugAddon.Send(new Dictionary<string, object> { ["type"] = "event", ["name"] = name, ["data"] = data });
#endif
    }

    public static int Int(Dictionary<string, object> args, string key, int fallback = 0) =>
        args != null && args.TryGetValue(key, out var v) && v != null ? Convert.ToInt32(v, CultureInfo.InvariantCulture) : fallback;

    public static float Float(Dictionary<string, object> args, string key, float fallback = 0) =>
        args != null && args.TryGetValue(key, out var v) && v != null ? Convert.ToSingle(v, CultureInfo.InvariantCulture) : fallback;

    public static string Str(Dictionary<string, object> args, string key, string fallback = null) =>
        args != null && args.TryGetValue(key, out var v) && v != null ? Convert.ToString(v, CultureInfo.InvariantCulture) : fallback;

    public static bool Bool(Dictionary<string, object> args, string key, bool fallback = false) =>
        args != null && args.TryGetValue(key, out var v) && v is bool b ? b : fallback;
}

#if DEVELOPMENT_BUILD || UNITY_EDITOR
public sealed class ACDebugAddon : MonoBehaviour
{
    public const int Protocol = 1;
    public const int Port = 27191;
    const int MaxLine = 1 << 20;
    const int MaxText = 8000;

    sealed class Cmd
    {
        public string Help;
        public string[] Args;
        public Func<Dictionary<string, object>, object> Handler;
    }

    sealed class Client
    {
        public TcpClient Tcp;
        public Stream Stream;
        public readonly object WriteLock = new object();
    }

    static readonly Dictionary<string, Cmd> commands = new Dictionary<string, Cmd>();
    static readonly List<Client> clients = new List<Client>();
    static readonly ConcurrentQueue<KeyValuePair<Client, Dictionary<string, object>>> requests =
        new ConcurrentQueue<KeyValuePair<Client, Dictionary<string, object>>>();
    static TcpListener listener;
    static Thread acceptThread;
    static ACDebugAddon instance;
    static float fps;

    [RuntimeInitializeOnLoadMethod(RuntimeInitializeLoadType.BeforeSceneLoad)]
    static void Boot()
    {
        if (instance != null) return;
        var go = new GameObject("ACDebugAddon") { hideFlags = HideFlags.HideAndDontSave };
        DontDestroyOnLoad(go);
        instance = go.AddComponent<ACDebugAddon>();
    }

    internal static void Register(string name, string help, string[] args, Func<Dictionary<string, object>, object> handler)
    {
        lock (commands) commands[name] = new Cmd { Help = help, Args = args ?? new string[0], Handler = handler };
    }

    void Awake()
    {
        Register("time_scale", "Set Time.timeScale (0 pauses, 1 normal, 4 fast)", new[] { "scale:float" }, a =>
        {
            Time.timeScale = ACDebug.Float(a, "scale", 1);
            return Time.timeScale;
        });
        Register("load_scene", "Load a scene by name or build index", new[] { "name:string", "index:int" }, a =>
        {
            var n = ACDebug.Str(a, "name");
            if (!string.IsNullOrEmpty(n)) SceneManager.LoadScene(n);
            else SceneManager.LoadScene(ACDebug.Int(a, "index", 0));
            return "loading";
        });
        Register("scenes", "Scenes in the build", new string[0], a =>
        {
            var list = new List<object>();
            for (int i = 0; i < SceneManager.sceneCountInBuildSettings; i++)
                list.Add(System.IO.Path.GetFileNameWithoutExtension(UnityEngine.SceneManagement.SceneUtility.GetScenePathByBuildIndex(i)));
            return list;
        });
        Application.logMessageReceivedThreaded += OnLog;
        try
        {
            listener = new TcpListener(IPAddress.Loopback, Port);
            listener.Start();
            acceptThread = new Thread(Accept) { IsBackground = true, Name = "ACDebug accept" };
            acceptThread.Start();
        }
        catch (Exception e)
        {
            Debug.LogWarning("ACDebug: cannot listen on port " + Port + ": " + e.Message);
        }
    }

    void OnDestroy()
    {
        Application.logMessageReceivedThreaded -= OnLog;
        try { listener?.Stop(); } catch (Exception) { }
        lock (clients)
        {
            foreach (var c in clients) c.Tcp.Close();
            clients.Clear();
        }
    }

    void Update()
    {
        fps = Mathf.Lerp(fps, 1f / Mathf.Max(Time.unscaledDeltaTime, 1e-4f), 0.05f);
        // Requests run here, on the main thread, a few per frame.
        for (int n = 0; n < 16 && requests.TryDequeue(out var r); n++) Answer(r.Key, r.Value);
    }

    static void Accept()
    {
        while (true)
        {
            TcpClient tcp;
            try { tcp = listener.AcceptTcpClient(); }
            catch (Exception) { return; }
            tcp.NoDelay = true;
            var c = new Client { Tcp = tcp, Stream = tcp.GetStream() };
            lock (clients) clients.Add(c);
            Write(c, Hello());
            new Thread(() => Read(c)) { IsBackground = true, Name = "ACDebug client" }.Start();
        }
    }

    static void Read(Client c)
    {
        var buf = new List<byte>();
        var chunk = new byte[4096];
        try
        {
            while (true)
            {
                int n = c.Stream.Read(chunk, 0, chunk.Length);
                if (n <= 0) break;
                for (int i = 0; i < n; i++)
                {
                    if (chunk[i] != (byte)'\n') { if (buf.Count < MaxLine) buf.Add(chunk[i]); continue; }
                    var line = Encoding.UTF8.GetString(buf.ToArray()).Trim();
                    buf.Clear();
                    if (line.Length == 0) continue;
                    if (ACDebugJson.Parse(line) is Dictionary<string, object> req)
                        requests.Enqueue(new KeyValuePair<Client, Dictionary<string, object>>(c, req));
                }
            }
        }
        catch (Exception) { }
        lock (clients) clients.Remove(c);
        c.Tcp.Close();
    }

    static Dictionary<string, object> Hello()
    {
        var list = new List<object>();
        lock (commands)
        {
            foreach (var kv in commands)
            {
                var args = new Dictionary<string, object>();
                foreach (var a in kv.Value.Args)
                {
                    var p = a.Split(':');
                    args[p[0]] = p.Length > 1 ? p[1] : "any";
                }
                list.Add(new Dictionary<string, object> { ["name"] = kv.Key, ["args"] = args, ["help"] = kv.Value.Help });
            }
        }
        return new Dictionary<string, object>
        {
            ["type"] = "hello", ["protocol"] = Protocol, ["game"] = Application.productName, ["version"] = Application.version,
            ["engine"] = "Unity " + Application.unityVersion, ["package"] = Application.identifier, ["commands"] = list,
        };
    }

    static void Answer(Client c, Dictionary<string, object> req)
    {
        req.TryGetValue("id", out var id);
        var type = req.TryGetValue("type", out var t) ? t as string : null;
        var reply = new Dictionary<string, object> { ["id"] = id, ["type"] = "result" };
        try
        {
            object result;
            if (type == "state")
            {
                result = new Dictionary<string, object>
                {
                    ["scene"] = SceneManager.GetActiveScene().name, ["time_scale"] = Time.timeScale, ["fps"] = Mathf.Round(fps),
                    ["frame"] = Time.frameCount, ["time"] = Time.time, ["game"] = ACDebug.State?.Invoke(),
                };
            }
            else if (type == "command")
            {
                var name = req.TryGetValue("name", out var nm) ? nm as string : null;
                Cmd cmd;
                lock (commands) commands.TryGetValue(name ?? "", out cmd);
                if (cmd == null) throw new ArgumentException("unknown command " + name + " (see game_info)");
                result = cmd.Handler(req.TryGetValue("args", out var a) ? a as Dictionary<string, object> : null);
            }
            else if (type == "ping") result = "pong";
            else throw new ArgumentException("unknown request type " + type);
            reply["ok"] = true;
            reply["result"] = result;
        }
        catch (Exception e)
        {
            reply["ok"] = false;
            reply["error"] = e.GetType().Name + ": " + e.Message;
        }
        Write(c, reply);
    }

    static void OnLog(string message, string stack, LogType type)
    {
        string level;
        switch (type)
        {
            case LogType.Exception: level = "exception"; break;
            case LogType.Error:
            case LogType.Assert: level = "error"; break;
            case LogType.Warning: level = "warn"; break;
            default: level = "info"; break;
        }
        Send(new Dictionary<string, object> { ["type"] = "log", ["level"] = level, ["message"] = Clip(message), ["stack"] = Clip(stack) });
    }

    static string Clip(string s) => s == null || s.Length <= MaxText ? s : s.Substring(0, MaxText) + "…";

    internal static void Send(Dictionary<string, object> msg)
    {
        Client[] all;
        lock (clients) all = clients.ToArray();
        foreach (var c in all) Write(c, msg);
    }

    static void Write(Client c, Dictionary<string, object> msg)
    {
        try
        {
            var bytes = Encoding.UTF8.GetBytes(ACDebugJson.Write(msg) + "\n");
            lock (c.WriteLock) c.Stream.Write(bytes, 0, bytes.Length);
        }
        catch (Exception)
        {
            lock (clients) clients.Remove(c);
        }
    }
}

/// <summary>Minimal JSON (objects become Dictionary&lt;string, object&gt;, arrays List&lt;object&gt;, numbers long or double).</summary>
static class ACDebugJson
{
    public static object Parse(string s)
    {
        int i = 0;
        try
        {
            var v = Value(s, ref i);
            return v;
        }
        catch (Exception)
        {
            return null;
        }
    }

    static void Ws(string s, ref int i)
    {
        while (i < s.Length && char.IsWhiteSpace(s[i])) i++;
    }

    static object Value(string s, ref int i)
    {
        Ws(s, ref i);
        char c = s[i];
        if (c == '{')
        {
            var d = new Dictionary<string, object>();
            i++;
            Ws(s, ref i);
            if (s[i] == '}') { i++; return d; }
            while (true)
            {
                Ws(s, ref i);
                var k = Str(s, ref i);
                Ws(s, ref i);
                i++; // :
                d[k] = Value(s, ref i);
                Ws(s, ref i);
                if (s[i++] == '}') return d;
            }
        }
        if (c == '[')
        {
            var l = new List<object>();
            i++;
            Ws(s, ref i);
            if (s[i] == ']') { i++; return l; }
            while (true)
            {
                l.Add(Value(s, ref i));
                Ws(s, ref i);
                if (s[i++] == ']') return l;
            }
        }
        if (c == '"') return Str(s, ref i);
        if (s.Substring(i).StartsWith("true")) { i += 4; return true; }
        if (s.Substring(i).StartsWith("false")) { i += 5; return false; }
        if (s.Substring(i).StartsWith("null")) { i += 4; return null; }
        int start = i;
        while (i < s.Length && "+-0123456789.eE".IndexOf(s[i]) >= 0) i++;
        var num = s.Substring(start, i - start);
        if (long.TryParse(num, NumberStyles.Integer, CultureInfo.InvariantCulture, out var n)) return n;
        return double.Parse(num, CultureInfo.InvariantCulture);
    }

    static string Str(string s, ref int i)
    {
        var sb = new StringBuilder();
        i++; // opening quote
        while (s[i] != '"')
        {
            char c = s[i++];
            if (c != '\\') { sb.Append(c); continue; }
            char e = s[i++];
            switch (e)
            {
                case 'n': sb.Append('\n'); break;
                case 't': sb.Append('\t'); break;
                case 'r': sb.Append('\r'); break;
                case 'b': sb.Append('\b'); break;
                case 'f': sb.Append('\f'); break;
                case 'u': sb.Append((char)Convert.ToInt32(s.Substring(i, 4), 16)); i += 4; break;
                default: sb.Append(e); break;
            }
        }
        i++;
        return sb.ToString();
    }

    public static string Write(object v)
    {
        var sb = new StringBuilder();
        Write(sb, v, 0);
        return sb.ToString();
    }

    static void Write(StringBuilder sb, object v, int depth)
    {
        if (depth > 32) { sb.Append("null"); return; }
        switch (v)
        {
            case null: sb.Append("null"); break;
            case string s: Quote(sb, s); break;
            case bool b: sb.Append(b ? "true" : "false"); break;
            case float f: Number(sb, f); break;
            case double d: Number(sb, d); break;
            case decimal m: sb.Append(m.ToString(CultureInfo.InvariantCulture)); break;
            case int _:
            case long _:
            case short _:
            case byte _:
            case uint _:
            case ulong _:
                sb.Append(Convert.ToString(v, CultureInfo.InvariantCulture)); break;
            case Enum e: Quote(sb, e.ToString()); break;
            case Vector2 p: Write(sb, new List<object> { p.x, p.y }, depth + 1); break;
            case Vector3 p: Write(sb, new List<object> { p.x, p.y, p.z }, depth + 1); break;
            case IDictionary d:
                sb.Append('{');
                bool first = true;
                foreach (DictionaryEntry kv in d)
                {
                    if (!first) sb.Append(',');
                    first = false;
                    Quote(sb, Convert.ToString(kv.Key, CultureInfo.InvariantCulture));
                    sb.Append(':');
                    Write(sb, kv.Value, depth + 1);
                }
                sb.Append('}');
                break;
            case IEnumerable list:
                sb.Append('[');
                bool f1 = true;
                foreach (var x in list)
                {
                    if (!f1) sb.Append(',');
                    f1 = false;
                    Write(sb, x, depth + 1);
                }
                sb.Append(']');
                break;
            default:
                // [Serializable] classes and structs.
                var json = JsonUtility.ToJson(v);
                sb.Append(string.IsNullOrEmpty(json) ? "null" : json);
                break;
        }
    }

    static void Number(StringBuilder sb, double d)
    {
        if (double.IsNaN(d) || double.IsInfinity(d)) sb.Append("null");
        else sb.Append(d.ToString("R", CultureInfo.InvariantCulture));
    }

    static void Quote(StringBuilder sb, string s)
    {
        sb.Append('"');
        foreach (char c in s)
        {
            switch (c)
            {
                case '"': sb.Append("\\\""); break;
                case '\\': sb.Append("\\\\"); break;
                case '\n': sb.Append("\\n"); break;
                case '\r': sb.Append("\\r"); break;
                case '\t': sb.Append("\\t"); break;
                default:
                    if (c < 0x20) sb.Append("\\u").Append(((int)c).ToString("x4"));
                    else sb.Append(c);
                    break;
            }
        }
        sb.Append('"');
    }
}
#endif
