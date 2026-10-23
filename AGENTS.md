# pouriya-www

The source of pouriya.net. One binary serves a front end (`--www`) with
`index.html`, `software.html` and `contact.html` filled in from settings in
`src/main.rs`, counts who came and which links they followed, and keeps the
messages sent from the contact page, in SQLite (`--db`). Nothing
it counts is shown on the site: only on the admin page at `--admin-path`,
behind a password and a captcha. Plain HTTP on `--port`, meant to sit behind a
reverse proxy that terminates TLS; `--url` is the address visitors use.

## ⚠️ Read this before writing any code

**ASK, DO NOT DECIDE.** Every design or behaviour choice — a format, a
dependency, a flag, a table, a UI detail — is the author's. Put the options to
them, recommended first, and wait. Do not pick a default and explain later.

**DO NOT CREATE NEW FILES UNLESS EXPLICITLY TOLD TO.** The backend is
`src/main.rs` and nothing else; the layout below is closed. No `utils.rs`,
`db.rs`, `lib.rs` or any other split on your own initiative. If you think a
split is needed, say so and wait for an answer.

**INLINE EVERYTHING UNLESS YOU KNOW IT WILL BE REUSED.** One caller means no
function: paste the body into the call site. This applies to functions, types,
constants and modules alike. Abstraction is earned by a second caller that
exists today, never by anticipation. When a helper's only other caller
disappears, collapse it back.

**USE `make`. DO NOT CALL `cargo` DIRECTLY.** `cargo build` does not pin
`--target`, `cargo clippy` without `-D warnings` does not fail.
**Before reporting any task finished, `make all` must pass.** Quote its
result; do not claim it passed without running it.

## Make targets

| Target | What it does |
|--------|--------------|
| `make all` | **The gate**: `dev` + `clippy` + `test` + `check-style` |
| `make dev` | Debug build → `build/pouriya-www-<version>-<target>-dev` |
| `make release` | Release build → `build/pouriya-www-<version>-<target>` |
| `make start-dev` | `dev`, then serves `www/` on `PORT` (8080) with `--debug`, database `pouriya-www.db`, `--url` from `URL` (`http://localhost:$PORT`), the admin page at `ADMIN_PATH` (`/dev-admin`) with password `ADMIN_PASSWORD` (`dev`) |
| `make dist` | `build/dist/pouriya-www-<version>-<target>.tar.gz` (binary + `www/`) and its `.sha256` |
| `make docker` | Alpine image `pouriya-www:<version>` and `pouriya-www:latest`, `www/` baked in |
| `make clippy` | `cargo clippy --all-targets --no-deps -- -D warnings` |
| `make check-style` | `cargo fmt --check` |
| `make fmt` | Rewrites formatting in place |
| `make lint` | `clippy` + `check-style`, no build |
| `make test` | `cargo test --target …` |
| `make clean` / `dist-clean` | Drop `target/` / also `build/` |

## Layout

```
src/main.rs     The whole backend: settings consts, CLI, schema, routes, handlers
www/            The front end, served as-is from --www
  index.html    Template for / and /index.html; {{name}} filled by the server.
                Name, tagline, and the three ways on: software, fragments
                (plogia.ir, new tab, ?l= in the page's language) and contact.
                English and Persian
  software.html Template for /software: open-source contributions and projects.
                English only
  pouriya.png   The portrait: beside the name on the home page, and og:image
                on every page. ASCII art on white, blended into the page
  contact.html  Template for /contact: Telegram, X, the email behind a captcha,
                and a message form behind another. English and Persian
  admin.html    The admin page, served as-is at --admin-path: login, then
                Overview, Messages, Visits and Clicks. English only
  fonts/        Inconsolata (English) and Vazirmatn (Persian), woff2, each
                with its OFL licence beside it
Makefile        Every build/check entry point. Use it, not cargo.
README.md       For people running it
LICENSE         MIT
Dockerfile      Pinned rust-alpine builder, pinned alpine runtime
```

## Settings

No configuration file and no environment variables. What the site *is* lives
as `const`s at the top of `src/main.rs`: `SITE_TITLE` (`pouriya.net`),
`SITE_DESCRIPTION` (`Pouriya Jahanbakhsh`), `LINK_GITHUB`
(`https://github.com/pouriya`), `LINK_TELEGRAM` (`https://t.me/plogia`),
`LINK_BLOG` (`https://plogia.ir`), `EMAIL`, `CAPTCHA_LENGTH` (5),
`CAPTCHA_LIFETIME` (10 minutes), `CAPTCHA_LIMIT` (1000),
`CAPTCHA_ADDRESSES_PER_SECOND` (100), `MESSAGE_NAME_LIMIT` (100 characters),
`MESSAGE_WORD_LIMIT` (1000), `MESSAGE_CHARACTER_LIMIT` (5000),
`MESSAGE_BYTE_LIMIT` (10000), `MESSAGE_CONTACT_LIMIT` (200),
`MESSAGE_CONTACT_KINDS` (`phone`, `telegram`, `email`, `other`), `TEXT_LIMIT`,
`ROBOT_MARKERS`, `ADMIN_SESSION_IDLE` (1 hour), `ADMIN_COOKIE`
(`pouriya-www-admin`), `ADMIN_PAGE_SIZES` (20, 50, 100).
What differs per machine comes from the command line, and every flag is
mandatory with no default:

```
pouriya-www --www DIR --db FILE --port PORT --url URL --admin-path PATH --admin-hash HASH [--debug | --trace | --quiet]
pouriya-www hash
```

`--url` (`https://pouriya.net`) is where visitors reach the site: the
canonical link and `og:url`. `--admin-path` is where the admin page is served,
a plain path of its own (letters, digits, `-_.~`, no trailing `/`) that no page,
route or file in `--www` takes; it is never written in the source, the docs or
any page. `--admin-hash` is the admin password's argon2 PHC string, printed by
`pouriya-www hash` from one line of stdin; both are checked at startup.

It binds `0.0.0.0:<port>`, so it works inside a container; on a host, keep the
port firewalled so only the proxy reaches it.

## Template

`index.html` (`/`), `software.html` (`/software`) and `contact.html`
(`/contact`) are read from disk on every request, so a front-end change is live
on the next load. The last two are never served raw: `/software.html` and
`/contact.html` are `404`. `admin.html` is read the same way but served as-is,
nothing filled, at `--admin-path` only (`/admin.html` is `404`), with
`Cache-Control: no-store`. `{{name}}` is replaced, HTML-escaped, in one pass; unknown
names are left as written.

| Name | Value |
|------|-------|
| `title` | `SITE_TITLE` |
| `description` | `SITE_DESCRIPTION` |
| `url` | `--url` then the page's path: `/`, `/software` or `/contact` |
| `home` | `--url` then `/`, on every page: the site root, for `og:image` |
| `github` | `LINK_GITHUB` |
| `telegram` | `LINK_TELEGRAM` |
| `blog` | `LINK_BLOG` |

**`EMAIL` is never a template name.** It is not in the page, so scrapers
reading the HTML do not find it; only `POST /api/email` gives it out.

`software.html` is English only. `contact.html` holds both languages:
**Persian by default** — the HTML itself is written in Persian, right to left,
`og:locale=fa_IR`, so link previews and visitors without JavaScript get Persian
— and an English / فارسی toggle at the top switches the text and the direction
in place and is remembered in `localStorage` (`pouriya-www.language`); `?l=fa`
or `?l=en` in the link wins. `/software` links to `/contact?l=en`. Persian is set
in Vazirmatn, everything else in Inconsolata. `index.html` works the same way,
sharing the remembered language with `/contact`.

Tab titles are `پوریا` for the home page and `پوریا · ارتباط با من` for the
contact page, in English `pouriya` and `pouriya · contact`; `pouriya · software`
is English only.

## HTTP

| Route | What |
|-------|------|
| `GET /`, `/index.html` | `www/index.html` with its `{{name}}` fields filled |
| `GET /software`, `/contact` | `www/software.html`, `www/contact.html` the same way |
| `GET /go?to=URL&from=PATH&device=ID` | Records the click, then `303` to `URL`; `404` unless a `www/*.html` page has `href="/go?to=URL"` |
| `POST /api/click` | By beacon: `{link, action, page, device?}`, `action` `copy` (a handle or the email copied) or `open` (the email's `mailto:` link) → `204`; `404` unless `/go` would take `link` or it is `mailto:` + `EMAIL` |
| `POST /api/visit` | The handshake, once per page open: `{device?, path, referrer}` → `{device}` |
| `GET /api/captcha` | `{id, image}`, a `data:image/png` URL; `429` + `Retry-After: 1` over the limit |
| `POST /api/email` | `{captcha, answer}` → `{email}`, or `400 {error: "captcha"}` |
| `POST /api/message` | `{name, text, contact_kind, contact, language, device?, captcha, answer}` → `204`, or `400 {error}` naming `name`, `text`, `contact` or `captcha` |
| `GET <--admin-path>` | `www/admin.html` as-is |
| `POST /api/admin/login` | `{password, captcha, answer}` → `204` + session cookie, or `400 {error}` naming `captcha` or `password` |
| `POST /api/admin/logout` | Ends the session → `204` |
| `POST /api/admin/overview` | `{from?, offset}` → totals, per-day counts, top paths, referrers and links |
| `POST /api/admin/{visits,clicks,messages}` | `{filter, page, size}` → `{columns, total, rows}` |
| `POST /api/admin/{visits,clicks,messages}/delete` | `{ids}` or `{filter}` → `{deleted}` |
| `POST /api/admin/messages/seen` | `{ids, seen}` → `204` |
| anything else | A file from `--www`, else `404` |

Behind the proxy, the client address is the right-most entry of `Forwarded:
for=`, then `X-Forwarded-For`, then `X-Real-IP`, then the socket. The
right-most entry is the one our own proxy appended; a client can forge
anything to its left.

Everything answers `Cache-Control: no-cache` unless the handler set its own:
the browser revalidates and gets a 304 for an unchanged file. Every response
carries `X-Content-Type-Options: nosniff`.

## Devices, visits, robots

```sql
device (id BLOB PRIMARY KEY, created_at)          -- ULID, one per browser
visit  (id, device, at, ip, referrer, language, path)   -- every page open
click  (id, at, ip, link, page, device?, action)  -- every outbound link followed (open) or handle/email copied (copy)
message (id, at, ip, device?, language, name, text, contact_kind, contact, seen?)  -- contact-page messages; seen = when marked read
robot  (day, kind, dropped, PRIMARY KEY (day, kind))  -- refused robots, per UTC day
```

- **The device id is a ULID** issued by `POST /api/visit` and kept in the
  browser's `localStorage` (`pouriya-www.device`). An id the database does not
  know gets a new one. 48 bits of milliseconds then 80 random bits from
  `getrandom`, Crockford base32, hand-written after borhan's, no monotonic
  factory.
- **A view is a page open**: one `visit` row. Links between our own pages are
  plain (`/software`, `/contact?l=en`), never `/go`: the visit and its
  `referrer` (the page it came from) are the record of the move. Unique visitors are `device`
  rows. Strings from the client or its headers are clipped to `TEXT_LIMIT`
  characters.
- **Nothing is shown publicly.** The numbers stay in SQLite and are seen only
  on the admin page. The User-Agent is read to spot robots and never stored.
  The schema is `CREATE TABLE IF NOT EXISTS` with no migrations: a change to
  it comes with an `ALTER TABLE` run by hand on the production database
  before the redeploy.
- **Clicks**: every link to another site is `href="/go?to=URL"` with
  `target="_blank"`, written literally (not through `{{name}}`), because the
  pages are the allow-list: `/go` reads every `www/*.html` and refuses a `to` none of them
  links to, so it is no open redirect. The page script appends `from` (its
  path) and `device` once it has one. `device` is kept only if the database
  knows it. A redirect, not a beacon, so leaving the page cannot cancel the
  record; with JavaScript off the click is recorded without page or device.
  What `/go` cannot see comes by `sendBeacon` to `POST /api/click`: copying a
  handle or the revealed email on `/contact` (the browser's `copy` event, when
  the selection takes in one), `action` `copy`; and opening the email's
  `mailto:` link, `action` `open`. Rows from `/go` are `open`.
- **Robots** are refused from visits, clicks, captchas, the email and
  messages, and only counted under `kind` `page`, `visit`, `click`, `captcha`,
  `email` or `message`. A robot
  following `/go` is still redirected. `page` is still
  served, so link previews work. A User-Agent not starting with `Mozilla/`, or
  containing one of `ROBOT_MARKERS`, is a robot; the front end also sends
  nothing when `navigator.webdriver` is set. A refused API call gets `204`.
  Times are Unix milliseconds.

## Email and messages behind captchas

- **Captchas** are drawn like the `captcha` crate's `Lucy` preset at `Medium`
  difficulty (noise 0.4, a 6×6 grid, 220×120), but always `CAPTCHA_LENGTH`
  characters, where the presets pick 4 to 6. Held in memory only: at most `CAPTCHA_LIMIT`, the oldest
  pushed out past that, each good for `CAPTCHA_LIFETIME` and one try, answered
  without case. One per client address per second, and at most
  `CAPTCHA_ADDRESSES_PER_SECOND` addresses in any second; the rest get `429`.
- **Email**, on `/contact`: a "Show email" button opens the captcha picture, ↻
  for a new one, and an answer field. A right answer replaces the form with a
  `mailto:` link; a wrong one fetches a new captcha. Nothing is remembered: a
  reload asks again.
- **Messages**, on `/contact`, with their own captcha, fetched the first time
  the form is touched so opening the page costs none. A required name (up to
  `MESSAGE_NAME_LIMIT` characters), a required text (at most
  `MESSAGE_WORD_LIMIT` words, `MESSAGE_CHARACTER_LIMIT` characters and
  `MESSAGE_BYTE_LIMIT` UTF-8 bytes, all after trimming), and an optional way
  to reply: a kind from `MESSAGE_CONTACT_KINDS` and up to
  `MESSAGE_CONTACT_LIMIT` characters of anything, not checked further. The
  fields are checked before the captcha, which a typo should not cost; the
  front end checks the same limits first. Sent, the form is replaced by "Your
  message was sent."; another needs a reload. Messages are only stored, for the
  author's own dashboard: no log line, no notification. `device` is kept only
  if the database knows it; `language` is the page's, to reply in.
- Each email given out logs `Gave out the email` (`info`) with the client
  `ip`. It is not stored in the database.

## Admin page

- **Login** is the password and a captcha (the same `/api/captcha`), checked
  captcha first so every password tried costs one. A success logs `Admin
  logged in`, a wrong password `Refused admin login` (`warn`), both with `ip`.
- **Sessions** are random 256-bit tokens held in memory, in an `HttpOnly;
  Secure; SameSite=Strict` cookie scoped to `/api/admin`. Each ends after
  `ADMIN_SESSION_IDLE` without a request, at logout, or on restart. Every
  `/api/admin` route but login answers `401` without one, and all of them
  `Cache-Control: no-store`.
- **Nothing the admin does is counted**: the page sends no visit, and the API
  records nothing. Visits to the public pages from the same browser still
  count; nothing marks a device as the admin's.
- **Layout**: a side menu (a hamburger on phones) with Overview, the default,
  then Messages (unread count beside it), Visits and Clicks. The page never
  scrolls; every table scrolls inside its own box under a sticky header.
- **Overview**: Today, 7 days, 30 days or All, days in the browser's zone
  (`offset`); totals, a per-day table and the top 10 paths, referrers and
  links. Robots are kept per UTC day and shown that way. On phones the four
  tables are one at a time.
- **Messages, Visits, Clicks**: a search on top, a table, and a pager always in
  view with the rows per page (`ADMIN_PAGE_SIZES`). The search is one line of
  `Key:"value"` words, coloured as typed, with keys suggested, a time picker
  for `From`/`To` and bad words marked: `From`, `To`, `Ip`, `Device`, `Text`
  (inside the section's text columns), and `Seen:yes|no` on messages. A click
  on an IP or device searches for it. Rows are deleted by checkbox (at most a
  page, by id) or all that match the search (by filter, on the server,
  however many). Deleting visits also deletes the devices left with none and
  forgets them on clicks and messages. A message row is bold while unread,
  opens its whole text in place on a click, and has a read/unread toggle;
  selected rows can be marked together.

## Docker

Built by `make docker` from base images pinned to exact versions —
`DOCKER_RUST_IMAGE` (`rust:1.98.1-alpine3.24`) and `DOCKER_ALPINE_IMAGE`
(`alpine:3.24.2`), defaults in both the Makefile and the Dockerfile — so a
build reuses what is cached and never pulls a tag that moved. Docker Hub is not
reachable from every build machine; `DOCKER_REGISTRY` prefixes both base images
when a mirror is needed. `DOCKER_IMAGE` names the image for a registry to push
to (`make docker DOCKER_IMAGE=<registry>/pouriya-www`). `www/` is baked in, so
changing the site is building and pushing a new image. The database is the
only state: `VOLUME /var/lib/pouriya-www`. The runtime image carries the
`sqlite3` shell for debugging:
`docker exec -it <container> sqlite3 /var/lib/pouriya-www/pouriya-www.db`.

```
docker run -d -p 8080:8080 -v pouriya-www:/var/lib/pouriya-www pouriya-www:latest \
    --url https://pouriya.net --admin-path /<secret> --admin-hash '<hash>'
```

`--url`, `--admin-path` and `--admin-hash` are the flags the image leaves to
you: they go after the image name. The hash comes from the image too:
`docker run --rm -i --entrypoint pouriya-www pouriya-www:latest hash`.

## Code style conventions

- **Plain `for` loops over iterator method chains.**
- **`match` over combinators for `Result`**, **`if let`/`match` over
  combinators for `Option`.** Where clippy insists otherwise, clippy wins.
- **Don't extract single-use helpers.** Same rule as at the top.
- **`anyhow` at the binary boundary**: `fn main() -> anyhow::Result<()>`, `?`
  plus `.context(...)`. Never convert errors to `String` to propagate them.

## Lint & style conventions

- **No `#[allow(...)]` anywhere** — fix the root cause.
- **`make clippy` is the gate** — `-D warnings`.

## Testing

No `#[cfg(test)]` blocks in `src/` — ever. Tests live in `tests/` and drive the
built binary over HTTP; there are none yet.

## Logging conventions

`tracing` only, JSON to stderr, installed in `main`. **Default level is
`INFO`**, because this is a server and its journal wants the access log.

| Flag | Level | Extra fields |
|------|-------|--------------|
| `--quiet` | `OFF` | — |
| *(none)* | `INFO` | — |
| `--debug` | `DEBUG` | target |
| `--trace` | `TRACE` | target, file, line |

Every request logs one event: `Handled request` (2xx/3xx, `info`), `Rejected
request` (4xx, `warn`) or `Failed request` (5xx, `error`), with
`http.request.method`, `http.route`, `url.path`, `url.query`,
`http.response.status_code` and `total_ms`.

Every log call starts with a `msg` field whose value begins with a capital
letter; structured fields follow. `?` for `Debug`, `%` for `Display`.

```rust
tracing::error!(msg = "Could not record visit", error = %format!("{error:#}"));
```

| Level | When to use |
|-------|-------------|
| `info` | Important success event (listening, a request handled) |
| `warn` | Intentionally ignored error, or a fallback being taken |
| `error` | Failure the server recovers from; one it does not is returned from `main` |
| `debug` | Before attempting something important, with its inputs |
| `trace` | After a low-level operation, with what it produced |
