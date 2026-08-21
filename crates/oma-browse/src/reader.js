// Reader mode: the article, and nothing else.
//
// Evaluated once with the cleaned markup already in hand -- `page reader` runs
// `extract.js` first and passes its `html` in, so the "what is the article on
// this page" question is answered in exactly one place and reader mode and
// `page markdown` can never disagree about it.
//
// The document is replaced rather than restyled. Restyling means fighting the
// site's own stylesheet, its `!important`s and its layout scripts forever; a
// document with none of those in it needs no fighting. Leaving reader mode is a
// reload, which is honest about what happened.
(function (payload) {
  var article = payload.html;
  var title = payload.title;
  var base = payload.base;

  // The theme's own tokens, published by `page.js` beside this. Falling back to
  // something readable rather than to nothing: a reader that is black on black
  // because a token was missing is worse than one that ignores the theme.
  var t = (window.__oma && window.__oma.theme) || {};
  var fg = t.fg || "#e6e6e6";
  var accent = t.accent || "#7aa2f7";
  var faint = t.faint || fg;

  // The site's stylesheets go; ours stay. Wiping the whole `<head>` was the
  // first version of this and it took the theme's own injection with it -- the
  // veil, the readability floor that closes it over a bright wallpaper, and the
  // page recolouring -- leaving light text composited straight onto the
  // wallpaper. Everything this browser injects is marked; see
  // `AppState::page_script`.
  var theirs = document.querySelectorAll(
    'link[rel~="stylesheet"],style:not([id^="__oma_"]),script,iframe,object,embed'
  );
  for (var s = 0; s < theirs.length; s++) theirs[s].remove();

  var head = document.head || document.documentElement;

  // Relative links and images in the cleaned markup still point at the page
  // they came from, so the document has to keep saying where that was.
  if (!document.querySelector("base")) {
    var baseEl = document.createElement("base");
    baseEl.href = base;
    head.appendChild(baseEl);
  }

  document.title = title || document.title;

  var style = document.createElement("style");
  style.id = "__oma_reader";
  style.textContent = [
    // No background here: the theme already painted `html`, veil and all, and
    // painting it again would square the alpha -- the same trap
    // `AppState::background_color` documents for the webview itself.
    "html{color:" + fg + ";}",
    "body{background:none;margin:0;padding:4rem 1.5rem 6rem;font:16px/1.7 -apple-system,'Inter','Segoe UI',system-ui,sans-serif;}",
    "main{max-width:38rem;margin:0 auto;}",
    "h1{font-size:2rem;line-height:1.2;margin:0 0 2rem;}",
    "h2,h3,h4{line-height:1.3;margin:2.5rem 0 .75rem;}",
    "p,li,blockquote{font-size:1.05rem;}",
    "p{margin:0 0 1.25rem;}",
    "a{color:" + accent + ";}",
    "img,video,svg{max-width:100%;height:auto;border-radius:4px;}",
    "figure{margin:2rem 0;}",
    "figcaption,small{opacity:.7;font-size:.9rem;}",
    "pre{overflow-x:auto;padding:1rem;border-radius:6px;background:rgba(127,127,127,.12);}",
    "code{font-family:ui-monospace,'JetBrainsMono Nerd Font',monospace;font-size:.92em;}",
    "blockquote{margin:1.5rem 0;padding-left:1rem;border-left:2px solid " + accent + ";opacity:.9;}",
    "table{border-collapse:collapse;width:100%;margin:1.5rem 0;}",
    "th,td{border:1px solid rgba(127,127,127,.35);padding:.4rem .6rem;text-align:left;}",
    "hr{border:0;border-top:1px solid rgba(127,127,127,.35);margin:2.5rem 0;}",
    ".oma-reader-mark{margin:0 0 3rem;font-size:.8rem;letter-spacing:.08em;text-transform:uppercase;opacity:.55;color:" + faint + ";}"
  ].join("\n");
  head.appendChild(style);

  var main = document.createElement("main");
  var mark = document.createElement("p");
  mark.className = "oma-reader-mark";
  mark.textContent = "reader · reload to leave";
  main.appendChild(mark);

  if (title) {
    var h1 = document.createElement("h1");
    h1.textContent = title;
    main.appendChild(h1);
  }

  var body = document.createElement("div");
  body.innerHTML = article;
  // The extraction already dropped scripts, but `innerHTML` is the one place
  // where being wrong about that would matter, so it is checked again here.
  var scripts = body.querySelectorAll("script,style,link,iframe,object,embed");
  for (var i = 0; i < scripts.length; i++) scripts[i].remove();
  // A heading identical to the one just written is the article repeating its
  // own title, which every second article does.
  var first = body.querySelector("h1");
  if (first && title && first.textContent.trim() === title.trim()) first.remove();
  main.appendChild(body);

  document.body.innerHTML = "";
  document.body.appendChild(main);
  window.scrollTo(0, 0);
  return "reader";
})
