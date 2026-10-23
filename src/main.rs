//! pouriya-www: the backend of pouriya.net.
//!
//! Serves the front end from `--www`, with `index.html`, `software.html` and
//! `contact.html` filled in from the settings below, and keeps who came in
//! SQLite (`--db`): one row per device, one per page open, one per link
//! followed to another site or handle copied, one per message sent from the
//! contact page, and a per-day counter of robots turned away. None of it
//! is shown on the site: only on the admin page, at `--admin-path`, behind
//! a password and a captcha. The contact email is never written into the
//! page; it is handed out only for a solved captcha, and a message needs one
//! too.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    fmt::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::{
    Json, Router,
    extract::{ConnectInfo, MatchedPath, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use clap::{Parser, Subcommand};
use rusqlite::{
    Connection, OptionalExtension, params, params_from_iter,
    types::{Value, ValueRef},
};
use serde::{Deserialize, Serialize};
use tower_http::{services::ServeDir, set_header::SetResponseHeaderLayer};
use tracing_subscriber::{filter::LevelFilter, fmt as subscriber};

/// The site's name: the tab title and the title of a link preview.
const SITE_TITLE: &str = "pouriya.net";

/// Under the title in a link preview, and the page's meta description.
const SITE_DESCRIPTION: &str = "Pouriya Jahanbakhsh";

/// `{{github}}` in `index.html`.
const LINK_GITHUB: &str = "https://github.com/pouriya";

/// `{{telegram}}` in `index.html`: the plogia channel.
const LINK_TELEGRAM: &str = "https://t.me/plogia";

/// `{{blog}}` in `index.html`.
const LINK_BLOG: &str = "https://plogia.ir";

/// Never written into the page, so a scraper reading the HTML does not find
/// it: `POST /api/email` hands it out for a solved captcha only.
const EMAIL: &str = "pouriya.jahanbakhsh@gmail.com";

/// Characters in every captcha. The crate's presets pick 4 to 6 at random, so
/// the picture is drawn here as its `Lucy` preset at `Medium` difficulty, with
/// this many characters.
const CAPTCHA_LENGTH: u32 = 5;

/// How long a captcha may wait for its answer. It gets one try either way.
const CAPTCHA_LIFETIME: Duration = Duration::from_secs(600);

/// Captchas held in memory awaiting an answer; past this the oldest is
/// dropped.
const CAPTCHA_LIMIT: usize = 1000;

/// Client addresses given a captcha in any one second; each may have one per
/// second. The rest get a `429`.
const CAPTCHA_ADDRESSES_PER_SECOND: usize = 100;

/// The name on a message, in characters. Required.
const MESSAGE_NAME_LIMIT: usize = 100;

/// A message's text: at most this many words, characters and UTF-8 bytes,
/// all three. Required.
const MESSAGE_WORD_LIMIT: usize = 1000;
const MESSAGE_CHARACTER_LIMIT: usize = 5000;
const MESSAGE_BYTE_LIMIT: usize = 10000;

/// The optional way to reply to a message's sender, in characters, and the
/// kinds the front end may name it as.
const MESSAGE_CONTACT_LIMIT: usize = 200;
const MESSAGE_CONTACT_KINDS: [&str; 4] = ["phone", "telegram", "email", "other"];

/// Longest header or client-sent string kept in a `visit` row, in characters.
/// A referrer is rarely over 300; this only stops a hostile one filling the
/// disk.
const TEXT_LIMIT: usize = 512;

/// An admin session ends after this long without a request. Sessions are held
/// in memory only, so a restart ends them all.
const ADMIN_SESSION_IDLE: Duration = Duration::from_secs(3600);

/// The cookie holding the admin session token, sent to `/api/admin` only.
const ADMIN_COOKIE: &str = "pouriya-www-admin";

/// Rows per page the admin page may ask for, smallest first. The largest is
/// also the most ids one request may delete or mark.
const ADMIN_PAGE_SIZES: [usize; 3] = [20, 50, 100];

/// Lowercase User-Agent fragments that mark a robot. Cheap and incomplete on
/// purpose: a browser-faking crawler gets through this, but almost none of
/// those run the JavaScript that sends a visit.
const ROBOT_MARKERS: [&str; 22] = [
    "bot",
    "crawl",
    "spider",
    "slurp",
    "headless",
    "preview",
    "scrape",
    "fetch",
    "curl",
    "wget",
    "python",
    "java/",
    "go-http",
    "okhttp",
    "axios",
    "libwww",
    "httpclient",
    "lighthouse",
    "phantomjs",
    "facebookexternalhit",
    "whatsapp",
    "monitor",
];

/// Serve pouriya.net and count its visitors.
///
/// Every page load reads its HTML from `--www` on disk, so a change to the
/// front end is live on the next load. Meant to sit behind a reverse proxy that
/// terminates TLS; the client address is read from `Forwarded`,
/// `X-Forwarded-For` or `X-Real-IP`, in that order.
#[derive(Debug, Parser)]
#[command(
    version,
    author,
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct CommandLine {
    #[command(subcommand)]
    action: Option<Action>,

    /// The front end: `index.html`, `software.html`, `contact.html`,
    /// `admin.html` and every file they link to, served as-is.
    #[arg(long, value_name = "DIRECTORY", required = true)]
    www: Option<PathBuf>,

    /// SQLite database for devices, visits, clicks, messages and robot
    /// counters. Created if missing.
    #[arg(long, value_name = "FILE", required = true)]
    db: Option<PathBuf>,

    /// Where visitors reach the site, like `https://pouriya.net`: the
    /// canonical link and the link-preview URL.
    #[arg(long, value_name = "URL", required = true)]
    url: Option<String>,

    /// TCP port to listen on, on every interface.
    #[arg(long, required = true)]
    port: Option<u16>,

    /// Where the admin page is served, like `/some-unguessable-word`. Nothing
    /// links to it; its API is under `/api/admin`.
    #[arg(long, value_name = "PATH", required = true)]
    admin_path: Option<String>,

    /// The admin password's argon2 hash, hex-encoded so that no shell, quote
    /// or deploy form can mangle its `$`s: what `pouriya-www hash-hex`
    /// prints.
    #[arg(long, value_name = "HEX", required = true)]
    admin_hash_hex: Option<String>,

    /// Enable trace level logging (shows target and location).
    #[arg(long)]
    trace: bool,

    /// Enable debug level logging (shows target).
    #[arg(long)]
    debug: bool,

    /// Disable all logging.
    #[arg(long)]
    quiet: bool,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Read the admin password from stdin, one line, and print its argon2
    /// hash, hex-encoded, for `--admin-hash-hex`.
    HashHex,
}

struct App {
    www: PathBuf,
    /// `--url` without a trailing `/`.
    url: String,
    /// `--admin-hash-hex` decoded, checked at startup to parse.
    admin_hash: String,
    /// Admin sessions: token, last request. Each ends `ADMIN_SESSION_IDLE`
    /// after its last request, or at logout.
    sessions: Mutex<Vec<(String, Instant)>>,
    database: Mutex<Connection>,
    /// Captchas shown and not yet answered: id, answer, when made. At most
    /// `CAPTCHA_LIMIT`, oldest first; each is good for `CAPTCHA_LIFETIME` and
    /// one try.
    captchas: Mutex<VecDeque<(String, String, Instant)>>,
    /// Who was given a captcha in the last second, at most
    /// `CAPTCHA_ADDRESSES_PER_SECOND` of them.
    recent: Mutex<Vec<(IpAddr, Instant)>>,
}

/// 48 bits of Unix milliseconds then 80 random bits, big-endian, written as 26
/// Crockford base32 characters — borhan's ULID without its error enum. No
/// monotonic factory: two made in one millisecond sort arbitrarily.
struct Ulid([u8; 16]);

/// Crockford's base32: no `I`, `L`, `O` or `U`.
const ULID_ALPHABET: [u8; 32] = *b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

impl Ulid {
    fn new() -> anyhow::Result<Self> {
        let milliseconds = now() as u64;
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&milliseconds.to_be_bytes()[2..]);
        if let Err(error) = getrandom::fill(&mut bytes[6..]) {
            bail!("Could not read random bytes from the operating system: {error}");
        }
        Ok(Self(bytes))
    }

    /// Case-insensitive, since a ULID that went through localStorage, a URL or
    /// a copy-paste may come back lowercased. `None` for anything that is not
    /// exactly 26 Crockford digits starting at or below `7`.
    fn parse(text: &str) -> Option<Self> {
        if text.len() != 26 {
            return None;
        }
        let mut value: u128 = 0;
        for (index, character) in text.bytes().enumerate() {
            let upper = character.to_ascii_uppercase();
            let mut digit = None;
            for (position, letter) in ULID_ALPHABET.iter().enumerate() {
                if *letter == upper {
                    digit = Some(position as u128);
                }
            }
            let digit = digit?;
            // 26 characters carry 130 bits; the first has only 3 to spend.
            if index == 0 && digit > 0b111 {
                return None;
            }
            value = (value << 5) | digit;
        }
        Some(Self(value.to_be_bytes()))
    }
}

impl fmt::Display for Ulid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = u128::from_be_bytes(self.0);
        for index in 0..26 {
            let shift = 5 * (25 - index);
            formatter.write_char(char::from(
                ULID_ALPHABET[((value >> shift) & 0b11111) as usize],
            ))?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings = CommandLine::parse();

    if let Some(Action::HashHex) = settings.action {
        let mut password = String::new();
        std::io::stdin()
            .read_line(&mut password)
            .context("Could not read the password from stdin")?;
        let password = password.trim_end_matches(['\r', '\n']);
        if password.is_empty() {
            bail!("No password on stdin");
        }
        let mut salt = [0u8; 16];
        if let Err(error) = getrandom::fill(&mut salt) {
            bail!("Could not read random bytes from the operating system: {error}");
        }
        match Argon2::default().hash_password_with_salt(password.as_bytes(), &salt) {
            Ok(hash) => println!("{}", hex(hash.to_string().as_bytes())),
            Err(error) => bail!("Could not hash the password: {error}"),
        }
        return Ok(());
    }
    // Required by clap unless there is a subcommand, and there was none.
    let (Some(www), Some(db), Some(given_url), Some(port), Some(admin_path), Some(admin_hash_hex)) = (
        settings.www,
        settings.db,
        settings.url,
        settings.port,
        settings.admin_path,
        settings.admin_hash_hex,
    ) else {
        bail!("--www, --db, --url, --port, --admin-path and --admin-hash-hex are all required");
    };

    let level = if settings.quiet {
        LevelFilter::OFF
    } else if settings.trace {
        LevelFilter::TRACE
    } else if settings.debug {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };
    subscriber::Subscriber::builder()
        .with_max_level(level)
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(true)
        .log_internal_errors(true)
        .with_level(true)
        .with_file(level == LevelFilter::TRACE)
        .with_line_number(level == LevelFilter::TRACE)
        .with_target(matches!(level, LevelFilter::DEBUG | LevelFilter::TRACE))
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_writer(std::io::stderr)
        .init();

    // Checked once here so that a typo'd path fails at startup, naming the
    // flag, rather than as a 500 on the first visitor.
    for file in ["index.html", "software.html", "contact.html", "admin.html"] {
        if !www.join(file).is_file() {
            bail!(
                "--www {} has no {file}; it must be the front-end directory",
                www.display()
            );
        }
    }
    let url = given_url.trim_end_matches('/').to_string();
    match url.split_once("://") {
        Some(("http" | "https", rest)) if !rest.is_empty() => {}
        _ => bail!("--url {given_url} must start with http:// or https:// and name a host"),
    }
    // A plain path of its own: nothing a route pattern would read as a
    // parameter, and no page, API or front-end file already there.
    let mut plain = admin_path.len() > 1
        && admin_path.starts_with('/')
        && !admin_path.ends_with('/')
        && !admin_path.contains("//");
    for character in admin_path.chars() {
        if !(character.is_ascii_alphanumeric() || "/-_.~".contains(character)) {
            plain = false;
        }
    }
    if !plain {
        bail!(
            "--admin-path {admin_path} must be a path like /some-word: letters, digits and - _ . ~, no trailing /"
        );
    }
    let first = match admin_path[1..].split_once('/') {
        Some((first, _)) => first,
        None => &admin_path[1..],
    };
    if matches!(first, "software" | "contact" | "go" | "api") || www.join(first).exists() {
        bail!("--admin-path {admin_path} is taken by a page, the API or a file in --www");
    }
    let digits = admin_hash_hex.as_bytes();
    let mut decoded = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks(2) {
        let mut byte = None;
        if pair.len() == 2
            && pair[0].is_ascii_hexdigit()
            && pair[1].is_ascii_hexdigit()
            && let Ok(text) = std::str::from_utf8(pair)
            && let Ok(parsed) = u8::from_str_radix(text, 16)
        {
            byte = Some(parsed);
        }
        match byte {
            Some(byte) => decoded.push(byte),
            None => bail!(
                "--admin-hash-hex must be hex digits only, an even number of them; make it with `pouriya-www hash-hex`"
            ),
        }
    }
    let Ok(admin_hash) = String::from_utf8(decoded) else {
        bail!(
            "--admin-hash-hex is not a hex-encoded argon2 hash; make it with `pouriya-www hash-hex`"
        );
    };
    if let Err(error) = PasswordHash::new(&admin_hash) {
        bail!(
            "--admin-hash-hex is not a hex-encoded argon2 hash ({error}); make it with `pouriya-www hash-hex`"
        );
    }

    let connection =
        Connection::open(&db).with_context(|| format!("Could not open --db {}", db.display()))?;
    connection.busy_timeout(Duration::from_secs(30))?;
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;

             -- One row per browser, made by the first POST /api/visit from it.
             -- The id is a ULID, so its first 6 bytes are created_at again.
             CREATE TABLE IF NOT EXISTS device (
                 id BLOB PRIMARY KEY,
                 created_at INTEGER NOT NULL
             ) WITHOUT ROWID;

             -- One row per page open, the device's first included.
             CREATE TABLE IF NOT EXISTS visit (
                 id INTEGER PRIMARY KEY,
                 device BLOB NOT NULL REFERENCES device (id),
                 at INTEGER NOT NULL,
                 ip TEXT NOT NULL,
                 referrer TEXT NOT NULL,
                 language TEXT NOT NULL,
                 path TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS visit_device ON visit (device, at);

             -- One row per link to another site followed through GET /go
             -- (our own pages are plain links: their visit is the record), and
             -- per handle or email
             -- copied or email link opened, from POST /api/click: what, from
             -- which page, the device when the page knew it and the database
             -- does too, and `action`, `open` or `copy`.
             CREATE TABLE IF NOT EXISTS click (
                 id INTEGER PRIMARY KEY,
                 at INTEGER NOT NULL,
                 ip TEXT NOT NULL,
                 link TEXT NOT NULL,
                 page TEXT NOT NULL,
                 device BLOB REFERENCES device (id),
                 action TEXT NOT NULL
             );

             -- One row per message sent from the contact page with a solved
             -- captcha. `language` is the page's, `en` or `fa`, to reply in.
             -- `contact_kind` is empty when no way to reply was given. `seen`
             -- is when the admin marked it read, NULL while unread.
             CREATE TABLE IF NOT EXISTS message (
                 id INTEGER PRIMARY KEY,
                 at INTEGER NOT NULL,
                 ip TEXT NOT NULL,
                 device BLOB REFERENCES device (id),
                 language TEXT NOT NULL,
                 name TEXT NOT NULL,
                 text TEXT NOT NULL,
                 contact_kind TEXT NOT NULL,
                 contact TEXT NOT NULL,
                 seen INTEGER
             );

             -- Requests refused as robots, counted per UTC day and per kind:
             -- `page`, `visit`, `click`, `captcha`, `email` or `message`.
             -- Nothing else about them is kept.
             CREATE TABLE IF NOT EXISTS robot (
                 day TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 dropped INTEGER NOT NULL,
                 PRIMARY KEY (day, kind)
             ) WITHOUT ROWID;",
        )
        .with_context(|| format!("Could not set up the schema in {}", db.display()))?;

    let app = Arc::new(App {
        www: www.clone(),
        url,
        admin_hash,
        sessions: Mutex::new(Vec::new()),
        database: Mutex::new(connection),
        captchas: Mutex::new(VecDeque::new()),
        recent: Mutex::new(Vec::new()),
    });

    let router = Router::new()
        .route(
            "/",
            get(|State(app): State<Arc<App>>, headers: HeaderMap| {
                page(app, headers, "index.html", "/")
            }),
        )
        .route(
            "/index.html",
            get(|State(app): State<Arc<App>>, headers: HeaderMap| {
                page(app, headers, "index.html", "/")
            }),
        )
        .route(
            "/software",
            get(|State(app): State<Arc<App>>, headers: HeaderMap| {
                page(app, headers, "software.html", "/software")
            }),
        )
        .route(
            "/contact",
            get(|State(app): State<Arc<App>>, headers: HeaderMap| {
                page(app, headers, "contact.html", "/contact")
            }),
        )
        // Served filled in at their own paths only, never raw by the fallback.
        .route("/software.html", get(|| async { StatusCode::NOT_FOUND }))
        .route("/contact.html", get(|| async { StatusCode::NOT_FOUND }))
        .route("/admin.html", get(|| async { StatusCode::NOT_FOUND }))
        // The admin page, as-is: no fields to fill, no visit, no robot count.
        // Without a session it shows only the login, and everything it shows
        // after comes from /api/admin.
        .route(
            &admin_path,
            get(|State(app): State<Arc<App>>| async move {
                match tokio::fs::read_to_string(app.www.join("admin.html")).await {
                    Ok(html) => ([(header::CACHE_CONTROL, "no-store")], Html(html)).into_response(),
                    Err(error) => {
                        tracing::error!(msg = "Could not read page", file = "admin.html", www = ?app.www, error = %error);
                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    }
                }
            }),
        )
        .route("/go", get(go))
        .route("/api/visit", post(visit))
        .route("/api/captcha", get(captcha))
        .route("/api/email", post(email))
        .route("/api/message", post(message))
        .route("/api/click", post(click))
        .nest(
            "/api/admin",
            Router::new()
                .route("/login", post(login))
                .route("/logout", post(logout))
                .route("/overview", post(overview))
                .route(
                    "/visits",
                    post(|State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<SearchRequest>| {
                        search(app, headers, &VISITS, body)
                    }),
                )
                .route(
                    "/visits/delete",
                    post(|State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<DeleteRequest>| {
                        delete(app, headers, &VISITS, body)
                    }),
                )
                .route(
                    "/clicks",
                    post(|State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<SearchRequest>| {
                        search(app, headers, &CLICKS, body)
                    }),
                )
                .route(
                    "/clicks/delete",
                    post(|State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<DeleteRequest>| {
                        delete(app, headers, &CLICKS, body)
                    }),
                )
                .route(
                    "/messages",
                    post(|State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<SearchRequest>| {
                        search(app, headers, &MESSAGES, body)
                    }),
                )
                .route(
                    "/messages/delete",
                    post(|State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<DeleteRequest>| {
                        delete(app, headers, &MESSAGES, body)
                    }),
                )
                .route("/messages/seen", post(seen))
                // Nothing the admin sees is worth a copy in any cache.
                .layer(SetResponseHeaderLayer::overriding(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("no-store"),
                )),
        )
        .fallback_service(ServeDir::new(&www))
        // Revalidate everything: ServeDir answers with Last-Modified, so an
        // unchanged file costs a 304, and a changed one shows up on the next
        // load without any cache-busting in the front end.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(middleware::from_fn(access_log))
        .with_state(app);

    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("Could not listen on {address}"))?;
    tracing::info!(
        msg = "Listening",
        address = %address,
        www = ?www,
        db = ?db,
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(terminate) => terminate,
                Err(error) => {
                    tracing::error!(msg = "Could not listen for SIGTERM", error = %error);
                    std::future::pending::<()>().await;
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
        tracing::info!(msg = "Shutting down");
    })
    .await?;
    Ok(())
}

/// `GET /` and `/index.html` (`index.html`), `GET /software`
/// (`software.html`): the page's `file` with its `{{name}}` fields filled from
/// the settings at the top of this file; `{{url}}` is `--url` then `path`,
/// `{{home}}` is `--url` then `/` on every page.
/// Robots are counted and still served, so link previews work.
async fn page(
    app: Arc<App>,
    headers: HeaderMap,
    file: &'static str,
    path: &'static str,
) -> Response {
    robot(&app, &headers, "page").await;

    let template = match tokio::fs::read_to_string(app.www.join(file)).await {
        Ok(template) => template,
        Err(error) => {
            tracing::error!(msg = "Could not read page", file = file, www = ?app.www, error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let url = format!("{}{path}", app.url);
    let home = format!("{}/", app.url);
    let values = [
        ("title", SITE_TITLE),
        ("description", SITE_DESCRIPTION),
        ("url", url.as_str()),
        ("home", home.as_str()),
        ("github", LINK_GITHUB),
        ("telegram", LINK_TELEGRAM),
        ("blog", LINK_BLOG),
    ];
    // One pass over the template, so a `{{…}}` inside a value is text, never
    // a second substitution. Unknown names are left as written. Values are
    // escaped, safe inside HTML text and attributes.
    let mut html = String::with_capacity(template.len() + 1024);
    let mut rest = template.as_str();
    while let Some(start) = rest.find("{{") {
        html.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            html.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let name = &after[..end];
        let mut value = None;
        for (key, found) in values {
            if key == name {
                value = Some(found);
            }
        }
        match value {
            Some(value) => {
                for character in value.chars() {
                    match character {
                        '&' => html.push_str("&amp;"),
                        '<' => html.push_str("&lt;"),
                        '>' => html.push_str("&gt;"),
                        '"' => html.push_str("&quot;"),
                        '\'' => html.push_str("&#39;"),
                        other => html.push(other),
                    }
                }
            }
            None => html.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    html.push_str(rest);
    Html(html).into_response()
}

#[derive(Deserialize)]
struct VisitRequest {
    /// The ULID this browser was given before, if it kept one.
    device: Option<String>,
    /// `location.pathname` of the page being opened.
    path: String,
    /// `document.referrer`: where the visitor came from. The Referer header of
    /// this request would only name our own page.
    referrer: String,
}

#[derive(Serialize)]
struct VisitResponse {
    device: String,
}

/// `POST /api/visit`, once per page open: the handshake. An unknown or missing
/// device gets a new ULID; either way one `visit` row is written. Robots get a
/// `204` and no device.
async fn visit(
    State(app): State<Arc<App>>,
    ConnectInfo(socket): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<VisitRequest>,
) -> Response {
    if robot(&app, &headers, "visit").await {
        return StatusCode::NO_CONTENT.into_response();
    }

    let ip = client_ip(&headers, socket);

    let language = match header_text(&headers, "accept-language") {
        Some(language) => clip(&language),
        None => String::new(),
    };
    let referrer = clip(&body.referrer);
    let path = clip(&body.path);
    let claimed = match &body.device {
        Some(text) => Ulid::parse(text),
        None => None,
    };
    let fresh = match Ulid::new() {
        Ok(fresh) => fresh,
        Err(error) => {
            tracing::error!(msg = "Could not make a device id", error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let result = database(&app, move |connection| {
        let transaction = connection.unchecked_transaction()?;
        let mut device = None;
        if let Some(claimed) = claimed {
            let known: Option<i64> = transaction
                .query_row(
                    "SELECT 1 FROM device WHERE id = ?1",
                    [&claimed.0[..]],
                    |row| row.get(0),
                )
                .optional()?;
            if known.is_some() {
                device = Some(claimed);
            }
        }
        let device = match device {
            Some(device) => device,
            None => {
                transaction.execute(
                    "INSERT INTO device (id, created_at) VALUES (?1, ?2)",
                    params![&fresh.0[..], now()],
                )?;
                fresh
            }
        };
        transaction.execute(
            "INSERT INTO visit (device, at, ip, referrer, language, path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &device.0[..],
                now(),
                ip.to_string(),
                referrer,
                language,
                path
            ],
        )?;
        transaction.commit()?;
        Ok(device)
    })
    .await;

    match result {
        Ok(device) => Json(VisitResponse {
            device: device.to_string(),
        })
        .into_response(),
        Err(error) => {
            tracing::error!(msg = "Could not record visit", error = %format!("{error:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Deserialize)]
struct GoRequest {
    /// Where the link points.
    to: String,
    /// `location.pathname` of the page the link was on, added by its script.
    from: Option<String>,
    /// The browser's device id, added by the page's script once it has one.
    device: Option<String>,
}

/// `GET /go?to=URL`: every link to another site goes through here, so
/// following one is recorded even though the page it was on is gone. Links
/// to our own pages do not: the visit they lead to, with its referrer, is the
/// record. The browser is sent on with a `303`. Only a `to` that some `--www` page links
/// to as `href="/go?to=…"` is allowed, so this is no open redirect for anyone
/// else's links; any other is a `404`. Robots are sent on too, but counted
/// rather than recorded.
async fn go(
    State(app): State<Arc<App>>,
    ConnectInfo(socket): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(request): Query<GoRequest>,
) -> Response {
    let allowed = match linked(&app, &request.to).await {
        Ok(allowed) => allowed,
        Err(error) => {
            tracing::error!(msg = "Could not read the front end", www = ?app.www, error = %format!("{error:#}"));
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !allowed {
        tracing::warn!(msg = "Refused a link no page has", to = %request.to);
        return StatusCode::NOT_FOUND.into_response();
    }
    let location = match HeaderValue::from_str(&request.to) {
        Ok(location) => location,
        Err(error) => {
            tracing::error!(msg = "A page links to an unusable URL", to = %request.to, error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let onward = (StatusCode::SEE_OTHER, [(header::LOCATION, location)]);

    if robot(&app, &headers, "click").await {
        return onward.into_response();
    }
    let ip = client_ip(&headers, socket);
    let page = match &request.from {
        Some(from) => clip(from),
        None => String::new(),
    };
    let claimed = match &request.device {
        Some(text) => Ulid::parse(text),
        None => None,
    };
    let link = request.to;
    let recorded = database(&app, move |connection| {
        let device = known_device(connection, claimed)?;
        connection.execute(
            "INSERT INTO click (at, ip, link, page, device, action) VALUES (?1, ?2, ?3, ?4, ?5, 'open')",
            params![now(), ip.to_string(), link, page, device],
        )
    })
    .await;
    // The visitor is sent on either way: a lost row is ours to worry about.
    if let Err(error) = recorded {
        tracing::error!(msg = "Could not record click", error = %format!("{error:#}"));
    }
    onward.into_response()
}

#[derive(Deserialize)]
struct ClickRequest {
    /// What was opened or copied: a `/go` target some page has, or
    /// `mailto:` and `EMAIL`.
    link: String,
    /// `open` or `copy`.
    action: String,
    /// `location.pathname` of the page it happened on.
    page: String,
    /// The browser's device id, when the page has one.
    device: Option<String>,
}

/// `POST /api/click`, sent by beacon: what a followed `/go` link cannot
/// record. A handle or the email copied from a page (`copy`), and the email's
/// `mailto:` link opened, which never reaches this server (`open`). The link
/// must be one `/go` would take, or the email; anything else is a `404`, so
/// nobody can write junk. `204` either way for robots, who are only counted.
async fn click(
    State(app): State<Arc<App>>,
    ConnectInfo(socket): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<ClickRequest>,
) -> Response {
    let action = match body.action.as_str() {
        "open" => "open",
        "copy" => "copy",
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let allowed = if body.link == format!("mailto:{EMAIL}") {
        true
    } else {
        match linked(&app, &body.link).await {
            Ok(allowed) => allowed,
            Err(error) => {
                tracing::error!(msg = "Could not read the front end", www = ?app.www, error = %format!("{error:#}"));
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    };
    if !allowed {
        tracing::warn!(msg = "Refused a link no page has", to = %body.link);
        return StatusCode::NOT_FOUND.into_response();
    }
    if robot(&app, &headers, "click").await {
        return StatusCode::NO_CONTENT.into_response();
    }
    let ip = client_ip(&headers, socket);
    let page = clip(&body.page);
    let claimed = match &body.device {
        Some(text) => Ulid::parse(text),
        None => None,
    };
    let link = body.link;
    let recorded = database(&app, move |connection| {
        let device = known_device(connection, claimed)?;
        connection.execute(
            "INSERT INTO click (at, ip, link, page, device, action) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now(), ip.to_string(), link, page, device, action],
        )
    })
    .await;
    match recorded {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(msg = "Could not record click", error = %format!("{error:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Whether some `--www` page links to `target` as `href="/go?to=…"`. The pages
/// are the only list of allowed targets: every one is read, as a page load
/// does, so a link added to the HTML works on the next click.
async fn linked(app: &App, target: &str) -> anyhow::Result<bool> {
    let mut entries = tokio::fs::read_dir(&app.www)
        .await
        .context("Could not list the front end")?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .context("Could not list the front end")?
    {
        let file = entry.path();
        match file.extension() {
            Some(extension) if extension == "html" => {}
            _ => continue,
        }
        let html = tokio::fs::read_to_string(&file)
            .await
            .with_context(|| format!("Could not read {}", file.display()))?;
        let mut rest = html.as_str();
        while let Some(start) = rest.find("href=\"/go?") {
            let after = &rest[start + "href=\"".len()..];
            let Some(end) = after.find('"') else {
                break;
            };
            // Parsed the way a request to /go is, so an encoded and a plain
            // spelling of one URL match.
            if let Ok(uri) = after[..end].replace("&amp;", "&").parse::<Uri>()
                && let Ok(Query(link)) = Query::<GoRequest>::try_from_uri(&uri)
                && link.to == target
            {
                return Ok(true);
            }
            rest = &after[end..];
        }
    }
    Ok(false)
}

#[derive(Serialize)]
struct CaptchaResponse {
    /// What `POST /api/email` names this captcha by.
    id: String,
    /// A `data:image/png;base64,…` URL of the picture.
    image: String,
}

/// `GET /api/captcha`: a new picture of characters guarding the email. One
/// per client address per second, for at most `CAPTCHA_ADDRESSES_PER_SECOND`
/// addresses in that second; the rest get a `429` and come back a second
/// later.
async fn captcha(
    State(app): State<Arc<App>>,
    ConnectInfo(socket): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if robot(&app, &headers, "captcha").await {
        return StatusCode::NO_CONTENT.into_response();
    }
    let ip = client_ip(&headers, socket);
    let mut allowed = true;
    {
        let mut recent = match app.recent.lock() {
            Ok(recent) => recent,
            Err(poisoned) => poisoned.into_inner(),
        };
        let started = Instant::now();
        recent.retain(|(_, at)| started.duration_since(*at) < Duration::from_secs(1));
        for (seen, _) in recent.iter() {
            if *seen == ip {
                allowed = false;
            }
        }
        if recent.len() >= CAPTCHA_ADDRESSES_PER_SECOND {
            allowed = false;
        }
        if allowed {
            recent.push((ip, started));
        }
    }
    if !allowed {
        return (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "1")]).into_response();
    }

    let made = tokio::task::spawn_blocking(|| {
        let mut made = captcha::Captcha::new();
        made.add_chars(CAPTCHA_LENGTH)
            .apply_filter(captcha::filters::Noise::new(0.4))
            .apply_filter(captcha::filters::Grid::new(6, 6))
            .view(220, 120);
        (made.chars_as_string(), made.as_base64())
    })
    .await;
    let (answer, image) = match made {
        Ok((answer, Some(image))) => (answer, image),
        Ok((_, None)) => {
            tracing::error!(msg = "Could not draw a captcha");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(error) => {
            tracing::error!(msg = "Captcha maker panicked", error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let id = match Ulid::new() {
        Ok(id) => id.to_string(),
        Err(error) => {
            tracing::error!(msg = "Could not make a captcha id", error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    tracing::trace!(msg = "Made captcha", id = %id, ip = %ip);
    {
        let mut captchas = match app.captchas.lock() {
            Ok(captchas) => captchas,
            Err(poisoned) => poisoned.into_inner(),
        };
        let made = Instant::now();
        captchas.retain(|(_, _, at)| made.duration_since(*at) < CAPTCHA_LIFETIME);
        if captchas.len() >= CAPTCHA_LIMIT {
            captchas.pop_front();
        }
        captchas.push_back((id.clone(), answer, made));
    }
    Json(CaptchaResponse {
        id,
        image: format!("data:image/png;base64,{image}"),
    })
    .into_response()
}

#[derive(Deserialize)]
struct EmailRequest {
    /// The id from `GET /api/captcha`.
    captcha: String,
    /// What the visitor read in its picture.
    answer: String,
}

#[derive(Serialize)]
struct EmailResponse {
    email: &'static str,
}

#[derive(Serialize)]
struct FieldError {
    /// Which field to fix: `captcha` (wrong, expired or already tried), for a
    /// message `name`, `text` or `contact`, for the admin login `password`,
    /// and for an admin search `device`.
    error: &'static str,
}

/// `POST /api/email`: a solved captcha for `EMAIL`. A wrong answer is a `400`
/// and costs the captcha: the front end asks for a new one.
async fn email(
    State(app): State<Arc<App>>,
    ConnectInfo(socket): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<EmailRequest>,
) -> Response {
    if robot(&app, &headers, "email").await {
        return StatusCode::NO_CONTENT.into_response();
    }
    if !solved(&app, &body.captcha, &body.answer) {
        return (
            StatusCode::BAD_REQUEST,
            Json(FieldError { error: "captcha" }),
        )
            .into_response();
    }
    tracing::info!(msg = "Gave out the email", ip = %client_ip(&headers, socket));
    Json(EmailResponse { email: EMAIL }).into_response()
}

#[derive(Deserialize)]
struct MessageRequest {
    /// The sender's name.
    name: String,
    /// What they wrote.
    text: String,
    /// One of `MESSAGE_CONTACT_KINDS`, naming what `contact` is.
    contact_kind: String,
    /// How to reply to them; empty when they gave nothing.
    contact: String,
    /// `en` or `fa`: the language the page was in.
    language: String,
    /// The browser's device id, when the page has one.
    device: Option<String>,
    /// The id from `GET /api/captcha`.
    captcha: String,
    /// What the visitor read in its picture.
    answer: String,
}

/// `POST /api/message`: a message from the contact page, kept for the author
/// to read. `204` when stored; `400 {error}` naming the field to fix. The
/// fields are checked before the captcha, which a typo should not cost; a
/// wrong answer costs the captcha, and the front end asks for a new one.
async fn message(
    State(app): State<Arc<App>>,
    ConnectInfo(socket): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<MessageRequest>,
) -> Response {
    if robot(&app, &headers, "message").await {
        return StatusCode::NO_CONTENT.into_response();
    }
    let refuse = |field: &'static str| {
        (StatusCode::BAD_REQUEST, Json(FieldError { error: field })).into_response()
    };

    let name = body.name.trim().to_string();
    if name.is_empty() || name.chars().count() > MESSAGE_NAME_LIMIT {
        return refuse("name");
    }
    let text = body.text.trim().to_string();
    if text.is_empty()
        || text.len() > MESSAGE_BYTE_LIMIT
        || text.chars().count() > MESSAGE_CHARACTER_LIMIT
        || text.split_whitespace().count() > MESSAGE_WORD_LIMIT
    {
        return refuse("text");
    }
    let contact = body.contact.trim().to_string();
    let mut contact_kind = String::new();
    if !contact.is_empty() {
        for kind in MESSAGE_CONTACT_KINDS {
            if kind == body.contact_kind {
                contact_kind = kind.to_string();
            }
        }
        if contact_kind.is_empty() || contact.chars().count() > MESSAGE_CONTACT_LIMIT {
            return refuse("contact");
        }
    }
    let language = match body.language.as_str() {
        "fa" => "fa",
        _ => "en",
    };
    if !solved(&app, &body.captcha, &body.answer) {
        return refuse("captcha");
    }

    let ip = client_ip(&headers, socket);
    let claimed = match &body.device {
        Some(text) => Ulid::parse(text),
        None => None,
    };
    let stored = database(&app, move |connection| {
        let device = known_device(connection, claimed)?;
        connection.execute(
            "INSERT INTO message (at, ip, device, language, name, text, contact_kind, contact)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                now(),
                ip.to_string(),
                device,
                language,
                name,
                text,
                contact_kind,
                contact
            ],
        )
    })
    .await;
    match stored {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(msg = "Could not store message", error = %format!("{error:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Deserialize)]
struct LoginRequest {
    /// Checked against `--admin-hash-hex`.
    password: String,
    /// The id from `GET /api/captcha`.
    captcha: String,
    /// What the admin read in its picture.
    answer: String,
}

/// `POST /api/admin/login`: the password and a solved captcha for a session
/// cookie, `204`. The captcha is checked first, so every password tried costs
/// one; `400 {error}` names `captcha` or `password`.
async fn login(
    State(app): State<Arc<App>>,
    ConnectInfo(socket): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Response {
    let refuse = |field: &'static str| {
        (StatusCode::BAD_REQUEST, Json(FieldError { error: field })).into_response()
    };
    if !solved(&app, &body.captcha, &body.answer) {
        return refuse("captcha");
    }
    let ip = client_ip(&headers, socket);
    let hash = app.admin_hash.clone();
    // Argon2 is slow on purpose: kept off the async threads.
    let checked = tokio::task::spawn_blocking(move || {
        Argon2::default().verify_password(body.password.as_bytes(), hash.as_str())
    })
    .await;
    match checked {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            tracing::warn!(msg = "Refused admin login", ip = %ip);
            return refuse("password");
        }
        Err(error) => {
            tracing::error!(msg = "Password check panicked", error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    let mut bytes = [0u8; 32];
    if let Err(error) = getrandom::fill(&mut bytes) {
        tracing::error!(msg = "Could not make a session token", error = %error);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let token = hex(&bytes);
    {
        let mut sessions = match app.sessions.lock() {
            Ok(sessions) => sessions,
            Err(poisoned) => poisoned.into_inner(),
        };
        let started = Instant::now();
        sessions.retain(|(_, at)| started.duration_since(*at) < ADMIN_SESSION_IDLE);
        sessions.push((token.clone(), started));
    }
    tracing::info!(msg = "Admin logged in", ip = %ip);
    let cookie =
        format!("{ADMIN_COOKIE}={token}; Path=/api/admin; HttpOnly; Secure; SameSite=Strict");
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response()
}

/// `POST /api/admin/logout`: ends the session, if any, and clears its cookie.
async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if let Some(token) = signed_in(&app, &headers) {
        let mut sessions = match app.sessions.lock() {
            Ok(sessions) => sessions,
            Err(poisoned) => poisoned.into_inner(),
        };
        sessions.retain(|(open, _)| *open != token);
    }
    let cookie =
        format!("{ADMIN_COOKIE}=; Path=/api/admin; HttpOnly; Secure; SameSite=Strict; Max-Age=0");
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response()
}

/// The token of the open admin session this request's cookie names, whose
/// idle time starts over; `None` without one.
fn signed_in(app: &App, headers: &HeaderMap) -> Option<String> {
    let mut token = None;
    for value in headers.get_all(header::COOKIE) {
        let Ok(text) = value.to_str() else {
            continue;
        };
        for pair in text.split(';') {
            if let Some((name, value)) = pair.trim().split_once('=')
                && name == ADMIN_COOKIE
            {
                token = Some(value.to_string());
            }
        }
    }
    let token = token?;
    let mut sessions = match app.sessions.lock() {
        Ok(sessions) => sessions,
        Err(poisoned) => poisoned.into_inner(),
    };
    let checked = Instant::now();
    sessions.retain(|(_, at)| checked.duration_since(*at) < ADMIN_SESSION_IDLE);
    let mut found = false;
    for (open, at) in sessions.iter_mut() {
        // Every byte compared, so the time taken says nothing about how many
        // matched.
        let mut difference = u8::from(open.len() != token.len());
        for (left, right) in open.bytes().zip(token.bytes()) {
            difference |= left ^ right;
        }
        if difference == 0 {
            *at = checked;
            found = true;
        }
    }
    if found { Some(token) } else { None }
}

#[derive(Deserialize)]
struct OverviewRequest {
    /// Unix milliseconds the range starts at, the browser's midnight; none
    /// for everything.
    from: Option<i64>,
    /// The browser's offset from UTC in minutes, east positive: the days
    /// below are its days.
    offset: i64,
}

#[derive(Serialize)]
struct OverviewResponse {
    visits: i64,
    devices: i64,
    new_devices: i64,
    clicks: i64,
    messages: i64,
    /// Unread among `messages`.
    unread: i64,
    /// Unread messages of all time.
    unread_all: i64,
    /// `[kind, dropped]`, over UTC days from the one `from` falls in.
    robots: Vec<(String, i64)>,
    /// Newest first.
    days: Vec<Day>,
    /// The ten most opened, `[path, visits]`.
    paths: Vec<(String, i64)>,
    /// The ten most common, `[referrer, visits]`.
    referrers: Vec<(String, i64)>,
    /// The ten most clicked, `[link, opened, copied]`.
    links: Vec<(String, i64, i64)>,
}

#[derive(Serialize)]
struct Day {
    /// `YYYY-MM-DD` in the browser's zone.
    day: String,
    visits: i64,
    devices: i64,
    new_devices: i64,
    clicks: i64,
    messages: i64,
    /// By UTC day, as the `robot` table keeps them.
    robots: i64,
}

/// `POST /api/admin/overview`: totals, per-day counts and the most common
/// paths, referrers and links since `from`.
async fn overview(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<OverviewRequest>,
) -> Response {
    if signed_in(&app, &headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let from = body.from.unwrap_or_default();
    let shift = body.offset.clamp(-14 * 60, 14 * 60) * 60_000;
    let result = database(&app, move |connection| {
        let pairs = |sql: &str| -> rusqlite::Result<Vec<(String, i64)>> {
            let mut statement = connection.prepare(sql)?;
            let mut rows = statement.query([from])?;
            let mut pairs = Vec::new();
            while let Some(row) = rows.next()? {
                pairs.push((row.get(0)?, row.get(1)?));
            }
            Ok(pairs)
        };
        let (visits, devices) = connection.query_row(
            "SELECT count(*), count(DISTINCT device) FROM visit WHERE at >= ?1",
            [from],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let new_devices = connection.query_row(
            "SELECT count(*) FROM device WHERE created_at >= ?1",
            [from],
            |row| row.get(0),
        )?;
        let clicks =
            connection.query_row("SELECT count(*) FROM click WHERE at >= ?1", [from], |row| {
                row.get(0)
            })?;
        let (messages, unread) = connection.query_row(
            "SELECT count(*), count(*) - count(seen) FROM message WHERE at >= ?1",
            [from],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let unread_all = connection.query_row(
            "SELECT count(*) FROM message WHERE seen IS NULL",
            [],
            |row| row.get(0),
        )?;
        let robots = pairs(
            "SELECT kind, sum(dropped) FROM robot WHERE day >= date(?1 / 1000, 'unixepoch')
             GROUP BY kind ORDER BY kind",
        )?;

        // Per day, `[visits, devices, new devices, clicks, messages, robots]`;
        // each query fills its own columns, from `first` on.
        let mut days: BTreeMap<String, [i64; 6]> = BTreeMap::new();
        for (sql, first) in [
            (
                "SELECT date((at + ?2) / 1000, 'unixepoch') AS day, count(*), count(DISTINCT device)
                 FROM visit WHERE at >= ?1 GROUP BY day",
                0,
            ),
            (
                "SELECT date((created_at + ?2) / 1000, 'unixepoch') AS day, count(*)
                 FROM device WHERE created_at >= ?1 GROUP BY day",
                2,
            ),
            (
                "SELECT date((at + ?2) / 1000, 'unixepoch') AS day, count(*)
                 FROM click WHERE at >= ?1 GROUP BY day",
                3,
            ),
            (
                "SELECT date((at + ?2) / 1000, 'unixepoch') AS day, count(*)
                 FROM message WHERE at >= ?1 GROUP BY day",
                4,
            ),
        ] {
            let mut statement = connection.prepare(sql)?;
            let counts = statement.column_count() - 1;
            let mut rows = statement.query([from, shift])?;
            while let Some(row) = rows.next()? {
                let day: String = row.get(0)?;
                let entry = days.entry(day).or_insert([0; 6]);
                for index in 0..counts {
                    entry[first + index] = row.get(index + 1)?;
                }
            }
        }
        for (day, dropped) in pairs(
            "SELECT day, sum(dropped) FROM robot WHERE day >= date(?1 / 1000, 'unixepoch')
             GROUP BY day",
        )? {
            days.entry(day).or_insert([0; 6])[5] = dropped;
        }
        let mut newest_first = Vec::new();
        for (day, counts) in days.into_iter().rev() {
            newest_first.push(Day {
                day,
                visits: counts[0],
                devices: counts[1],
                new_devices: counts[2],
                clicks: counts[3],
                messages: counts[4],
                robots: counts[5],
            });
        }

        let paths = pairs(
            "SELECT path, count(*) AS n FROM visit WHERE at >= ?1
             GROUP BY path ORDER BY n DESC, path LIMIT 10",
        )?;
        let referrers = pairs(
            "SELECT referrer, count(*) AS n FROM visit WHERE at >= ?1 AND referrer != ''
             GROUP BY referrer ORDER BY n DESC, referrer LIMIT 10",
        )?;
        let mut links = Vec::new();
        let mut statement = connection.prepare(
            "SELECT link, sum(action = 'open'), sum(action = 'copy') FROM click WHERE at >= ?1
             GROUP BY link ORDER BY count(*) DESC, link LIMIT 10",
        )?;
        let mut rows = statement.query([from])?;
        while let Some(row) = rows.next()? {
            links.push((row.get(0)?, row.get(1)?, row.get(2)?));
        }

        Ok(OverviewResponse {
            visits,
            devices,
            new_devices,
            clicks,
            messages,
            unread,
            unread_all,
            robots,
            days: newest_first,
            paths,
            referrers,
            links,
        })
    })
    .await;
    match result {
        Ok(overview) => Json(overview).into_response(),
        Err(error) => {
            tracing::error!(msg = "Could not count for the overview", error = %format!("{error:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// A table the admin page lists, searches and deletes from.
struct Records {
    table: &'static str,
    /// Sent in this order, `device` as its ULID.
    columns: &'static [&'static str],
    /// What a search's `text` looks inside.
    text: &'static [&'static str],
}

static VISITS: Records = Records {
    table: "visit",
    columns: &["id", "at", "ip", "device", "path", "referrer", "language"],
    text: &["path", "referrer", "language"],
};

static CLICKS: Records = Records {
    table: "click",
    columns: &["id", "at", "ip", "action", "link", "page", "device"],
    text: &["link", "page"],
};

static MESSAGES: Records = Records {
    table: "message",
    columns: &[
        "id",
        "at",
        "ip",
        "name",
        "text",
        "contact_kind",
        "contact",
        "language",
        "device",
        "seen",
    ],
    text: &["name", "text", "contact"],
};

/// What an admin search or delete matches; every field given must hold.
#[derive(Deserialize)]
struct Filter {
    /// Unix milliseconds, inclusive.
    from: Option<i64>,
    /// Unix milliseconds, exclusive.
    to: Option<i64>,
    /// Exactly this client address.
    ip: Option<String>,
    /// Exactly this device ULID.
    device: Option<String>,
    /// Inside any of the table's text columns, ignoring ASCII case.
    text: Option<String>,
    /// Messages only: read (`true`) or unread (`false`).
    seen: Option<bool>,
}

/// `filter` as a `WHERE` clause over `records` and its values in order, or
/// the field it cannot read.
fn conditions(records: &Records, filter: &Filter) -> Result<(String, Vec<Value>), &'static str> {
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    if let Some(from) = filter.from {
        clauses.push("at >= ?".to_string());
        values.push(Value::Integer(from));
    }
    if let Some(to) = filter.to {
        clauses.push("at < ?".to_string());
        values.push(Value::Integer(to));
    }
    if let Some(ip) = &filter.ip {
        clauses.push("ip = ?".to_string());
        values.push(Value::Text(ip.trim().to_string()));
    }
    if let Some(device) = &filter.device {
        match Ulid::parse(device.trim()) {
            Some(device) => {
                clauses.push("device = ?".to_string());
                values.push(Value::Blob(device.0.to_vec()));
            }
            None => return Err("device"),
        }
    }
    if let Some(text) = &filter.text
        && !text.is_empty()
    {
        let mut any = Vec::new();
        for column in records.text {
            any.push(format!("instr(lower({column}), lower(?)) > 0"));
            values.push(Value::Text(text.clone()));
        }
        clauses.push(format!("({})", any.join(" OR ")));
    }
    if let Some(seen) = filter.seen
        && records.table == "message"
    {
        clauses.push(
            if seen {
                "seen IS NOT NULL"
            } else {
                "seen IS NULL"
            }
            .to_string(),
        );
    }
    if clauses.is_empty() {
        return Ok(("1".to_string(), values));
    }
    Ok((clauses.join(" AND "), values))
}

#[derive(Deserialize)]
struct SearchRequest {
    filter: Filter,
    /// From 1.
    page: usize,
    /// One of `ADMIN_PAGE_SIZES`.
    size: usize,
}

#[derive(Serialize)]
struct SearchResponse {
    columns: &'static [&'static str],
    /// Rows the filter matches, all pages.
    total: i64,
    /// This page of them, newest first, each in `columns` order.
    rows: Vec<Vec<Cell>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Cell {
    Null,
    Integer(i64),
    Text(String),
}

/// `POST /api/admin/{visits,clicks,messages}`: one page of what `filter`
/// matches, newest first, and how many it matches in all.
async fn search(
    app: Arc<App>,
    headers: HeaderMap,
    records: &'static Records,
    body: SearchRequest,
) -> Response {
    if signed_in(&app, &headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if body.page == 0 || !ADMIN_PAGE_SIZES.contains(&body.size) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let (clause, values) = match conditions(records, &body.filter) {
        Ok(found) => found,
        Err(field) => {
            return (StatusCode::BAD_REQUEST, Json(FieldError { error: field })).into_response();
        }
    };
    let size = body.size;
    let offset = (body.page - 1).saturating_mul(size);
    let result = database(&app, move |connection| {
        let total = connection.query_row(
            &format!("SELECT count(*) FROM {} WHERE {clause}", records.table),
            params_from_iter(values.iter()),
            |row| row.get(0),
        )?;
        let mut statement = connection.prepare(&format!(
            "SELECT {} FROM {} WHERE {clause} ORDER BY at DESC, id DESC LIMIT {size} OFFSET {offset}",
            records.columns.join(", "),
            records.table
        ))?;
        let mut found = statement.query(params_from_iter(values.iter()))?;
        let mut rows = Vec::new();
        while let Some(row) = found.next()? {
            let mut cells = Vec::new();
            for index in 0..records.columns.len() {
                cells.push(match row.get_ref(index)? {
                    ValueRef::Null => Cell::Null,
                    ValueRef::Integer(number) => Cell::Integer(number),
                    ValueRef::Real(number) => Cell::Integer(number as i64),
                    ValueRef::Text(text) => Cell::Text(String::from_utf8_lossy(text).into_owned()),
                    // The only blobs are device ids.
                    ValueRef::Blob(blob) => match <[u8; 16]>::try_from(blob) {
                        Ok(bytes) => Cell::Text(Ulid(bytes).to_string()),
                        Err(_) => Cell::Null,
                    },
                });
            }
            rows.push(cells);
        }
        Ok((total, rows))
    })
    .await;
    match result {
        Ok((total, rows)) => Json(SearchResponse {
            columns: records.columns,
            total,
            rows,
        })
        .into_response(),
        Err(error) => {
            tracing::error!(msg = "Could not search", table = records.table, error = %format!("{error:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Deserialize)]
struct DeleteRequest {
    /// These rows, at most the largest of `ADMIN_PAGE_SIZES`...
    ids: Option<Vec<i64>>,
    /// ...or every row this matches, however many. Exactly one of the two.
    filter: Option<Filter>,
}

#[derive(Serialize)]
struct DeleteResponse {
    deleted: usize,
}

/// `POST /api/admin/{visits,clicks,messages}/delete`: the rows named, or all
/// a filter matches, in one transaction. Deleting visits also deletes the
/// devices left with none, which no longer count as visitors, and forgets
/// them on clicks and messages.
async fn delete(
    app: Arc<App>,
    headers: HeaderMap,
    records: &'static Records,
    body: DeleteRequest,
) -> Response {
    if signed_in(&app, &headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (clause, values) = match (body.ids, body.filter) {
        (Some(ids), None)
            if !ids.is_empty() && ids.len() <= ADMIN_PAGE_SIZES[ADMIN_PAGE_SIZES.len() - 1] =>
        {
            let mut marks = Vec::new();
            let mut values = Vec::new();
            for id in ids {
                marks.push("?");
                values.push(Value::Integer(id));
            }
            (format!("id IN ({})", marks.join(", ")), values)
        }
        (None, Some(filter)) => match conditions(records, &filter) {
            Ok(found) => found,
            Err(field) => {
                return (StatusCode::BAD_REQUEST, Json(FieldError { error: field }))
                    .into_response();
            }
        },
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    tracing::debug!(msg = "Deleting", table = records.table, clause = %clause);
    let result = database(&app, move |connection| {
        let transaction = connection.unchecked_transaction()?;
        let deleted = transaction.execute(
            &format!("DELETE FROM {} WHERE {clause}", records.table),
            params_from_iter(values.iter()),
        )?;
        if records.table == "visit" {
            transaction.execute_batch(
                "UPDATE click SET device = NULL
                 WHERE device IS NOT NULL AND device NOT IN (SELECT device FROM visit);
                 UPDATE message SET device = NULL
                 WHERE device IS NOT NULL AND device NOT IN (SELECT device FROM visit);
                 DELETE FROM device WHERE id NOT IN (SELECT device FROM visit);",
            )?;
        }
        transaction.commit()?;
        Ok(deleted)
    })
    .await;
    match result {
        Ok(deleted) => {
            tracing::info!(msg = "Deleted", table = records.table, deleted = deleted);
            Json(DeleteResponse { deleted }).into_response()
        }
        Err(error) => {
            tracing::error!(msg = "Could not delete", table = records.table, error = %format!("{error:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Deserialize)]
struct SeenRequest {
    /// At most the largest of `ADMIN_PAGE_SIZES`.
    ids: Vec<i64>,
    /// Read (`true`: now, unless already read) or unread (`false`).
    seen: bool,
}

/// `POST /api/admin/messages/seen`: marks messages read or unread, `204`.
async fn seen(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<SeenRequest>,
) -> Response {
    if signed_in(&app, &headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if body.ids.is_empty() || body.ids.len() > ADMIN_PAGE_SIZES[ADMIN_PAGE_SIZES.len() - 1] {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let mut marks = Vec::new();
    let mut values = Vec::new();
    if body.seen {
        values.push(Value::Integer(now()));
    }
    for id in body.ids {
        marks.push("?");
        values.push(Value::Integer(id));
    }
    let sql = if body.seen {
        format!(
            "UPDATE message SET seen = ? WHERE seen IS NULL AND id IN ({})",
            marks.join(", ")
        )
    } else {
        format!(
            "UPDATE message SET seen = NULL WHERE id IN ({})",
            marks.join(", ")
        )
    };
    let result = database(&app, move |connection| {
        connection.execute(&sql, params_from_iter(values.iter()))
    })
    .await;
    match result {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(msg = "Could not mark messages", error = %format!("{error:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The bytes of `claimed` when the database knows that device, for a row that
/// names it. A forged or forgotten id gives `None` rather than an error: the
/// click or message is no less worth keeping.
fn known_device(
    connection: &Connection,
    claimed: Option<Ulid>,
) -> rusqlite::Result<Option<Vec<u8>>> {
    let Some(claimed) = claimed else {
        return Ok(None);
    };
    let known: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM device WHERE id = ?1",
            [&claimed.0[..]],
            |row| row.get(0),
        )
        .optional()?;
    match known {
        Some(_) => Ok(Some(claimed.0.to_vec())),
        None => Ok(None),
    }
}

/// Whether `answer` is what captcha `id` showed. One try: right or wrong, the
/// captcha is gone afterwards. Unknown and expired ids are never solved.
fn solved(app: &App, id: &str, answer: &str) -> bool {
    let mut captchas = match app.captchas.lock() {
        Ok(captchas) => captchas,
        Err(poisoned) => poisoned.into_inner(),
    };
    let checked = Instant::now();
    captchas.retain(|(_, _, at)| checked.duration_since(*at) < CAPTCHA_LIFETIME);
    let mut found = None;
    for (index, (shown, _, _)) in captchas.iter().enumerate() {
        if shown == id {
            found = Some(index);
        }
    }
    match found {
        Some(index) => match captchas.remove(index) {
            Some((_, expected, _)) => expected.eq_ignore_ascii_case(answer.trim()),
            None => false,
        },
        None => false,
    }
}

/// One line per request, at a level that follows the status: 2xx and 3xx
/// `info`, 4xx `warn`, 5xx `error`.
async fn access_log(request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let query = match request.uri().query() {
        Some(query) => query.to_string(),
        None => String::new(),
    };
    let route = match request.extensions().get::<MatchedPath>() {
        Some(matched) => matched.as_str().to_string(),
        None => path.clone(),
    };
    let started = Instant::now();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    let total_ms = started.elapsed().as_secs_f64() * 1000.0;
    macro_rules! report {
        ($level:ident, $said:literal) => {
            tracing::$level!(
                msg = $said,
                http.request.method = method.as_str(),
                http.route = route.as_str(),
                url.path = path.as_str(),
                url.query = query.as_str(),
                http.response.status_code = status,
                total_ms = total_ms,
            )
        };
    }
    match status {
        500.. => report!(error, "Failed request"),
        400..500 => report!(warn, "Rejected request"),
        _ => report!(info, "Handled request"),
    }
    response
}

/// Whether this request looks like a robot, and if so, count it under `kind`
/// for today. Browsers all send a User-Agent starting `Mozilla/`; robots
/// mostly send none, their own name, or a library's.
async fn robot(app: &Arc<App>, headers: &HeaderMap, kind: &'static str) -> bool {
    let agent = match header_text(headers, "user-agent") {
        Some(agent) => agent.to_ascii_lowercase(),
        None => String::new(),
    };
    let mut robot = !agent.starts_with("mozilla/");
    for marker in ROBOT_MARKERS {
        if agent.contains(marker) {
            robot = true;
        }
    }
    if !robot {
        return false;
    }
    tracing::debug!(msg = "Dropped robot", kind = kind, user_agent = %agent);
    let counted = database(app, move |connection| {
        connection.execute(
            "INSERT INTO robot (day, kind, dropped) VALUES (date('now'), ?1, 1)
             ON CONFLICT (day, kind) DO UPDATE SET dropped = dropped + 1",
            [kind],
        )
    })
    .await;
    if let Err(error) = counted {
        tracing::error!(msg = "Could not count robot", kind = kind, error = %format!("{error:#}"));
    }
    true
}

/// Run `work` against the database on a blocking thread.
async fn database<T, F>(app: &Arc<App>, work: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
{
    let app = app.clone();
    let joined = tokio::task::spawn_blocking(move || {
        // A panic while holding the lock leaves SQLite itself consistent: any
        // open transaction is rolled back when it is dropped.
        let connection = match app.database.lock() {
            Ok(connection) => connection,
            Err(poisoned) => poisoned.into_inner(),
        };
        work(&connection)
    })
    .await;
    match joined {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error.into()),
        Err(error) => Err(anyhow::anyhow!("Database task panicked: {error}")),
    }
}

/// The visitor's address behind our proxy, else the socket's.
fn client_ip(headers: &HeaderMap, socket: SocketAddr) -> IpAddr {
    // The right-most entry of each header is the one our own proxy appended,
    // which the client cannot forge; anything to its left can be.
    let mut ip = None;
    if let Some(forwarded) = header_text(headers, "forwarded")
        && let Some(last) = forwarded.rsplit(',').next()
    {
        for pair in last.split(';') {
            if let Some((key, value)) = pair.trim().split_once('=')
                && key.eq_ignore_ascii_case("for")
            {
                // `for="[2001:db8::1]:4711"`, `for=192.0.2.1:80` or bare.
                let value = value.trim_matches('"');
                let value = match value.strip_prefix('[') {
                    Some(bracketed) => match bracketed.split_once(']') {
                        Some((inside, _)) => inside,
                        None => bracketed,
                    },
                    None => match value.split_once(':') {
                        Some((address, _)) if !value[address.len() + 1..].contains(':') => address,
                        _ => value,
                    },
                };
                if let Ok(parsed) = value.parse::<IpAddr>() {
                    ip = Some(parsed);
                }
            }
        }
    }
    if ip.is_none()
        && let Some(value) = header_text(headers, "x-forwarded-for")
        && let Some(last) = value.rsplit(',').next()
        && let Ok(parsed) = last.trim().parse::<IpAddr>()
    {
        ip = Some(parsed);
    }
    if ip.is_none()
        && let Some(value) = header_text(headers, "x-real-ip")
        && let Ok(parsed) = value.trim().parse::<IpAddr>()
    {
        ip = Some(parsed);
    }
    match ip {
        Some(ip) => ip,
        None => socket.ip(),
    }
}

/// `bytes` as lowercase hex, two digits each.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// A header's value as text, or `None` when absent or not visible ASCII.
fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    match headers.get(name) {
        Some(value) => match value.to_str() {
            Ok(text) => Some(text.to_string()),
            Err(_) => None,
        },
        None => None,
    }
}

/// At most [`TEXT_LIMIT`] characters of `text`.
fn clip(text: &str) -> String {
    match text.char_indices().nth(TEXT_LIMIT) {
        Some((cut, _)) => text[..cut].to_string(),
        None => text.to_string(),
    }
}

/// Unix milliseconds. Saturates to 0 on a clock before 1970: a timestamp is
/// not worth refusing a write over.
fn now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis() as i64,
        Err(_) => 0,
    }
}
