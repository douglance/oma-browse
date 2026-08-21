// Everything the page says, on its way to `oma-browse page console`.
//
// Injected with the theme and the hints script (see `AppState::page_script`),
// at document start, so a message logged before the first line of the page's
// own code is still caught. The originals are always called: this observes the
// console, it does not replace it, and the inspector still shows what it always
// showed.
//
// Lines are held here and collected by `page console`, rather than pushed out
// as they happen, and that is not a preference -- it is the only channel that
// works. WebKit's script-message handlers are named, but wry connects to
// `script-message-received` with *no* name filter
// (wry-0.55.1/src/webkitgtk/mod.rs:638), so every message posted under any name
// is handed to Tauri's IPC parser. Tauri cannot parse a console line, says so
// with `console.error`, and this script catches that and posts it -- which is
// an infinite loop, measured, at a few thousand lines a second. A buffer the
// browser reads on demand costs nothing when nobody is looking and cannot feed
// itself.
(function () {
  if (window.__omaConsole) return;

  var LEVELS = ["debug", "log", "info", "warn", "error"];
  // Enough to hold a burst between two polls of `--follow`, which drains four
  // times a second. Past this the oldest line goes, exactly as it does in the
  // browser-side buffer this feeds.
  var KEEP = 500;
  var pending = [];

  function send(level, text, source) {
    try {
      if (pending.length >= KEEP) pending.shift();
      pending.push({ level: level, text: text, source: source || "", at: Date.now() });
    } catch (e) {
      // A console patch that can throw is a console patch that breaks pages.
    }
  }

  // What the browser calls to collect. Returns JSON rather than an array
  // because that is what survives the trip through `eval`, and empties itself
  // so that nothing is reported twice.
  window.__omaConsole = {
    drain: function () {
      var taken = pending;
      pending = [];
      return JSON.stringify(taken);
    }
  };

  // What a value looks like written down. Not `JSON.stringify` alone: it throws
  // on a cycle, renders an Error as `{}`, and turns a DOM node into a wall of
  // attributes -- and all three are exactly what somebody debugging is logging.
  function render(value, depth) {
    if (value === null) return "null";
    if (value === undefined) return "undefined";
    var kind = typeof value;
    if (kind === "string") return depth ? JSON.stringify(value) : value;
    if (kind === "number" || kind === "boolean" || kind === "bigint") return String(value);
    if (kind === "function") return "function " + (value.name || "(anonymous)");
    if (kind === "symbol") return value.toString();
    // WebKit's `stack` is frames only -- no `Error: message` header, unlike V8 --
    // so the header has to be put back or the line says where and never what.
    if (value instanceof Error) {
      var head = (value.name || "Error") + ": " + value.message;
      return value.stack ? head + "\n" + value.stack : head;
    }
    if (typeof Element !== "undefined" && value instanceof Element) {
      return "<" + value.tagName.toLowerCase() + (value.id ? "#" + value.id : "") + ">";
    }
    if (depth > 2) return Array.isArray(value) ? "[...]" : "{...}";
    try {
      if (Array.isArray(value)) {
        var items = [];
        for (var i = 0; i < value.length && i < 20; i++) items.push(render(value[i], depth + 1));
        if (value.length > 20) items.push("... " + (value.length - 20) + " more");
        return "[" + items.join(", ") + "]";
      }
      var parts = [];
      var keys = Object.keys(value);
      for (var k = 0; k < keys.length && k < 20; k++) {
        parts.push(keys[k] + ": " + render(value[keys[k]], depth + 1));
      }
      if (keys.length > 20) parts.push("... " + (keys.length - 20) + " more");
      return "{" + parts.join(", ") + "}";
    } catch (e) {
      return String(value);
    }
  }

  function join(args) {
    var out = [];
    for (var i = 0; i < args.length; i++) out.push(render(args[i], 0));
    return out.join(" ");
  }

  for (var n = 0; n < LEVELS.length; n++) {
    (function (level) {
      var original = console[level];
      console[level] = function () {
        send(level, join(arguments), "");
        if (original) return original.apply(console, arguments);
      };
    })(LEVELS[n]);
  }

  // The two failures that never reach `console.error` on their own, and the two
  // that matter most: a script that threw, and a promise nobody caught.
  window.addEventListener("error", function (e) {
    var where = e.filename ? e.filename + ":" + e.lineno + ":" + e.colno : "";
    // When there is a real Error its stack already says where, so saying it
    // again in the source column is the same fact twice on one line.
    if (e.error) send("error", render(e.error, 0), "");
    else send("error", String(e.message || "script error"), where);
  });

  window.addEventListener("unhandledrejection", function (e) {
    send("error", "unhandled rejection: " + render(e.reason, 0), "");
  });
})();
