// The page-side runtime, injected into every loaded website.
//
// Lives in its own file rather than inside a Rust string literal so that it can
// be read, linted and `node --check`ed directly. `css.rs` pulls it in with
// `include_str!` and substitutes the config placeholder on the line below.
//
// Design note, because the obvious implementation is the wrong one: an earlier
// version walked every element, read its computed style, and wrote attributes
// that were themselves selectors in our stylesheet. That is O(elements) with a
// forced style recalc per element -- 1500 recalcs per animation frame on a big
// page -- and it goes permanently stale the moment a site re-renders, because
// the attributes stay behind on nodes whose styling has changed.
//
// This version reads the site's *stylesheets* instead. That is O(rules), needs
// no computed-style reads, touches no DOM, and cannot go stale because there is
// nothing to leave behind. Only two things still need real elements: finding
// which surfaces float over the page, and the opaque patches those floats need
// underneath them.
(function () {
  "use strict";

  var CFG = __OMA_CONFIG__;
  var STYLE_ID = "__oma_browse_theme";
  var OVERRIDE_ID = "__oma_browse_override";
  var VEIL_ID = "__oma_browse_veil";
  var BACKER_CLASS = "__oma_browse_backer";

  // Re-injection is normal: `restyle` evaluates this script into a live
  // document on every theme change and recolour toggle. Without a teardown the
  // previous instance's listeners, observers and patches all stay live -- which
  // is why turning recolour *off* used to leave it visibly on.
  if (window.__oma && typeof window.__oma.teardown === "function") {
    try {
      window.__oma.teardown();
    } catch (e) {}
  }

  var listeners = [];
  var observers = [];
  var backers = new Map();
  var floats = [];
  var sticky = [];

  window.__oma = {
    // The theme, for the injected scripts that are not this one. `link hints`
    // needs a colour that stands off the page, and the token block below is
    // published to *our own* chrome rather than to loaded sites, so reading
    // `--oma-accent` off the document root gets nothing.
    theme: {
      accent: CFG.accent,
      selection: CFG.selection,
      bg: "rgb(" + CFG.tint.join(",") + ")",
      fg: "rgb(" + CFG.fgRgb.join(",") + ")"
    },
    teardown: function () {
      for (var i = 0; i < listeners.length; i++) {
        listeners[i][0].removeEventListener(listeners[i][1], listeners[i][2], listeners[i][3]);
      }
      listeners.length = 0;
      for (var j = 0; j < observers.length; j++) observers[j].disconnect();
      observers.length = 0;
      backers.forEach(function (b) {
        if (b.parentNode) b.parentNode.removeChild(b);
      });
      backers.clear();
      for (var k = 0; k < floats.length; k++) floats[k].removeAttribute("data-oma-layer");
      floats.length = 0;
      sticky.length = 0;
      drop(OVERRIDE_ID);
      drop(VEIL_ID);
      drop(STYLE_ID);
    }
  };

  function drop(id) {
    var el = document.getElementById(id);
    if (el && el.parentNode) el.parentNode.removeChild(el);
  }

  function on(target, type, fn, opts) {
    target.addEventListener(type, fn, opts);
    listeners.push([target, type, fn, opts]);
  }

  function watch(observer) {
    observers.push(observer);
    return observer;
  }

  // ---------------------------------------------------------------- colour --

  function channels(value) {
    if (!value) return null;
    var v = String(value).trim();

    // Custom properties are stored as authored rather than normalised to
    // `rgb()` the way a known colour property is, so hex has to be parsed here
    // or every `--background-color-base: #fff` looks like a non-colour.
    var hex = v.match(/^#([0-9a-f]{3,8})$/i);
    if (hex) {
      var h = hex[1];
      if (h.length === 3 || h.length === 4) {
        h = h.split("").map(function (c) { return c + c; }).join("");
      }
      if (h.length !== 6 && h.length !== 8) return null;
      var out = [
        parseInt(h.slice(0, 2), 16),
        parseInt(h.slice(2, 4), 16),
        parseInt(h.slice(4, 6), 16),
        h.length === 8 ? parseInt(h.slice(6, 8), 16) / 255 : 1
      ];
      return out;
    }

    var m = v.match(/rgba?\(([^)]+)\)/i);
    if (m) {
      var p = m[1].split(/[\s,\/]+/).filter(Boolean).map(Number);
      if (p.length < 3) return null;
      for (var i = 0; i < 3; i++) if (!isFinite(p[i])) return null;
      if (p.length < 4 || !isFinite(p[3])) p[3] = 1;
      return p;
    }

    // Everything else the engine calls a colour: `black`, `hsl()`, `color-mix()`,
    // `lab()`. Wikipedia is the case that proved this matters -- its infobox is
    // `.infobox { color: black }`, and CSSOM hands a declaration back as
    // *authored*, so this saw the word rather than `rgb(0, 0, 0)`, called it
    // "not a colour", and left the text black on a surface it had just made
    // transparent. A keyword list would fix `black` and miss the next syntax;
    // asking the engine cannot go out of date.
    var normal = normalise(v);
    return normal === v ? null : channels(normal);
  }

  // The engine's own colour parser, borrowed. Assigning a canvas `fillStyle`
  // normalises anything it recognises to `#rrggbb` or `rgba(...)` and silently
  // *keeps the previous value* for anything it does not -- which is the whole
  // test, done against two different sentinels so a value that happens to equal
  // one of them is not mistaken for a rejection.
  var swatch = null;
  var normalised = {};

  function normalise(value) {
    if (Object.prototype.hasOwnProperty.call(normalised, value)) return normalised[value];
    var out = value;
    try {
      if (!swatch) swatch = document.createElement("canvas").getContext("2d");
      if (swatch) {
        swatch.fillStyle = "#010203";
        swatch.fillStyle = value;
        var first = swatch.fillStyle;
        if (first !== "#010203") {
          out = first;
        } else {
          swatch.fillStyle = "#040506";
          swatch.fillStyle = value;
          // Still moved, so `#010203` really was the answer the first time.
          if (swatch.fillStyle !== "#040506") out = first;
        }
      }
    } catch (e) {
      /* No canvas (a CSP that forbids it, a document with no view). */
    }
    // Bounded: a page has far fewer distinct colour literals than rules that
    // mention them, and this runs once per rule per declaration on every
    // recolour.
    if (Object.keys(normalised).length < 4000) normalised[value] = out;
    return out;
  }

  function lightness(value) {
    var p = channels(value);
    if (!p || p[3] < 0.03) return null;
    return (0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]) / 255;
  }

  // Only repaint surfaces the site left grey. A channel spread this small means
  // the colour carries no brand intent, so replacing it is safe; anything more
  // saturated is the site's own identity and must survive theming.
  function isNeutral(value) {
    var p = channels(value);
    if (!p || p[3] < 0.03) return false;
    return Math.max(p[0], p[1], p[2]) - Math.min(p[0], p[1], p[2]) <= 34;
  }

  // ------------------------------------------------------------ stylesheets --

  // Rules recovered from stylesheets this document is not allowed to read.
  //
  // A big site almost never serves its CSS from its own origin: Amazon's desktop
  // UI is eight `m.media-amazon.com` bundles, and every one of them throws on
  // `.cssRules`. Skipping them is why the page stayed white -- 7,288 readable
  // rules and not one of them painted the background. Fetching the same URL over
  // CORS and parsing it into a detached document gives back rules that are
  // ordinary objects, with no effect on what the page renders.
  var foreign = [];
  var fetched = {};

  function eachStyleRule(fn) {
    var sheets = document.styleSheets;
    for (var i = 0; i < sheets.length; i++) {
      var sheet = sheets[i];
      var owner = sheet.ownerNode;
      if (owner && (owner.id === STYLE_ID || owner.id === OVERRIDE_ID)) continue;
      var rules;
      try {
        rules = sheet.cssRules;
      } catch (e) {
        continue;
      }
      walkRules(rules, "", fn);
    }
    for (var k = 0; k < foreign.length; k++) walkRules(foreign[k], "", fn);
  }

  // A scratch document to parse into. Its stylesheets are real and readable but
  // participate in no layout, so this costs a parse and nothing else.
  var scratch = null;
  function parseSheet(text) {
    if (!scratch) scratch = document.implementation.createHTMLDocument("");
    var style = scratch.createElement("style");
    style.textContent = text;
    scratch.head.appendChild(style);
    try {
      return style.sheet ? style.sheet.cssRules : null;
    } catch (e) {
      return null;
    }
  }

  // Fetch every sheet the same-origin policy hid from us, then re-run once they
  // have all answered. Sites whose CDN refuses CORS simply stay as they are --
  // the inline-style and computed-colour passes still cover what they paint.
  function recoverForeign(done) {
    var sheets = document.styleSheets;
    var wanted = [];
    for (var i = 0; i < sheets.length; i++) {
      var href = sheets[i].href;
      if (!href || fetched[href]) continue;
      try {
        sheets[i].cssRules;
        continue;
      } catch (e) {
        fetched[href] = true;
        wanted.push(href);
      }
    }
    if (!wanted.length || typeof fetch !== "function") return;
    var left = wanted.length;
    var gained = false;
    for (var j = 0; j < wanted.length; j++) {
      (function (url) {
        fetch(url, { mode: "cors", credentials: "omit" })
          .then(function (r) { return r.ok ? r.text() : null; })
          .then(function (text) {
            if (text) {
              var rules = parseSheet(text);
              if (rules && rules.length) {
                foreign.push(rules);
                gained = true;
              }
            }
          })
          .catch(function () {})
          .then(function () {
            if (--left === 0 && gained) done();
          });
      })(wanted[j]);
    }
  }

  function walkRules(rules, wrap, fn) {
    for (var i = 0; i < rules.length; i++) {
      var rule = rules[i];
      if (rule.selectorText && rule.style) {
        fn(rule, wrap);
      } else if (rule.cssRules) {
        // Keep @media context so an override cannot leak into a breakpoint the
        // original rule did not apply to.
        var next = rule.media && rule.media.mediaText ? "@media " + rule.media.mediaText : wrap;
        walkRules(rule.cssRules, next, fn);
      }
    }
  }

  // The page's own base lightness, taken from what the site *authored* rather
  // than from what is on screen. Reading it back off `html`/`body` at runtime
  // measures our own veil and reports that a plain white page is as far from
  // its own background as it is possible to be.
  // What the page actually paints, sampled off the screen with our own styling
  // switched off for the duration.
  //
  // A site that states its canvas in CSS is easy; a single-page app that paints
  // it onto some inner shell div and leaves `html` and `body` transparent tells
  // us nothing, and guessing white there inverts a site that was already dark --
  // ChatGPT rendered as a flat grey slab for exactly that reason. Nine hit tests
  // and a walk up to the first painted ancestor answer the question directly.
  //
  // This costs one restyle and runs once per load, not per pass; the cached
  // answer is what every later pass reads.
  var measured;
  var siteVars = null;
  var pendingVarNames = null;
  var measuredFinal = false;

  /// Read the site's own resolved custom properties, our sheets switched off.
  ///
  /// Authored text is not enough. A site that ships both a light and a dark
  /// palette declares each variable more than once -- in a `prefers-color-scheme`
  /// block, or under `[data-theme]` -- and picking the last declaration mixes
  /// the two. Wikipedia was the case that proved it: its canvas came out of the
  /// light palette and its content container out of the dark one, so the
  /// container looked far from the canvas and got painted an opaque grey over
  /// what should have been the veil. The engine has already done this cascade;
  /// ask it rather than redo it.
  function readSiteVars(names) {
    var out = {};
    var cs = window.getComputedStyle(document.documentElement);
    for (var i = 0; i < names.length; i++) {
      var value = cs.getPropertyValue(names[i]);
      if (value) out[names[i]] = value.trim();
    }
    return out;
  }

  // Also the one place the site's resolved custom properties are snapshotted:
  // the read has to happen with our own sheets off, and this is the only window
  // where they are. A cached measurement must therefore not short-circuit past
  // a pending snapshot, or the map would be built from authored text forever.
  function measureBase() {
    if (measured !== undefined && pendingVarNames === null) return measured;
    if (!document.body || document.readyState === "loading") return null;
    var mine = [document.getElementById(OVERRIDE_ID), document.getElementById(STYLE_ID)];
    for (var m = 0; m < mine.length; m++) {
      if (mine[m] && mine[m].sheet) mine[m].sheet.disabled = true;
    }
    // Same disabled window, so this costs no extra restyle.
    if (pendingVarNames) {
      siteVars = readSiteVars(pendingVarNames);
      pendingVarNames = null;
    }
    if (measured !== undefined) {
      for (var q = 0; q < mine.length; q++) {
        if (mine[q] && mine[q].sheet) mine[q].sheet.disabled = false;
      }
      return measured;
    }
    var veil = document.getElementById(VEIL_ID);
    var shown = veil ? veil.style.display : null;
    if (veil) veil.style.display = "none";

    // The document's own background, resolved. This is the canvas whenever the
    // site states one at all, and it is media-query correct by construction.
    var vals = [];
    var rootLum = lightness(getComputedStyle(document.documentElement).backgroundColor);
    var bodyLum = lightness(getComputedStyle(document.body).backgroundColor);
    if (bodyLum !== null) vals.push(bodyLum);
    else if (rootLum !== null) vals.push(rootLum);

    for (var x = 1; x <= 3; x++) {
      for (var y = 1; y <= 3; y++) {
        var el = document.elementFromPoint(
          Math.round((window.innerWidth * x) / 4),
          Math.round((window.innerHeight * y) / 4)
        );
        while (el) {
          if (!el.classList || !el.classList.contains(BACKER_CLASS)) {
            var l = lightness(getComputedStyle(el).backgroundColor);
            if (l !== null) {
              vals.push(l);
              break;
            }
          }
          el = el.parentElement;
        }
      }
    }

    if (veil) veil.style.display = shown;
    for (var r = 0; r < mine.length; r++) {
      if (mine[r] && mine[r].sheet) mine[r].sheet.disabled = false;
    }
    // Median, so one dark hero image behind three of the nine points cannot
    // decide the answer for the whole page.
    if (!vals.length) return null;
    vals.sort(function (a, b) { return a - b; });
    measured = vals[Math.floor(vals.length / 2)];
    return measured;
  }

  function pageBase() {
    var found = null;
    var vars = {};
    var scheme = null;
    eachStyleRule(function (rule) {
      // Collect every root-level custom property on the way past. Sites state
      // their canvas as `html { background: var(--surface) }` far more often
      // than as a literal, and an unresolved `var()` used to read as "no
      // background at all", i.e. white -- which inverted every site that ships
      // its own dark theme.
      var sel = rule.selectorText;
      var root = /(^|,)\s*(html|body|:root)\s*($|,)/i.test(sel);
      if (root) {
        for (var ci = 0; ci < rule.style.length; ci++) {
          var prop = rule.style[ci];
          if (prop.slice(0, 2) === "--") vars[prop] = rule.style.getPropertyValue(prop).trim();
        }
        var cs = rule.style.getPropertyValue("color-scheme");
        if (cs) scheme = cs;
      }
      // Match rather than pattern-match the selector text. X authors both
      // `:root { background: #fff }` and `:root[data-theme="dark"] { background:
      // #000 }`; a textual test takes the first and decides the site is white,
      // which then inverts the whole page. Asking the document which rule
      // actually applies gets the right one, and taking the last keeps the
      // cascade's own answer.
      if (!root || !applies(sel)) return;
      var l = lightness(resolveVars(rule.style.getPropertyValue("background-color"), vars));
      if (l !== null) found = l;
    });
    // What the engine resolved wins over what any single rule authored: a site
    // with a light and a dark palette authors both, and only one is in force.
    var seen = measureBase();
    if (seen !== null && seen !== undefined) return seen;
    if (found !== null) return found;
    // Nothing in CSS: fall back to the legacy attribute before assuming white.
    if (document.body) {
      var attr = lightness(attrColour(document.body.getAttribute("bgcolor")));
      if (attr !== null) return attr;
    }
    // A site that declares itself dark but states its canvas somewhere we could
    // not follow is still a dark site, and treating it as white would invert it.
    if (scheme && /dark/i.test(scheme) && !/light/i.test(scheme)) return 0.08;
    // A site that paints no background at all leaves the browser canvas
    // showing, which is white on the overwhelming majority of the web.
    return 1;
  }

  // Whether a selector currently matches the document root or its body. Selector
  // text a site can author but this engine cannot parse simply does not count.
  function applies(sel) {
    var parts = sel.split(",");
    for (var i = 0; i < parts.length; i++) {
      var one = parts[i].trim();
      try {
        if (document.documentElement.matches(one)) return true;
        if (document.body && document.body.matches(one)) return true;
      } catch (e) {
        continue;
      }
    }
    return false;
  }

  // Follow `var(--a, fallback)` through root-level declarations. Bounded to a
  // few hops: design systems chain aliases, but not deeply, and a cycle must not
  // hang the page.
  // A `background` shorthand, taken apart into the longhand we actually map.
  //
  // This is not a nicety. CSSOM leaves every longhand of a shorthand that
  // contains `var()` serializing as the empty string -- "pending-substitution"
  // -- so `background: var(--surface)` reports no `background-color` at all.
  // Reading only the longhand therefore skipped the background of every site
  // that states it through a token, which is most design systems now:
  // omarchy.org's own buttons kept their brand fill not because `isNeutral`
  // spared them but because nothing here ever saw them.
  //
  // The engine's parser does the work rather than a regex, because the
  // shorthand is a real grammar -- `#fff url(x) no-repeat` is a colour and an
  // image and three keywords -- and getting that wrong quietly repaints the
  // wrong half of it. The probe is detached and never inserted, so setting a
  // property on it parses and normalises without touching layout.
  var probe = null;

  function fromShorthand(value, longhand) {
    if (!value || value.indexOf("var(") !== -1) return "";
    if (!probe) probe = document.createElement("div");
    probe.style.cssText = "";
    try {
      probe.style.background = value;
    } catch (e) {
      return "";
    }
    return probe.style.getPropertyValue(longhand);
  }

  // The background colour a rule paints, whichever way it says it.
  function backgroundOf(rule, vars, longhand) {
    var direct = rule.style.getPropertyValue(longhand);
    if (direct) return resolveVars(direct, vars);
    // Resolved first: the probe cannot parse a `var()` any more than the rule
    // could, so substitution has to happen before the engine sees it.
    var short = resolveVars(rule.style.getPropertyValue("background"), vars);
    return fromShorthand(short, longhand);
  }

  function resolveVars(value, vars) {
    var v = value;
    for (var hop = 0; hop < 4; hop++) {
      if (!v || v.indexOf("var(") === -1) return v;
      var m = v.match(/var\(\s*(--[\w-]+)\s*(?:,\s*([^()]*))?\)/);
      if (!m) return v;
      var next = vars[m[1]];
      if (next === undefined) next = m[2] === undefined ? "" : m[2].trim();
      v = v.replace(m[0], next);
    }
    return v;
  }

  // How far a colour sits from the page's own canvas, on a 0..1 scale where 0 is
  // the canvas itself and 1 is as far from it as the page goes.
  //
  // Measuring *depth* rather than absolute lightness is what lets one mapping
  // serve both a white site and a site that already ships its own dark theme:
  // on the first, depth counts downward from white, on the second it counts
  // upward from near-black, and in both cases the page's body text lands near 1.
  function depth(l, base) {
    var span = base > 0.5 ? base : 1 - base;
    if (span < 0.05) span = 0.05;
    var t = (base > 0.5 ? base - l : l - base) / span;
    return t < 0 ? 0 : t > 1 ? 1 : t;
  }

  // A colour t of the way along the theme's own canvas-to-foreground ramp.
  //
  // Interpolating between the two theme colours rather than snapping to a
  // handful of pre-baked surfaces is the whole point. A site's palette is a
  // *sequence* -- gray-100 through gray-900, or a scrim at five opacities -- and
  // collapsing that sequence onto two or three colours destroys the structure
  // the design depended on. ChatGPT is the case that proved it: 52 of its
  // design tokens landed on the identical foreground hex, which painted the
  // document lavender. Keeping the ramp continuous keeps the order intact, and
  // because both ends come from the theme every step is Omarchy-tinted.
  function ramp(t, alpha) {
    t = t < 0 ? 0 : t > 1 ? 1 : t;
    var c = [];
    for (var i = 0; i < 3; i++) {
      c.push(Math.round(CFG.tint[i] + (CFG.fgRgb[i] - CFG.tint[i]) * t));
    }
    return "rgba(" + c[0] + "," + c[1] + "," + c[2] + "," + alpha + ")";
  }

  // Backgrounds live in the bottom third of the ramp. A surface is meant to sit
  // behind text, so however light the site painted it, it has to stay dark
  // enough here for the foreground to read against it.
  var BG_COMPRESS = 0.34;
  // Below this the colour *is* the page canvas, and the right answer is to paint
  // nothing at all so the veil -- and the wallpaper under it -- comes through.
  var CANVAS_BAND = 0.1;

  function mapBackground(value, base) {
    var p = channels(value);
    var l = lightness(value);
    if (!p || l === null) return null;
    var t = depth(l, base);
    if (t < CANVAS_BAND) return "transparent";
    // Surfaces further from the canvas read as more solid, so a card sits *on*
    // the translucent page rather than dissolving into it.
    var a = Math.min(1, CFG.opacity + 0.14 + t * 0.5);
    return ramp(t * BG_COMPRESS, p[3] < 1 ? p[3] * a : a);
  }

  // Foregrounds start most of the way up the ramp so that even the palest grey a
  // site uses for a caption stays legible over the veil, and keep the site's own
  // hierarchy above that: body text lands on the theme foreground exactly.
  //
  // The floor is high because the thing behind the text is not a solid colour --
  // it is the wallpaper, at whatever brightness that pixel happens to be. Text
  // that would read fine against a fixed dark panel disappears over the light
  // half of a photograph.
  var FG_FLOOR = 0.55;

  function mapForeground(value, base) {
    var p = channels(value);
    var l = lightness(value);
    if (!p || l === null) return null;
    return ramp(FG_FLOOR + (1 - FG_FLOOR) * depth(l, base), p[3]);
  }

  // WCAG relative luminance, which is not the same thing as `lightness` above.
  // That one is a cheap weighted average and is all the ramp needs; picking a
  // readable foreground is a contrast question, and contrast is only defined on
  // linearised channels. Using the cheap one here picks the wrong colour on
  // mid-tone brand fills, which is precisely the case this is for.
  function relativeLuminance(p) {
    var c = [];
    for (var i = 0; i < 3; i++) {
      var v = p[i] / 255;
      c.push(v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4));
    }
    return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
  }

  function contrastRatio(a, b) {
    return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
  }

  // WCAG's large-text threshold. Button and chip labels are the thing this
  // guards, and they are bold or uppercase far more often than not.
  var LEGIBLE_MIN = 3;

  // The better of the theme's two ends against a background we are not touching.
  //
  // Only ever the theme's own foreground or its canvas, so this stays correct
  // under every Omarchy theme without knowing anything about which one is on:
  // a light brand fill gets the canvas, a dark one gets the foreground, and a
  // theme whose ends are unusually close still gets whichever is further away.
  function readableOn(value) {
    var p = channels(value);
    if (!p) return null;
    var bg = relativeLuminance(p);
    var toFg = contrastRatio(bg, relativeLuminance(CFG.fgRgb));
    var toCanvas = contrastRatio(bg, relativeLuminance(CFG.tint));
    var best = Math.max(toFg, toCanvas);
    // A floor, not a preference. Some theme-and-brand pairings have no legible
    // answer inside the palette at all -- Everforest's foreground on Stripe's
    // indigo is 2.78:1, Latte's on Hacker News orange is 2.72:1 -- and a label
    // nobody can read is a worse outcome than one pixel of the page not being
    // Omarchy-tinted. Only reached when both theme ends fail, which across the
    // stock themes is a handful of cases out of sixty.
    if (best < LEGIBLE_MIN) {
      return contrastRatio(bg, 1) >= contrastRatio(bg, 0) ? "#fff" : "#000";
    }
    var pick = toFg >= toCanvas ? CFG.fgRgb : CFG.tint;
    return "rgb(" + pick[0] + "," + pick[1] + "," + pick[2] + ")";
  }

  // A variable whose name gives no clue how it will be used. Treat it as a
  // surface, but never clear it: an unknown token forced to `transparent` is
  // invisible text if the site happened to be using it for a label.
  function mapUnknown(value, base) {
    var p = channels(value);
    var l = lightness(value);
    if (!p || l === null) return null;
    var t = depth(l, base);
    if (t < CANVAS_BAND) return ramp(0, Math.min(1, CFG.opacity + 0.14));
    return ramp(t * BG_COMPRESS, p[3] < 1 ? p[3] : Math.min(1, CFG.opacity + 0.14 + t * 0.5));
  }

  function rgbToHsl(r, g, b) {
    r /= 255; g /= 255; b /= 255;
    var max = Math.max(r, g, b), min = Math.min(r, g, b);
    var l = (max + min) / 2, h = 0, sat = 0;
    if (max !== min) {
      var d = max - min;
      sat = l > 0.5 ? d / (2 - max - min) : d / (max + min);
      if (max === r) h = (g - b) / d + (g < b ? 6 : 0);
      else if (max === g) h = (b - r) / d + 2;
      else h = (r - g) / d + 4;
      h /= 6;
    }
    return [h, sat, l];
  }

  // A site's *coloured* surfaces: Wikipedia's pastel section bars, a tinted
  // callout, a highlighted table row. `isNeutral` deliberately spares these so
  // that brand colour survives, but a pastel is not brand colour -- it is a
  // decoration chosen to sit on white, and left alone it reads as a bright
  // stripe across a dark page.
  //
  // Keeping the hue and moving only the lightness preserves what the colour was
  // *for* -- the section bars stay distinguishable from each other -- while
  // putting it in the same band as every other surface. A saturated colour that
  // is already dark, or one strong enough to be an actual brand element like a
  // blue submit button, falls outside the band and is untouched.
  var TINT_BAND = 0.45;

  function mapTintedBackground(value, base) {
    // Only on a light page. The band below picks out colours *near the canvas*,
    // which on a light site means pastels -- but on a site that already ships a
    // dark theme it means the saturated accents, and Instagram's primary button
    // came back a washed-out slab the first time this ran unguarded.
    if (base <= 0.5) return null;
    var p = channels(value);
    var l = lightness(value);
    if (!p || l === null) return null;
    var t = depth(l, base);
    if (t > TINT_BAND) return null;
    var hsl = rgbToHsl(p[0], p[1], p[2]);
    var sat = Math.min(hsl[1], 0.45);
    var lit = 0.1 + t * 0.55;
    return "hsla(" + Math.round(hsl[0] * 360) + "," + Math.round(sat * 100) + "%," +
      Math.round(lit * 100) + "%," + Math.min(1, CFG.opacity + 0.2) * (p[3] < 1 ? p[3] : 1) + ")";
  }

  // Gradients, which are a background but not a `background-color`.
  //
  // The "fade out to the page background" device -- a linear-gradient from
  // transparent to white -- is everywhere, and Google's collapsed result panel
  // is a white slab across the middle of the window without this. Rewriting each
  // colour stop through the same mapping turns the fade into a fade to nothing,
  // which is what it was always meant to be.
  var COLOUR_TOKEN = /#[0-9a-f]{3,8}\b|rgba?\([^()]*\)/gi;

  function mapGradient(value, base) {
    if (!value || value.indexOf("gradient(") === -1) return null;
    var changed = false;
    var out = value.replace(COLOUR_TOKEN, function (tok) {
      var mapped = isNeutral(tok) ? mapBackground(tok, base) : mapTintedBackground(tok, base);
      if (!mapped) return tok;
      changed = true;
      return mapped;
    });
    return changed ? out : null;
  }

  // Which end of the ramp a custom property belongs to.
  //
  // A variable carries no clue about how it will be used, and the same hex can
  // legitimately be a page background or body text. The name is the only signal
  // a site reliably gives us. Where the name says nothing the answer is
  // "unknown" rather than a guess: guessing from lightness is what mapped
  // ChatGPT's entire `--gray-*` and `--*-transparent-*` primitive scale onto the
  // foreground colour, because a mid-grey primitive on a dark page is far from
  // the canvas and every distance test therefore called it text.
  //
  // Borders are deliberately left alone: they match `color` by name but painting
  // them with the foreground turns every hairline rule into a bright stripe.
  function varDirection(name) {
    var n = name.toLowerCase();
    if (/(^|-)border(-|$)/.test(n)) return null;
    if (/(^|-)(bg|background|surface|fill|canvas|backdrop|scrim|overlay)(-|$)/.test(n)) return "bg";
    if (/(^|-)(color|text|fg|foreground|ink|label|content|heading|caption|placeholder)(-|$)/.test(n)) return "fg";
    return "auto";
  }

  // One rule's worth of overrides, appended to `out`.
  //
  // Lifted out of `buildOverrides` so a single rule can be derived on its own,
  // which is what makes the cache below possible. Pure in everything that
  // matters: the output is a function of the rule, the page's measured `base`
  // and the resolved `varMap`, and nothing else.
  function emitRule(rule, wrap, base, varMap, out) {
    var decls = "";
    // Set when the background below turns out to be the site's own brand
    // colour, which we leave alone; the foreground pass then leaves its
    // partner alone too.
    var brandBg = null;
    // That partner is emitted separately, because it is the one declaration
    // here that has to outrank `a:any-link`. See the push below.
    var pairDecl = "";

    // Custom properties first. Retinting the variable retints every rule that
    // reads it, which on a site like Wikipedia is nearly all of them -- its
    // surfaces come almost entirely through `--background-color-*` tokens, so
    // rewriting concrete declarations alone leaves the page untouched.
    for (var ci = 0; ci < rule.style.length; ci++) {
      var prop = rule.style[ci];
      if (prop.slice(0, 2) !== "--") continue;
      if (prop.indexOf("--oma-") === 0) continue;
      var raw = resolveVars(rule.style.getPropertyValue(prop).trim(), varMap);
      var dir = varDirection(prop);
      if (!dir) continue;
      var mapped;
      if (isNeutral(raw)) {
        mapped =
          dir === "bg" ? mapBackground(raw, base)
          : dir === "fg" ? mapForeground(raw, base)
          : mapUnknown(raw, base);
      } else {
        // A coloured token still has to come down out of the light band, or a
        // pastel declared once as a variable reappears as a bright stripe
        // everywhere the site reads it.
        mapped = dir === "fg" ? null : mapTintedBackground(raw, base);
      }
      if (!mapped) continue;
      // `transparent` is the point, not a failure: a surface sitting at the
      // page's own base lightness is the page canvas, and letting the veil
      // through it is the whole design. Wikipedia paints its entire content
      // column from `--background-color-base`, so refusing to clear that one
      // variable leaves the site opaque white over the veil.
      decls += prop + ":" + mapped + " !important;";
    }

    var bg = backgroundOf(rule, varMap, "background-color");
    if (bg && isNeutral(bg)) {
      // The page canvas is the veil's job, always. Whatever neutral a site
      // paints on `html`/`body` is by definition its base, and letting it
      // stand -- even remapped to a dark theme grey -- puts an opaque sheet
      // between the wallpaper and the window. Sites that state the canvas
      // through a `var()` chain we could not follow land here too, which is
      // why this is a selector test rather than a lightness one.
      var isRoot = /(^|,)\s*(html|body|:root)\s*($|,)/i.test(rule.selectorText);
      var mb = isRoot ? "transparent" : mapBackground(bg, base);
      if (mb) decls += "background-color:" + mb + " !important;";
    } else if (bg) {
      var mt = mapTintedBackground(bg, base);
      if (mt) decls += "background-color:" + mt + " !important;";
      // Nothing mapped means we are keeping the site's own fill: brand colour,
      // by the rule `isNeutral` states. That decision reaches past the
      // background -- see `brandBg` below.
      else if (lightness(bg) !== null) brandBg = bg;
    }
    var img = backgroundOf(rule, varMap, "background-image");
    var mg = mapGradient(img, base);
    if (mg) decls += "background-image:" + mg + " !important;";
    var fg = resolveVars(rule.style.getPropertyValue("color"), varMap);
    // A foreground declared alongside a background we are keeping is half of a
    // pair, not an independent colour. The site picked those two to read
    // against each other, and mapping one end of a pair onto the theme ramp
    // while the other end keeps its brand value is what turns a legible button
    // into two colours of the same weight -- omarchy.org's own call-to-action
    // buttons came back accent-on-brand-blue, at about 1.1:1.
    //
    // So: keep the pair. The literal is emitted rather than the `var()` the
    // site wrote, because the custom-property pass above has already retinted
    // that variable for its use as a background elsewhere.
    if (brandBg) {
      var pair = fg || readableOn(brandBg);
      // A translucent fill is mostly the page underneath it, so the site's
      // pairing was never against this colour alone and `readableOn` would be
      // answering the wrong question. Only a declared colour is trustworthy
      // there; with none, fall through and leave the cascade alone.
      var solid = (channels(brandBg) || [0, 0, 0, 1])[3] >= 0.9;
      if (pair && (fg || solid)) pairDecl = "color:" + pair + " !important;";
    } else if (fg && isNeutral(fg)) {
      var mf = mapForeground(fg, base);
      if (mf) decls += "color:" + mf + " !important;";
    }
    if (decls) {
      var text = rule.selectorText + "{" + decls + "}";
      out.push(wrap ? wrap + "{" + text + "}" : text);
    }
    // The brand pair, raised above `css.rs`'s `a:any-link { !important }`.
    //
    // That rule is (0,1,1) and has to stay there, or a site's neutral-grey
    // links stop reading as links. But a link styled as a button is exactly
    // the case where it is wrong, and the site's own selector is usually
    // below it -- `.button` is (0,1,0). Two `:root`s buy (0,2,0) and `:is()`
    // carries the original selector at its own weight without needing the
    // selector list split, so `.button` lands at (0,3,0) and wins. The same
    // device, for the same reason, as the canvas rule at the end of this
    // function.
    if (pairDecl) {
      var lifted = ":root:root :is(" + rule.selectorText + "){" + pairDecl + "}";
      out.push(wrap ? wrap + "{" + lifted + "}" : lifted);
    }
  }

  // Per-rule derivation, remembered until its inputs change.
  //
  // `emitRule` is a pure function of the rule, the page's measured `base` and
  // the resolved `varMap`. A page load runs it over every rule in the document
  // eleven times -- 27,000 rules, ~270ms a pass, ~3.0s of a 4.4s load on
  // youtube.com -- and those passes cannot be skipped, because each one
  // genuinely sees stylesheets the last did not.
  //
  // What they mostly do not see is *new inputs*. Measured across those eleven
  // passes: `base` takes three distinct values and the `varMap` signature five.
  // So a rule derived under the same generation would compute the same answer
  // again, and a pass only has to derive rules that are new or whose inputs
  // moved.
  //
  // The generation is the whole of what `emitRule` reads besides the rule
  // itself, which is what makes this sound rather than a guess: if the
  // signature is unchanged then every input is unchanged. `WeakMap`, so a
  // rule's entry goes when its stylesheet does.
  //
  // `wrap` is absent from the signature on purpose, and this is the invariant
  // that keeps it safe to be: `walkRules` derives it purely from the rule's
  // position in the CSSOM tree, and a `CSSStyleRule` occupies one position for
  // its lifetime -- so it is already a function of the key. Extending
  // `walkRules` to `@supports` or `@container` is fine while it stays that way.
  // Folding anything that changes at runtime into `wrap` -- which breakpoint
  // matched, a container's size -- would make cached output stale, and it would
  // not show up as a wrong colour on any one page.
  var ruleCache = new WeakMap();
  // The custom properties each rule declares. Keyed by rule and never
  // invalidated: see the comment at its only use.
  var varCache = new WeakMap();
  var genId = 0;
  var genSig = null;

  function generation(base, varMap, names) {
    var sig = JSON.stringify(base);
    for (var i = 0; i < names.length; i++) {
      sig += names[i] + "\u0001" + varMap[names[i]] + "\u0002";
    }
    if (sig !== genSig) {
      genSig = sig;
      genId++;
    }
    return genId;
  }

  function buildOverrides() {
    var out = [];

    // Anything measured before the document finished loading is provisional.
    // Wikipedia is the case that proved it: its dark palette arrives as a
    // `skin-theme-clientpref-os` class its own script puts on `<html>`, so at
    // DOMContentLoaded the page is still white and every custom property is
    // still unset. Caching that reading made the canvas light, the content
    // container dark, and the container therefore "far from the canvas" -- so
    // it got painted an opaque grey over what should have been the veil.
    // Re-read until `load`, then cache for good.
    if (!measuredFinal) {
      measured = undefined;
      siteVars = null;
      pendingVarNames = null;
      if (document.readyState === "complete") measuredFinal = true;
    }

    // Every custom property the document declares, anywhere, so an alias can be
    // followed to the literal it eventually names. Design systems alias heavily
    // -- ChatGPT's canvas is `--theme-surface-canvas: var(--black-transparent-
    // 1000-a85)` -- and a value that is a `var()` reference is not a colour, so
    // without this the alias is skipped and the page keeps its own background.
    // Built from authored text rather than computed values so that re-running
    // reads the site's declarations again and not our own output.
    var authored = {};
    var names = [];
    eachStyleRule(function (rule) {
      // Which custom properties a rule declares, and their authored text, is a
      // property of the rule alone -- no `base`, no `varMap`, nothing that moves
      // between passes. So unlike the derivation below this needs no generation
      // and can be remembered for good.
      //
      // It is worth remembering because this is a second full walk of every
      // declaration of every rule in the document, on top of the one that emits:
      // ~65ms per pass over youtube.com's 27,000 rules, eleven times a load, and
      // for the great majority of rules the answer is "none".
      var mine = varCache.get(rule);
      if (mine === undefined) {
        mine = null;
        for (var vi = 0; vi < rule.style.length; vi++) {
          var name = rule.style[vi];
          if (name.slice(0, 2) === "--" && name.indexOf("--oma-") !== 0) {
            (mine || (mine = [])).push(name, rule.style.getPropertyValue(name).trim());
          }
        }
        varCache.set(rule, mine);
      }
      if (!mine) return;
      for (var mi = 0; mi < mine.length; mi += 2) {
        if (authored[mine[mi]] === undefined) names.push(mine[mi]);
        authored[mine[mi]] = mine[mi + 1];
      }
    });
    if (siteVars === null && pendingVarNames === null) {
      pendingVarNames = names;
    }
    // `pageBase` triggers the snapshot, so it has to run before the map is read.
    var base = pageBase();
    // Resolved values where the engine could give them, authored text where it
    // could not -- a variable declared only inside a rule that does not apply
    // has no computed value, and its authored text is the only thing there is.
    var varMap = {};
    for (var ni = 0; ni < names.length; ni++) {
      var key = names[ni];
      varMap[key] = (siteVars && siteVars[key]) || authored[key];
    }

    var gen = generation(base, varMap, names);
    eachStyleRule(function (rule, wrap) {
      var hit = ruleCache.get(rule);
      if (hit && hit.gen === gen) {
        for (var hi = 0; hi < hit.out.length; hi++) out.push(hit.out[hi]);
        return;
      }
      var mine = [];
      emitRule(rule, wrap, base, varMap, mine);
      ruleCache.set(rule, { gen: gen, out: mine });
      for (var mi = 0; mi < mine.length; mi++) out.push(mine[mi]);
    });
    // Unconditionally, last, so it wins on document order: the canvas belongs to
    // the veil. The per-rule pass above only fires on a neutral literal, and a
    // site that states its background as `var(--surface)` -- or through a sheet
    // whose CORS headers refused us -- would otherwise keep painting over the
    // wallpaper.
    // Repeated `:root` purely for specificity: sites state their canvas from
    // selectors like `:root[data-theme="dark"]`, which outranks a plain `html`
    // however important it is declared.
    out.push(":root:root:root,:root:root:root body{background-color:transparent !important;}");
    return { css: out.join("\n"), base: base };
  }

  // Colours a stylesheet cannot see: inline `style=` and the legacy
  // presentational attributes. Both get rewritten in place.
  //
  // Reading `el.style.*` is a parsed-attribute read rather than a computed one,
  // so none of this forces layout.
  // Our own chrome, injected into the page: the veil, the backers, the strip's
  // inset, link-hint labels. It is themed already, and running it through the
  // site pass would retint it to whatever the site's palette happens to imply.
  function ours(el) {
    if (el.classList && el.classList.contains(BACKER_CLASS)) return true;
    try {
      return !!(el.closest && el.closest('[id^="__oma_browse_"]'));
    } catch (e) {
      return false;
    }
  }

  function fixElementColours(base) {
    var nodes = document.querySelectorAll('[style*="background"],[style*="color"],[style*="gradient"]');
    for (var i = 0; i < nodes.length; i++) {
      var el = nodes[i];
      if (ours(el)) continue;
      var brandInline = null;
      var bg = el.style.backgroundColor;
      if (bg && isNeutral(bg)) {
        // Same rule as the stylesheet pass: the canvas belongs to the veil. X
        // sets it from script straight onto `<html>`, where no sheet can be
        // overridden because there is no rule to override.
        var root = el === document.documentElement || el === document.body;
        var mb = root ? "transparent" : mapBackground(bg, base);
        if (mb) el.style.setProperty("background-color", mb, "important");
      } else if (bg) {
        var mt = mapTintedBackground(bg, base);
        if (mt) el.style.setProperty("background-color", mt, "important");
        else if (lightness(bg) !== null) brandInline = bg;
      }
      var mg = mapGradient(el.style.backgroundImage, base);
      if (mg) el.style.setProperty("background-image", mg, "important");
      var fg = el.style.color;
      // The same pair rule the stylesheet pass applies, for the sites that write
      // their colours onto the element instead of into a sheet.
      if (brandInline) {
        var inlinePair = fg || readableOn(brandInline);
        var inlineSolid = (channels(brandInline) || [0, 0, 0, 1])[3] >= 0.9;
        if (inlinePair && (fg || inlineSolid)) {
          el.style.setProperty("color", inlinePair, "important");
        }
      } else if (fg && isNeutral(fg)) {
        var mf = mapForeground(fg, base);
        if (mf) el.style.setProperty("color", mf, "important");
      }
    }

    // `bgcolor` and friends are older than CSS and appear in no stylesheet at
    // all. Hacker News paints its entire page this way -- `<body bgcolor>` plus
    // a `<table bgcolor>` -- so a rules-only pass leaves it untouched.
    var legacy = document.querySelectorAll("[bgcolor],[text]");
    for (var j = 0; j < legacy.length; j++) {
      var node = legacy[j];
      var attrBg = attrColour(node.getAttribute("bgcolor"));
      if (attrBg && isNeutral(attrBg)) {
        var mapped = mapBackground(attrBg, base);
        if (mapped) node.style.setProperty("background-color", mapped, "important");
      }
      var attrFg = attrColour(node.getAttribute("text"));
      if (attrFg && isNeutral(attrFg)) {
        var mappedFg = mapForeground(attrFg, base);
        if (mappedFg) node.style.setProperty("color", mappedFg, "important");
      }
    }
  }

  // Presentational attributes allow bare hex with no leading `#`.
  function attrColour(raw) {
    if (!raw) return null;
    var v = raw.trim();
    return /^[0-9a-f]{3,8}$/i.test(v) ? "#" + v : v;
  }

  // ----------------------------------------------------------------- floats --

  // Anything that paints over its siblings rather than beside them. A surface
  // we make transparent stops occluding whatever scrolls beneath it, and for a
  // sticky header that is a bug you can read off the screen: headlines slide up
  // *through* the site's own navigation.
  //
  // `absolute` is deliberately excluded even though it is out of flow. Its rect
  // scrolls with the content while the patch underneath it is viewport-fixed,
  // so the two detach on every frame. `relative` is excluded too -- it stays in
  // flow and displaces its siblings instead of covering them.
  var FLOAT_HINTS =
    "header,nav,dialog,[role=dialog],[role=banner],[style*=fixed],[style*=sticky]";

  function floatCandidates() {
    var selectors = [FLOAT_HINTS];
    eachStyleRule(function (rule) {
      var p = rule.style.getPropertyValue("position");
      if (p === "fixed" || p === "sticky" || p === "-webkit-sticky") {
        selectors.push(rule.selectorText);
      }
    });
    var seen = new Set();
    for (var i = 0; i < selectors.length; i++) {
      var nodes;
      try {
        nodes = document.querySelectorAll(selectors[i]);
      } catch (e) {
        continue;
      }
      for (var j = 0; j < nodes.length; j++) seen.add(nodes[j]);
    }
    return Array.from(seen);
  }

  function findFloats() {
    for (var i = 0; i < floats.length; i++) floats[i].removeAttribute("data-oma-layer");
    floats = [];
    sticky = [];

    var candidates = floatCandidates();
    var keep = [];
    for (var k = 0; k < candidates.length; k++) {
      var el = candidates[k];
      if (ours(el)) continue;
      var cs;
      try {
        cs = getComputedStyle(el);
      } catch (e) {
        continue;
      }
      var pos = cs.position;
      if (pos !== "fixed" && pos !== "sticky" && pos !== "-webkit-sticky") continue;
      // Only neutral surfaces need this. One in the site's own brand colour is
      // still opaque after theming, so nothing bleeds through it anyway.
      if (!isNeutral(cs.backgroundColor)) continue;
      keep.push({ el: el, sticky: pos !== "fixed" });
    }

    // Drop anything nested inside another float, so blurs cannot stack.
    for (var a = 0; a < keep.length; a++) {
      var nested = false;
      for (var b = 0; b < keep.length; b++) {
        if (a !== b && keep[b].el.contains(keep[a].el)) {
          nested = true;
          break;
        }
      }
      if (nested) continue;
      keep[a].el.setAttribute("data-oma-layer", "float");
      floats.push(keep[a].el);
      if (keep[a].sticky) sticky.push(keep[a].el);
    }
  }

  // ---------------------------------------------------------------- backers --

  // Give the blur something to blur.
  //
  // `backdrop-filter` samples an opaque backdrop and nothing else -- verified in
  // this engine and in stock MiniBrowser at backdrop alpha 0, 0.5 and 1.0.
  // On a transparent page there is nothing behind a sticky header but glyphs
  // floating on air, so the filter spreads their brightness outward and the
  // result reads as bloom rather than blur.
  //
  // The fix is an opaque patch the exact size of the float, laid *under* the
  // page content on a negative z-index. Negative-z children paint before
  // in-flow block backgrounds, so content still draws on top and the float's
  // backdrop becomes opaque-plus-text: blurrable. That rectangle was already
  // hidden behind the float, so no transparency is lost.
  function backerFor(el) {
    var b = backers.get(el);
    if (b && b.isConnected) return b;
    b = document.createElement("div");
    b.className = BACKER_CLASS;
    b.setAttribute("aria-hidden", "true");
    document.body.appendChild(b);
    backers.set(el, b);
    return b;
  }

  function syncBackers() {
    if (!document.body) return;
    // Read every rect first, then write every style. Interleaving them makes
    // each write invalidate the next read and costs one forced layout per
    // float, per frame.
    var rects = [];
    for (var i = 0; i < floats.length; i++) rects.push(floats[i].getBoundingClientRect());
    for (var j = 0; j < floats.length; j++) {
      var r = rects[j];
      var b = backerFor(floats[j]);
      if (r.width < 2 || r.height < 2) {
        b.style.display = "none";
        continue;
      }
      b.style.display = "block";
      b.style.left = r.left + "px";
      b.style.top = r.top + "px";
      b.style.width = r.width + "px";
      b.style.height = r.height + "px";
    }
    backers.forEach(function (b, el) {
      if (floats.indexOf(el) === -1) {
        if (b.parentNode) b.parentNode.removeChild(b);
        backers.delete(el);
      }
    });
  }

  var pending = false;
  function queueBackers() {
    if (pending) return;
    pending = true;
    requestAnimationFrame(function () {
      pending = false;
      syncBackers();
    });
  }

  // ------------------------------------------------------------------ apply --

  function ensureSheet(id, css) {
    var style = document.getElementById(id);
    if (!style) {
      style = document.createElement("style");
      style.id = id;
      (document.head || document.documentElement).appendChild(style);
    }
    if (style.textContent !== css) style.textContent = css;
    // Keep our sheets last in the document. At document-start there is no
    // `<head>` yet, so they land on `<html>` ahead of everything the parser is
    // about to insert -- and `!important` loses to a site's own `!important` on
    // document order.
    var head = document.head;
    if (head && head.lastElementChild !== style) head.appendChild(style);
    return style;
  }

  function recolour() {
    if (!CFG.recolor) return;
    var built = buildOverrides();
    ensureSheet(OVERRIDE_ID, built.css);
    fixElementColours(built.base);
    findFloats();
    queueBackers();
    // Kick off recovery of anything cross-origin. It resolves after this pass,
    // so it re-enters here once -- and only once, because every href it touches
    // is marked before the request goes out.
    recoverForeign(recolour);
  }

  // The veil is a real element, not a background on `html`.
  //
  // WebKit does not paint the document root's background at all when the web
  // view's own background colour is transparent -- which ours is, so that CSS
  // can own the alpha rather than compounding with it. Proven by forcing
  // `html { background: red !important }`: `getComputedStyle` reports
  // `rgb(255, 0, 0)` and the window samples nothing of the sort. Every page was
  // therefore running with no veil at all, and the whole thing looked
  // see-through because it literally was.
  //
  // A plain fixed element underneath everything paints exactly as expected.
  function ensureVeil() {
    if (!document.body) return;
    var veil = document.getElementById(VEIL_ID);
    if (!veil) {
      veil = document.createElement("div");
      veil.id = VEIL_ID;
      veil.setAttribute("aria-hidden", "true");
    }
    // First child, and re-asserted on every apply: single-page apps replace
    // large parts of the body and would otherwise take it with them.
    if (veil.parentNode !== document.body || document.body.firstChild !== veil) {
      document.body.insertBefore(veil, document.body.firstChild);
    }
  }

  function apply() {
    if (!document.documentElement) return;
    ensureSheet(STYLE_ID, CFG.css);
    ensureVeil();
    recolour();
  }

  apply();
  if (document.readyState === "loading") {
    on(document, "DOMContentLoaded", apply, { once: true });
  }
  on(window, "load", apply, { once: true });

  if (CFG.recolor) {
    // Only re-read stylesheets when a stylesheet actually changes. The old
    // version observed `childList` over the whole subtree, so every DOM change
    // a site made cost a full-document walk.
    var sheetPending = false;
    var sheetObserver = watch(
      new MutationObserver(function (records) {
        for (var i = 0; i < records.length; i++) {
          var nodes = records[i].addedNodes;
          for (var j = 0; j < nodes.length; j++) {
            var n = nodes[j];
            if (n.nodeType !== 1) continue;
            var tag = n.tagName;
            if (tag === "STYLE" || (tag === "LINK" && n.rel === "stylesheet")) {
              if (n.id === STYLE_ID || n.id === OVERRIDE_ID) continue;
              if (sheetPending) return;
              sheetPending = true;
              requestAnimationFrame(function () {
                sheetPending = false;
                recolour();
              });
              return;
            }
          }
        }
      })
    );
    sheetObserver.observe(document.documentElement, { childList: true, subtree: true });

    // Floats appear and disappear as a site routes between views. Re-find them
    // on a slow tick rather than per mutation: a wrong float costs a missing
    // blur for a moment, not a broken page.
    var refloat = setInterval(function () {
      ensureVeil();
      findFloats();
      queueBackers();
    }, 1000);
    observers.push({ disconnect: function () { clearInterval(refloat); } });

    // A `position: fixed` float has a scroll-invariant rect, so scrolling costs
    // nothing at all unless something is actually sticky.
    on(
      window,
      "scroll",
      function () {
        if (sticky.length) queueBackers();
      },
      true
    );
    on(window, "resize", queueBackers);
  }
})();
