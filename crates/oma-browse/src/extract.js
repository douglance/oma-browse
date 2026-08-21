// The article on the page, as Markdown or as plain text.
//
// Not injected into every page like `inspect.js` and `hints.js`: this runs only
// when `page markdown` or `page text` asks for it, evaluated in place. A reader
// that costs every page something to have available is a reader nobody wants
// available.
//
// The extraction is Readability's idea in far less code -- score the candidate
// containers by how much prose they hold, take the best one -- and the
// conversion is a plain DOM walk. Neither is a general-purpose library, and the
// fallback when the scoring finds nothing worth keeping is the whole `body`,
// which is what a converter should do rather than returning nothing.
(function () {
  var STRIP = "script,style,noscript,template,svg,canvas,iframe,object,embed," +
    "nav,footer,aside,form,button,select,input,textarea,label," +
    "[aria-hidden=true],[hidden],[role=navigation],[role=banner]," +
    "[role=complementary],[role=search],[id^=__oma_],.__oma_browse_backer";

  var root = document.body ? document.body.cloneNode(true) : null;
  if (!root) return JSON.stringify({ title: "", markdown: "", text: "", chars: 0 });

  var doomed = root.querySelectorAll(STRIP);
  for (var i = 0; i < doomed.length; i++) doomed[i].remove();

  function proseLength(node) {
    var paragraphs = node.querySelectorAll("p,li,pre,blockquote");
    var total = 0;
    for (var i = 0; i < paragraphs.length; i++) {
      total += (paragraphs[i].textContent || "").trim().length;
    }
    return total;
  }

  // The obvious containers first: a page that says where its article is should
  // be believed rather than scored.
  var article = root.querySelector("article,main,[role=main]");
  if (!article || proseLength(article) < 200) {
    var best = null;
    var bestScore = 0;
    var boxes = root.querySelectorAll("div,section,article,main,td");
    for (var b = 0; b < boxes.length; b++) {
      var score = proseLength(boxes[b]);
      // Prefer the innermost box that still holds the prose: a wrapper scores
      // the same as its child, and the child is the one without the chrome.
      if (score > bestScore * 1.05) {
        best = boxes[b];
        bestScore = score;
      }
    }
    if (best && bestScore >= 200) article = best;
  }
  if (!article) article = root;

  // ------------------------------------------------------------------
  // DOM -> Markdown
  // ------------------------------------------------------------------

  var out = [];

  function esc(s) {
    return s.replace(/([\\`*_\[\]])/g, "\\$1");
  }

  function inline(node) {
    if (node.nodeType === 3) return node.nodeValue.replace(/\s+/g, " ");
    if (node.nodeType !== 1) return "";
    var tag = node.tagName.toLowerCase();
    var inner = children(node);
    switch (tag) {
      case "br": return "\n";
      case "strong": case "b": return inner.trim() ? "**" + inner.trim() + "**" : "";
      case "em": case "i": return inner.trim() ? "*" + inner.trim() + "*" : "";
      case "code": return inner.trim() ? "`" + inner.trim().replace(/`/g, "") + "`" : "";
      case "del": case "s": return inner.trim() ? "~~" + inner.trim() + "~~" : "";
      case "a":
        var href = node.getAttribute("href") || "";
        if (!inner.trim()) return "";
        if (!href || href.charAt(0) === "#") return inner;
        return "[" + inner.trim() + "](" + node.href + ")";
      case "img":
        var alt = node.getAttribute("alt") || "";
        return node.src ? "![" + alt + "](" + node.src + ")" : "";
      default:
        return inner;
    }
  }

  function children(node) {
    var parts = [];
    for (var i = 0; i < node.childNodes.length; i++) parts.push(inline(node.childNodes[i]));
    return parts.join("");
  }

  function text(node) {
    return children(node).replace(/[ \t]+/g, " ").trim();
  }

  function emit(line) {
    out.push(line);
  }

  function block(node, indent) {
    if (node.nodeType === 3) {
      var loose = node.nodeValue.replace(/\s+/g, " ").trim();
      if (loose) emit(indent + esc(loose));
      return;
    }
    if (node.nodeType !== 1) return;

    var tag = node.tagName.toLowerCase();
    switch (tag) {
      case "h1": case "h2": case "h3": case "h4": case "h5": case "h6": {
        var hashes = new Array(parseInt(tag.charAt(1), 10) + 1).join("#");
        var heading = text(node);
        if (heading) emit(indent + hashes + " " + heading);
        return;
      }
      case "p": {
        var body = text(node);
        if (body) emit(indent + body);
        return;
      }
      case "pre": {
        var code = (node.textContent || "").replace(/\s+$/, "");
        if (!code) return;
        var el = node.querySelector("code");
        var lang = "";
        var cls = (el && el.className) || node.className || "";
        var m = /(?:language|lang)-([\w+-]+)/.exec(cls);
        if (m) lang = m[1];
        // One entry, not one per line: blocks are joined with a blank line
        // between them, and a fence with blank lines inside it is not a fence.
        var fenced = [indent + "```" + lang];
        var rows = code.split("\n");
        for (var r = 0; r < rows.length; r++) fenced.push(indent + rows[r]);
        fenced.push(indent + "```");
        emit(fenced.join("\n"));
        return;
      }
      case "blockquote": {
        var inner = [];
        var was = out;
        out = inner;
        walk(node, "");
        out = was;
        var quoted = [];
        for (var q = 0; q < inner.length; q++) {
          quoted.push(indent + (inner[q] ? "> " + inner[q] : ">"));
        }
        if (quoted.length) emit(quoted.join("\n"));
        return;
      }
      case "ul": case "ol": {
        var index = 1;
        var listed = [];
        for (var c = 0; c < node.children.length; c++) {
          var item = node.children[c];
          if (item.tagName.toLowerCase() !== "li") continue;
          var marker = tag === "ol" ? index++ + ". " : "- ";
          var lines = [];
          var before = out;
          out = lines;
          walk(item, "");
          out = before;
          for (var l = 0; l < lines.length; l++) {
            if (!lines[l]) continue;
            listed.push(indent + (l === 0 ? marker : "  ") + lines[l]);
          }
        }
        // The whole list is one block: a blank line between the items would
        // make it a loose list, and between the rows of a table would stop it
        // being a table at all.
        if (listed.length) emit(listed.join("\n"));
        return;
      }
      case "table": {
        var rows = node.querySelectorAll("tr");
        var table = [];
        for (var t = 0; t < rows.length; t++) {
          var cells = rows[t].children;
          var values = [];
          for (var d = 0; d < cells.length; d++) values.push(text(cells[d]) || " ");
          if (!values.length) continue;
          table.push(indent + "| " + values.join(" | ") + " |");
          if (table.length === 1) {
            var rule = [];
            for (var u = 0; u < values.length; u++) rule.push("---");
            table.push(indent + "| " + rule.join(" | ") + " |");
          }
        }
        if (table.length) emit(table.join("\n"));
        return;
      }
      case "hr":
        emit(indent + "---");
        return;
      case "figure": case "picture":
        walk(node, indent);
        return;
      case "img": {
        var one = inline(node);
        if (one) emit(indent + one);
        return;
      }
      default:
        walk(node, indent);
    }
  }

  // A container whose children are all inline is a paragraph in everything but
  // name -- a `div` full of text, which is most of the web.
  function walk(node, indent) {
    var BLOCKS = /^(p|div|section|article|main|ul|ol|li|pre|blockquote|table|tr|h[1-6]|hr|figure|picture|header|dl|dd|dt)$/;
    var hasBlock = false;
    for (var i = 0; i < node.childNodes.length; i++) {
      var child = node.childNodes[i];
      if (child.nodeType === 1 && BLOCKS.test(child.tagName.toLowerCase())) {
        hasBlock = true;
        break;
      }
    }
    if (!hasBlock) {
      var line = text(node);
      if (line) emit(indent + line);
      return;
    }
    for (var c = 0; c < node.childNodes.length; c++) block(node.childNodes[c], indent);
  }

  var heading = document.querySelector("h1");
  var title = (document.title || (heading && heading.textContent) || "").trim();
  // Only when the article does not already open with one. A page whose <title>
  // and whose <h1> both say what it is -- which is most pages -- would otherwise
  // come out with two headings, and the second one is the real one.
  if (title && !article.querySelector("h1")) {
    emit("# " + title);
  }
  walk(article, "");

  // One blank line between blocks, never three, and none at either end.
  var markdown = [];
  for (var o = 0; o < out.length; o++) {
    var value = out[o];
    if (!value.trim()) continue;
    markdown.push(value);
  }
  var joined = markdown.join("\n\n").trim();
  var plain = (article.innerText || article.textContent || "")
    .replace(/[ \t]+/g, " ")
    .replace(/\n\s*\n\s*\n+/g, "\n\n")
    .trim();

  return JSON.stringify({
    title: title,
    url: location.href,
    markdown: joined,
    text: plain,
    // The cleaned article as markup, for `page reader`. The same node the two
    // above were rendered from, so what you read is what you would have piped.
    html: article.innerHTML,
    chars: joined.length
  });
})()
