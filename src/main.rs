//! pouriya-www: the backend of pouriya.net.
//!
//! Serves the front end from `--www`, with `index.html` filled in from the
//! settings below, and keeps who came in SQLite (`--db`): one row per device,
//! one per page open, and a per-day counter of robots turned away. None of it
//! is shown on the site. The contact email is never written into the page; it
//! is handed out only for a solved captcha.

use std::{
    collections::VecDeque,
    fmt,
    fmt::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use axum::{
    Json, Router,
    extract::{ConnectInfo, MatchedPath, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use rusqlite::{Connection, OptionalExtension, params};
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

/// How hard the captcha is to read, and which of the crate's drawings.
const CAPTCHA_DIFFICULTY: captcha::Difficulty = captcha::Difficulty::Easy;
const CAPTCHA_NAME: captcha::CaptchaName = captcha::CaptchaName::Lucy;

/// How long a captcha may wait for its answer. It gets one try either way.
const CAPTCHA_LIFETIME: Duration = Duration::from_secs(600);

/// Captchas held in memory awaiting an answer; past this the oldest is
/// dropped.
const CAPTCHA_LIMIT: usize = 1000;

/// Client addresses given a captcha in any one second; each may have one per
/// second. The rest get a `429`.
const CAPTCHA_ADDRESSES_PER_SECOND: usize = 100;

/// Longest header or client-sent string kept in a `visit` row, in characters.
/// A User-Agent is rarely over 300; this only stops a hostile one filling the
/// disk.
const TEXT_LIMIT: usize = 512;

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
/// Every page load reads `--www/index.html` from disk, so a change to the
/// front end is live on the next load. Meant to sit behind a reverse proxy that
/// terminates TLS; the client address is read from `Forwarded`,
/// `X-Forwarded-For` or `X-Real-IP`, in that order.
#[derive(Debug, Parser)]
#[command(version, author)]
struct CommandLine {
    /// The front end: `index.html` and every file it links to, served as-is.
    #[arg(long, value_name = "DIRECTORY")]
    www: PathBuf,

    /// SQLite database for devices, visits and robot counters. Created if
    /// missing.
    #[arg(long, value_name = "FILE")]
    db: PathBuf,

    /// Where visitors reach the site, like `https://pouriya.net`: the
    /// canonical link and the link-preview URL.
    #[arg(long, value_name = "URL")]
    url: String,

    /// TCP port to listen on, on every interface.
    #[arg(long)]
    port: u16,

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

struct App {
    www: PathBuf,
    /// `--url` without a trailing `/`.
    url: String,
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
    if !settings.www.join("index.html").is_file() {
        bail!(
            "--www {} has no index.html; it must be the front-end directory",
            settings.www.display()
        );
    }
    let url = settings.url.trim_end_matches('/').to_string();
    match url.split_once("://") {
        Some(("http" | "https", rest)) if !rest.is_empty() => {}
        _ => bail!(
            "--url {} must start with http:// or https:// and name a host",
            settings.url
        ),
    }

    let connection = Connection::open(&settings.db)
        .with_context(|| format!("Could not open --db {}", settings.db.display()))?;
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
                 user_agent TEXT NOT NULL,
                 referrer TEXT NOT NULL,
                 language TEXT NOT NULL,
                 path TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS visit_device ON visit (device, at);

             -- Requests refused as robots, counted per UTC day and per kind:
             -- `page`, `visit`, `captcha` or `email`. Nothing else about them
             -- is kept.
             CREATE TABLE IF NOT EXISTS robot (
                 day TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 dropped INTEGER NOT NULL,
                 PRIMARY KEY (day, kind)
             ) WITHOUT ROWID;",
        )
        .with_context(|| format!("Could not set up the schema in {}", settings.db.display()))?;

    let app = Arc::new(App {
        www: settings.www.clone(),
        url,
        database: Mutex::new(connection),
        captchas: Mutex::new(VecDeque::new()),
        recent: Mutex::new(Vec::new()),
    });

    let router = Router::new()
        .route("/", get(page))
        .route("/index.html", get(page))
        .route("/api/visit", post(visit))
        .route("/api/captcha", get(captcha))
        .route("/api/email", post(email))
        .fallback_service(ServeDir::new(&settings.www))
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

    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), settings.port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("Could not listen on {address}"))?;
    tracing::info!(
        msg = "Listening",
        address = %address,
        www = ?settings.www,
        db = ?settings.db,
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

/// `GET /` and `/index.html`: `index.html` with its `{{name}}` fields filled
/// from the settings at the top of this file. Robots are counted and still
/// served, so link previews work.
async fn page(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    robot(&app, &headers, "page").await;

    let template = match tokio::fs::read_to_string(app.www.join("index.html")).await {
        Ok(template) => template,
        Err(error) => {
            tracing::error!(msg = "Could not read index.html", www = ?app.www, error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let url = format!("{}/", app.url);
    let values = [
        ("title", SITE_TITLE),
        ("description", SITE_DESCRIPTION),
        ("url", url.as_str()),
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

    let user_agent = match header_text(&headers, "user-agent") {
        Some(agent) => clip(&agent),
        None => String::new(),
    };
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
            "INSERT INTO visit (device, at, ip, user_agent, referrer, language, path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &device.0[..],
                now(),
                ip.to_string(),
                user_agent,
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
        let made = captcha::by_name(CAPTCHA_DIFFICULTY, CAPTCHA_NAME);
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
struct EmailError {
    /// `captcha`: wrong, expired or already tried.
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
    let mut solved = false;
    {
        let mut captchas = match app.captchas.lock() {
            Ok(captchas) => captchas,
            Err(poisoned) => poisoned.into_inner(),
        };
        let checked = Instant::now();
        captchas.retain(|(_, _, at)| checked.duration_since(*at) < CAPTCHA_LIFETIME);
        let mut found = None;
        for (index, (id, _, _)) in captchas.iter().enumerate() {
            if *id == body.captcha {
                found = Some(index);
            }
        }
        // One try: right or wrong, it is gone.
        if let Some(index) = found
            && let Some((_, answer, _)) = captchas.remove(index)
        {
            solved = answer.eq_ignore_ascii_case(body.answer.trim());
        }
    }
    if !solved {
        return (
            StatusCode::BAD_REQUEST,
            Json(EmailError { error: "captcha" }),
        )
            .into_response();
    }
    tracing::info!(msg = "Gave out the email", ip = %client_ip(&headers, socket));
    Json(EmailResponse { email: EMAIL }).into_response()
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
