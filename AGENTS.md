# pouriya-www

The source of pouriya.net. One binary serves a front end (`--www`) with
`index.html` filled in from settings in `src/main.rs`, and counts who came in
SQLite (`--db`). Nothing it counts is shown on the site. Plain HTTP on
`--port`, meant to sit behind a reverse proxy that terminates TLS; `--url` is
the address visitors use.

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
| `make start-dev` | `dev`, then serves `www/` on `PORT` (8080) with `--debug`, database `pouriya-www.db`, `--url` from `URL` (`http://localhost:$PORT`) |
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
                For now only the visit handshake and the email captcha, unstyled
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
`LINK_BLOG` (`https://plogia.ir`), `EMAIL`, `CAPTCHA_DIFFICULTY` (Easy),
`CAPTCHA_NAME` (Lucy), `CAPTCHA_LIFETIME` (10 minutes), `CAPTCHA_LIMIT`
(1000), `CAPTCHA_ADDRESSES_PER_SECOND` (100), `TEXT_LIMIT`, `ROBOT_MARKERS`.
What differs per machine comes from the command line, and every flag is
mandatory with no default:

```
pouriya-www --www DIR --db FILE --port PORT --url URL [--debug | --trace | --quiet]
```

`--url` (`https://pouriya.net`) is where visitors reach the site: the
canonical link and `og:url`.

It binds `0.0.0.0:<port>`, so it works inside a container; on a host, keep the
port firewalled so only the proxy reaches it.

## Template

`index.html` is read from disk on every request, so a front-end change is live
on the next load. `{{name}}` is replaced, HTML-escaped, in one pass; unknown
names are left as written.

| Name | Value |
|------|-------|
| `title` | `SITE_TITLE` |
| `description` | `SITE_DESCRIPTION` |
| `url` | `--url` with a trailing `/` |
| `github` | `LINK_GITHUB` |
| `telegram` | `LINK_TELEGRAM` |
| `blog` | `LINK_BLOG` |

**`EMAIL` is never a template name.** It is not in the page, so scrapers
reading the HTML do not find it; only `POST /api/email` gives it out.

The page is English (`lang="en"`, `og:locale=en_US`). A Persian switch may
come later.

## HTTP

| Route | What |
|-------|------|
| `GET /`, `/index.html` | `www/index.html` with its `{{name}}` fields filled |
| `POST /api/visit` | The handshake, once per page open: `{device?, path, referrer}` → `{device}` |
| `GET /api/captcha` | `{id, image}`, a `data:image/png` URL; `429` + `Retry-After: 1` over the limit |
| `POST /api/email` | `{captcha, answer}` → `{email}`, or `400 {error: "captcha"}` |
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
visit  (id, device, at, ip, user_agent, referrer, language, path)   -- every page open
robot  (day, kind, dropped, PRIMARY KEY (day, kind))  -- refused robots, per UTC day
```

- **The device id is a ULID** issued by `POST /api/visit` and kept in the
  browser's `localStorage` (`pouriya-www.device`). An id the database does not
  know gets a new one. 48 bits of milliseconds then 80 random bits from
  `getrandom`, Crockford base32, hand-written after borhan's, no monotonic
  factory.
- **A view is a page open**: one `visit` row. Unique visitors are `device`
  rows. Strings from the client or its headers are clipped to `TEXT_LIMIT`
  characters.
- **Nothing is shown publicly.** The numbers stay in SQLite; a private stats
  page behind a login and a captcha is planned, not built.
- **Robots** are refused from visits, captchas and the email, and only
  counted under `kind` `page`, `visit`, `captcha` or `email`. `page` is still
  served, so link previews work. A User-Agent not starting with `Mozilla/`, or
  containing one of `ROBOT_MARKERS`, is a robot; the front end also sends
  nothing when `navigator.webdriver` is set. A refused API call gets `204`.
  Times are Unix milliseconds.

## Email behind a captcha

- **Captchas** are the `captcha` crate's `CAPTCHA_DIFFICULTY` preset
  (`CAPTCHA_NAME`), held in memory only: at most `CAPTCHA_LIMIT`, the oldest
  pushed out past that, each good for `CAPTCHA_LIFETIME` and one try, answered
  without case. One per client address per second, and at most
  `CAPTCHA_ADDRESSES_PER_SECOND` addresses in any second; the rest get `429`.
- **Front end**: a "Show email" button opens the captcha picture, ↻ for a new
  one, and an answer field. A right answer replaces the form with a `mailto:`
  link; a wrong one fetches a new captcha. Nothing is remembered: a reload asks
  again.
- Each email given out logs `Gave out the email` (`info`) with the client
  `ip`. It is not stored in the database.

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
docker run -d -p 8080:8080 -v pouriya-www:/var/lib/pouriya-www pouriya-www:latest --url https://pouriya.net
```

`--url` is the one flag the image leaves to you: it goes after the image name.

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
