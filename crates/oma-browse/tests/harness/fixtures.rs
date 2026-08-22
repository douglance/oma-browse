//! The web the end-to-end suite browses.
//!
//! A browser test that reaches the internet is a browser test that fails on a
//! train. Everything the suite loads is served from here: a few pages chosen so
//! that each one answers a question some command asks -- an article for
//! `page markdown`, a late-arriving element for `page wait`, an attachment for
//! the download commands, a `401` for `nav login`.
//!
//! Port zero, always. The first attempt at this bound a number somebody had
//! picked, which on a machine already running one of these silently served the
//! *other* suite's pages to this one's assertions. The kernel knows which ports
//! are free and is the only thing that does.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};

/// A page and the question it exists to answer.
struct Page {
    path: &'static str,
    body: &'static str,
}

const PAGES: &[Page] = &[
    Page {
        path: "/index.html",
        body: r#"<!doctype html>
<html><head><title>Fixture Home</title></head>
<body>
  <h1 id="heading">Fixture Home</h1>
  <p id="para">A needle hides in this haystack.</p>
  <a id="link" href="/article.html">Read the article</a>
  <a id="other" href="/form.html">Fill in a form</a>
  <button id="button" onclick="document.getElementById('heading').textContent = 'clicked'">Press</button>
  <script>
    console.log("fixture log line");
    console.warn("fixture warn line");
    console.error("fixture error line");
  </script>
</body></html>
"#,
    },
    // Long enough, and structured enough, that the readability pass has an
    // article to find. A three-word page extracts to nothing and makes
    // `page markdown` look broken when it is only being honest.
    Page {
        path: "/article.html",
        body: r#"<!doctype html>
<html><head><title>The Fixture Article</title></head>
<body>
  <nav id="chrome"><a href="/">home</a> <a href="/form.html">form</a></nav>
  <article>
    <h1>The Fixture Article</h1>
    <p>The first paragraph is long enough that a readability pass has something
    to weigh, because an extractor handed three words will decide the page has
    no article in it at all and answer with an empty string.</p>
    <p>The second paragraph exists so that the first is not the only one. Two
    paragraphs of real prose is the smallest thing that reliably reads as a
    body of text rather than as a heading with a caption under it.</p>
    <p>The third mentions a distinctive word, portmanteau, so that a search
    within the page has exactly one thing to find.</p>
  </article>
  <footer id="junk">Boilerplate that reader mode should drop.</footer>
</body></html>
"#,
    },
    Page {
        path: "/form.html",
        body: r#"<!doctype html>
<html><head><title>Fixture Form</title></head>
<body>
  <form id="form" onsubmit="return false">
    <input id="text-input" type="text" value="">
    <textarea id="text-area"></textarea>
    <div id="editable" contenteditable="true"></div>
  </form>
</body></html>
"#,
    },
    // For `page wait`: nothing matches at load, and both a selector and a string
    // turn up a moment later. The delay is small enough not to slow the suite
    // and long enough that a `wait` which returned immediately would be wrong.
    Page {
        path: "/slow.html",
        body: r#"<!doctype html>
<html><head><title>Fixture Slow</title></head>
<body>
  <p id="start">waiting</p>
  <script>
    setTimeout(function () {
      var p = document.createElement("p");
      p.id = "late";
      p.textContent = "the late arrival";
      document.body.appendChild(p);
    }, 700);
  </script>
</body></html>
"#,
    },
    // Asks for something the browser has to ask the user about, so
    // `permission decide` has a real question waiting. Geolocation rather than
    // the camera because it needs no hardware to be present: a machine with no
    // webcam refuses `getUserMedia` on its own, before anyone is asked.
    Page {
        path: "/geolocation.html",
        body: r#"<!doctype html>
<html><head><title>Fixture Geolocation</title></head>
<body>
  <h1 id="verdict">asking</h1>
  <script>
    navigator.geolocation.getCurrentPosition(
      function () { document.getElementById("verdict").textContent = "granted"; },
      function (e) { document.getElementById("verdict").textContent = "denied:" + e.code; }
    );
  </script>
</body></html>
"#,
    },
    // Loads a subresource, so `page network` has more than the document to show
    // and `--failed` has something real to narrow away from.
    Page {
        path: "/requests.html",
        body: r#"<!doctype html>
<html><head><title>Fixture Requests</title><link rel="stylesheet" href="/style.css"></head>
<body>
  <h1>Requests</h1>
  <script>fetch("/missing-on-purpose").catch(function () {});</script>
</body></html>
"#,
    },
];

/// The suite's own web server. Dropping it does not stop it; the process ending
/// does, which is the whole lifetime it needs.
pub struct Fixtures {
    pub port: u16,
}

impl Fixtures {
    /// Bind an ephemeral port and start serving.
    pub fn start() -> std::io::Result<Fixtures> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                // A thread per connection because WebKit opens several at once
                // for one page, and a serial server would deadlock the moment a
                // document waited on its own stylesheet.
                std::thread::spawn(move || {
                    let _ = serve(stream);
                });
            }
        });
        Ok(Fixtures { port })
    }

    /// The origin every fixture URL hangs off.
    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// A fixture URL, from a path like `/article.html`.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base())
    }
}

fn serve(mut stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();

    // Headers, to the blank line. Read rather than skipped: `/auth` answers
    // differently once the browser sends credentials, which is the only way to
    // tell a login that worked from one that was never attempted.
    let mut authorized = false;
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("authorization:") {
            authorized = value.trim().starts_with("basic ");
        }
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    // Drain a body if one was announced, so the client sees a clean close
    // rather than a reset in the middle of its own write.
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        let _ = reader.read_exact(&mut body);
    }

    if let Some(index) = path.find('?') {
        path.truncate(index);
    }
    if path == "/" {
        path = "/index.html".to_string();
    }

    let response = route(&path, authorized);
    stream.write_all(&response)?;
    stream.flush()
}

fn route(path: &str, authorized: bool) -> Vec<u8> {
    match path {
        // An attachment, so WebKit downloads it rather than rendering it. Small
        // on purpose: the download commands care that a file arrived, not how
        // long it took.
        "/download.bin" => reply(
            200,
            "application/octet-stream",
            b"fixture download payload\n",
            &[("content-disposition", "attachment; filename=\"fixture.bin\"")],
        ),
        // A password prompt for `nav login` to answer. The realm is named so a
        // failure message says which challenge went unanswered.
        "/auth" if !authorized => reply(
            401,
            "text/plain; charset=utf-8",
            b"unauthorized\n",
            &[("www-authenticate", "Basic realm=\"fixture\"")],
        ),
        "/auth" => reply(
            200,
            "text/html; charset=utf-8",
            b"<!doctype html><html><head><title>Fixture Secret</title></head>\
              <body><h1 id=\"secret\">the door opened</h1></body></html>",
            &[],
        ),
        "/style.css" => reply(200, "text/css; charset=utf-8", b"body { margin: 2rem; }\n", &[]),
        _ => match PAGES.iter().find(|page| page.path == path) {
            Some(page) => reply(200, "text/html; charset=utf-8", page.body.as_bytes(), &[]),
            None => reply(404, "text/plain; charset=utf-8", b"not here\n", &[]),
        },
    }
}

fn reply(status: u16, mime: &str, body: &[u8], extra: &[(&str, &str)]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        _ => "Not Found",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {mime}\r\ncontent-length: {}\r\n\
         cache-control: no-store\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}
