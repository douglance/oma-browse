// Link hints — reach any clickable thing on the page without the mouse.
//
// Vimium's interaction, because a decade of muscle memory already knows it:
// `f` labels every clickable element in the viewport, typing the label
// activates it, `F` does the same but opens links in a new tab. The same file
// carries the scroll keys -- `j`, `k`, `d`, `u`, `gg`, `G` -- which belong here
// for exactly the same reason: they are bare letters, and only the page knows
// whether the caret is in a text field.
//
// This runs as a *page* script rather than a GTK accelerator, which is the
// opposite of every other shortcut in this browser (see
// `AppState::page_script`). It has to be: a bare `f` bound on the toplevel
// would swallow the letter in every search box on the web, and only the page
// knows whether the caret is currently in one. Hints also need the DOM, so a
// page that blocks scripts was never going to have them anyway.
(function () {
  "use strict";
  if (window.__omaHints) return;

  // The browser cannot open a tab from inside a page: there is no channel from
  // page JavaScript back to the control plane -- it is `http://127.0.0.1` and
  // the page is usually `https`, so mixed content blocks the request. Instead
  // the page navigates to a sentinel URL that never resolves and the navigation
  // handler cancels it, reads the target, and opens the tab. See
  // `crate::hints::intercept`.
  var SENTINEL = "__OMA_HINT_SENTINEL__";
  var ID = "__oma_browse_hints";

  // Home row first. Vimium's default alphabet, which is chosen so that no two
  // hints of the same length share a confusable pair.
  var CHARS = "sadfjklewcmpgh";

  // A page with ten thousand links is a page where hints stop being useful, and
  // measuring every one of them costs a layout read each.
  var MAX_SCAN = 3000;

  var SELECTOR = [
    "a[href]",
    "area[href]",
    "button:not([disabled])",
    "details > summary",
    "input:not([type=hidden]):not([disabled])",
    "select:not([disabled])",
    "textarea:not([disabled])",
    "label[for]",
    "[onclick]",
    "[contenteditable='']",
    "[contenteditable='true']",
    "[tabindex]:not([tabindex='-1'])",
    "[role=button]",
    "[role=link]",
    "[role=checkbox]",
    "[role=radio]",
    "[role=switch]",
    "[role=menuitem]",
    "[role=menuitemcheckbox]",
    "[role=menuitemradio]",
    "[role=option]",
    "[role=tab]",
    "[role=treeitem]"
  ].join(",");

  // Fields where a keystroke is text, not a command.
  var TEXTY = [
    "text", "search", "email", "url", "tel", "password", "number",
    "date", "datetime-local", "month", "week", "time"
  ];

  var live = null;

  // `all: initial` on a hint label is what keeps a site's own CSS off it -- and
  // it resets custom properties too, so a `var(--oma-*)` in the same declaration
  // would resolve to nothing even where the token existed. The theme comes from
  // the page runtime instead, which is injected beside this script and carries
  // the resolved colours; see `window.__oma.theme` in `page.js`.
  function token(key, fallback) {
    var t = window.__oma && window.__oma.theme;
    return (t && t[key]) || fallback;
  }

  function typing() {
    var el = document.activeElement;
    if (!el || el === document.body) return false;
    if (el.isContentEditable) return true;
    var tag = el.tagName;
    if (tag === "INPUT") return TEXTY.indexOf((el.type || "text").toLowerCase()) !== -1;
    return tag === "TEXTAREA" || tag === "SELECT";
  }

  // Prefix-free labels, shortest first, so a page with fourteen links needs one
  // keystroke each and a page with two hundred needs two. Reversing before the
  // sort is what spreads the second character across the alphabet instead of
  // giving every hint on the page the same first letter.
  function labels(n) {
    var out = [""];
    var offset = 0;
    while (out.length - offset < n || out.length === 1) {
      var stem = out[offset++];
      for (var i = 0; i < CHARS.length; i++) out.push(stem + CHARS[i]);
    }
    return out
      .slice(offset, offset + n)
      .map(function (h) {
        return h.split("").reverse().join("");
      })
      .sort();
  }

  // Every rect is read before any style is written, so the pass costs one
  // layout rather than one per element.
  function collect() {
    var nodes;
    try {
      nodes = document.querySelectorAll(SELECTOR);
    } catch (e) {
      return [];
    }
    var found = [];
    var limit = Math.min(nodes.length, MAX_SCAN);
    for (var i = 0; i < limit; i++) {
      var el = nodes[i];
      if (el.closest && el.closest("#" + ID)) continue;
      var r = el.getBoundingClientRect();
      if (r.width < 4 || r.height < 4) continue;
      if (r.bottom <= 0 || r.right <= 0 || r.left >= innerWidth || r.top >= innerHeight) continue;
      var cs;
      try {
        cs = getComputedStyle(el);
      } catch (e2) {
        continue;
      }
      if (cs.visibility !== "visible" || cs.pointerEvents === "none") continue;
      if (parseFloat(cs.opacity) < 0.1) continue;
      // A closed menu is still laid out, still the right size, and still
      // entirely behind whatever is drawn over it. Asking the document what is
      // actually at that point is the only test that catches it.
      var x = Math.min(Math.max(r.left + Math.min(r.width / 2, 8), 1), innerWidth - 1);
      var y = Math.min(Math.max(r.top + Math.min(r.height / 2, 8), 1), innerHeight - 1);
      var hit = document.elementFromPoint(x, y);
      if (hit && hit !== el && !el.contains(hit) && !hit.contains(el)) continue;
      found.push({ el: el, rect: r });
    }
    var tags = labels(found.length);
    for (var j = 0; j < found.length; j++) found[j].label = tags[j];
    return found;
  }

  function box() {
    var el = document.getElementById(ID);
    if (!el) {
      el = document.createElement("div");
      el.id = ID;
      el.setAttribute("aria-hidden", "true");
      el.style.cssText =
        "all:initial;position:fixed;left:0;top:0;width:0;height:0;" +
        "z-index:2147483647;pointer-events:none;";
      (document.body || document.documentElement).appendChild(el);
    }
    return el;
  }

  // The label sits at the element's top-left corner, nudged inside the viewport
  // so a link that starts off-screen still gets a reachable hint.
  function place(node, rect) {
    node.style.left = Math.max(0, Math.min(rect.left, innerWidth - 24)) + "px";
    node.style.top = Math.max(0, Math.min(rect.top, innerHeight - 14)) + "px";
  }

  function render() {
    var host = box();
    for (var i = 0; i < live.items.length; i++) {
      var item = live.items[i];
      var on = item.label.indexOf(live.typed) === 0;
      if (!item.node) {
        item.node = document.createElement("div");
        item.node.style.cssText =
          "all:initial;position:absolute;" +
          "font:700 11px/1.15 " + live.theme.mono + ";" +
          "letter-spacing:0.5px;text-transform:uppercase;" +
          "padding:1px 4px;border-radius:" + live.theme.radius + ";" +
          "background:" + live.theme.accent + ";color:" + live.theme.bg + ";" +
          "box-shadow:0 1px 4px rgba(0,0,0,0.55);white-space:nowrap;";
        place(item.node, item.rect);
        host.appendChild(item.node);
      }
      item.node.style.display = on ? "block" : "none";
      if (!on) continue;
      // The part already typed stays visible but recedes, so the eye lands on
      // what is still to press rather than re-reading the whole label.
      item.node.textContent = "";
      if (live.typed) {
        var done = document.createElement("span");
        done.style.cssText = "all:inherit;opacity:0.4;";
        done.textContent = live.typed;
        item.node.appendChild(done);
      }
      item.node.appendChild(document.createTextNode(item.label.slice(live.typed.length)));
    }
  }

  function matches() {
    var out = [];
    for (var i = 0; i < live.items.length; i++) {
      if (live.items[i].label.indexOf(live.typed) === 0) out.push(live.items[i]);
    }
    return out;
  }

  function clear() {
    live = null;
    var host = document.getElementById(ID);
    if (host && host.parentNode) host.parentNode.removeChild(host);
  }

  function show(mode) {
    clear();
    var items = collect();
    if (!items.length) return 0;
    live = {
      mode: mode === "newtab" ? "newtab" : "click",
      typed: "",
      items: items,
      theme: {
        // A hint that matched the page would not be a hint. The accent is the
        // one theme colour chosen to stand off the canvas, which is exactly the
        // job here, and `newtab` uses the foreground so the two modes are told
        // apart at a glance rather than by memory.
        //
        // Not the *selection* colour, which is the obvious pick and the wrong
        // one: it is defined as a wash to sit *behind* the foreground, so on a
        // dark theme it lands a few percent off the canvas and the label goes
        // dark-on-dark. Both of these are chosen to carry text.
        accent: token(mode === "newtab" ? "fg" : "accent", "#ffd76e"),
        bg: token("bg", "#101014"),
        radius: "0px",
        mono: "ui-monospace,monospace"
      }
    };
    render();
    return items.length;
  }

  function activate(item, newtab) {
    var el = item.el;
    clear();

    if (newtab && el.tagName === "A" && el.href && el.protocol !== "javascript:") {
      location.href = SENTINEL + encodeURIComponent(el.href);
      return;
    }

    var tag = el.tagName;
    if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || el.isContentEditable) {
      try {
        el.focus();
        if (el.select) el.select();
      } catch (e) {}
      return;
    }

    // A real sequence rather than `el.click()`: sites listen for `mousedown` far
    // more often than for `click`, and a menu that opens on the first and closes
    // on the second needs both to arrive in order.
    var r = el.getBoundingClientRect();
    var init = {
      bubbles: true,
      cancelable: true,
      view: window,
      clientX: r.left + r.width / 2,
      clientY: r.top + r.height / 2
    };
    try {
      el.focus({ preventScroll: true });
    } catch (e) {}
    var types = ["mouseover", "mousedown", "mouseup", "click"];
    for (var i = 0; i < types.length; i++) {
      try {
        el.dispatchEvent(new MouseEvent(types[i], init));
      } catch (e) {}
    }
  }

  function stop(e) {
    e.preventDefault();
    e.stopPropagation();
    if (e.stopImmediatePropagation) e.stopImmediatePropagation();
  }

  function onKey(e) {
    if (live) {
      if (e.key === "Escape" || e.key === "Tab") {
        clear();
        stop(e);
        return;
      }
      if (e.key === "Backspace") {
        live.typed = live.typed.slice(0, -1);
        render();
        stop(e);
        return;
      }
      if (e.key === "Enter") {
        var only = matches();
        if (only.length) activate(only[0], e.shiftKey || live.mode === "newtab");
        else clear();
        stop(e);
        return;
      }
      if (e.key.length !== 1 || e.ctrlKey || e.altKey || e.metaKey) return;
      var ch = e.key.toLowerCase();
      if (CHARS.indexOf(ch) === -1) {
        clear();
        stop(e);
        return;
      }
      var newtab = e.shiftKey || live.mode === "newtab";
      live.typed += ch;
      var hits = matches();
      if (!hits.length) clear();
      else if (hits.length === 1 && hits[0].label === live.typed) activate(hits[0], newtab);
      else render();
      stop(e);
      return;
    }

    // No Ctrl-d / Ctrl-u here, deliberately. Both are already the browser's --
    // `bookmark add` and `page source` -- and both are bound on the GTK
    // toplevel with `propagate: false` (see `layout::BINDINGS`), so a page
    // handler for them can never fire. Measured: Ctrl-U opened the source in a
    // new tab and the page never saw the keystroke. Vimium's own half-page keys
    // are the bare `d` and `u` below anyway.
    if (e.ctrlKey || e.altKey || e.metaKey) return;
    if (typing()) return;

    // `gg` is two keystrokes, and the first one has to be remembered without
    // swallowing a `g` that turns out to be the start of something else. Half a
    // second is Vim's own feel for this.
    if (pendingG && e.key !== "g") pendingG = false;

    switch (e.key) {
      case "j": scroll(STEP, false); stop(e); return;
      case "k": scroll(-STEP, false); stop(e); return;
      case "d": scroll(page() / 2, false); stop(e); return;
      case "u": scroll(-page() / 2, false); stop(e); return;
      case "G": toEnd(true); stop(e); return;
      case "g":
        if (pendingG) {
          pendingG = false;
          clearTimeout(gTimer);
          toEnd(false);
          stop(e);
          return;
        }
        pendingG = true;
        gTimer = setTimeout(function () { pendingG = false; }, 500);
        stop(e);
        return;
      default:
        break;
    }

    if (e.key === "f") {
      show("click");
      stop(e);
    } else if (e.key === "F") {
      show("newtab");
      stop(e);
    }
  }

  // --------------------------------------------------------------------
  // Scrolling
  // --------------------------------------------------------------------
  //
  // Here rather than on the GTK toplevel for the same reason `f` is: these are
  // bare letters, and a toplevel accelerator would eat them in every search box
  // on the web. Only the page knows where the caret is, and `typing()` above is
  // already the thing that knows.

  var STEP = 64;
  var pendingG = false;
  var gTimer = null;

  // What actually scrolls. The document, when the document can -- but a great
  // many applications scroll an inner pane and leave `body` fixed, and on those
  // `window.scrollBy` is a no-op that looks like a broken key.
  function scroller() {
    var root = document.scrollingElement || document.documentElement;
    if (root && root.scrollHeight > root.clientHeight + 1) return root;
    var best = null;
    var boxes = document.querySelectorAll("div,main,section,article,ul,ol,pre,tbody");
    for (var i = 0; i < boxes.length && i < 2000; i++) {
      var el = boxes[i];
      if (el.scrollHeight <= el.clientHeight + 1) continue;
      var how = getComputedStyle(el).overflowY;
      if (how !== "auto" && how !== "scroll") continue;
      var area = el.clientWidth * el.clientHeight;
      if (!best || area > best.clientWidth * best.clientHeight) best = el;
    }
    return best || root;
  }

  function page() {
    var el = scroller();
    return Math.max(el.clientHeight || window.innerHeight, 1);
  }

  function scroll(by, smooth) {
    var el = scroller();
    // `scrollBy` on the scrolling element and on an inner pane are the same
    // call, which is the whole reason for picking an element rather than
    // branching on `window`.
    el.scrollBy({ top: by, left: 0, behavior: smooth ? "smooth" : "instant" });
  }

  function toEnd(bottom) {
    var el = scroller();
    el.scrollTo({ top: bottom ? el.scrollHeight : 0, left: 0, behavior: "smooth" });
  }

  // Capture, and installed at document-start, so a site's own key handling
  // cannot take the keystroke first.
  window.addEventListener("keydown", onKey, true);

  // Hints are pinned to viewport coordinates measured once. Anything that moves
  // the page under them makes every label point at the wrong thing, so the
  // honest response is to drop them rather than chase.
  var bail = function () {
    if (live) clear();
  };
  window.addEventListener("scroll", bail, true);
  window.addEventListener("resize", bail, true);
  window.addEventListener("mousedown", bail, true);
  window.addEventListener("blur", bail, true);

  // The command surface. `page hints` evaluates this, so the palette, the CLI
  // and MCP reach exactly the keystroke's behaviour.
  window.__omaHints = function (mode) {
    if (mode === "clear") {
      clear();
      return 0;
    }
    return show(mode);
  };
})();
