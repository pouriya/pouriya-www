# pouriya-www

[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**The source of [pouriya.net](https://pouriya.net).**

One binary and one SQLite file. It serves the pages, counts each page open, each browser and each link followed, privately, shows the contact email only after a captcha is solved, so scrapers don't find it, and keeps the messages people send from the contact page, behind a captcha too. An admin page, at a path only you know and behind a password and a captcha, shows the numbers and the messages and deletes what you don't want.

## Run it

With Docker. `www/` is baked into the image, and the database lives in the `pouriya-www` volume:

```bash
make docker
docker run --rm -i --entrypoint pouriya-www pouriya-www:latest hash-hex    # type the admin password, get its hash in hex
docker run -d --name pouriya-www -p 8080:8080 -v pouriya-www:/var/lib/pouriya-www pouriya-www:latest \
    --url https://example.com --admin-path /some-secret-word --admin-hash-hex <the hex>
```

From source, with a Rust toolchain. `make release` puts the binary in `build/`:

```bash
make release
build/pouriya-www-<version>-<target> hash-hex    # type the admin password, get its hash in hex
build/pouriya-www-<version>-<target> --www www --db pouriya-www.db --port 8080 --url https://example.com \
    --admin-path /some-secret-word --admin-hash-hex <the hex>
```

Every flag is required. `--url` is the address visitors use, and it becomes the page's canonical and link-preview URL. `--admin-path` is where the admin page lives; nothing links to it. `--admin-hash-hex` is the admin password's argon2 hash in hex, so it needs no quoting anywhere: in a shell, a compose file or a deploy form, paste it as printed.

pouriya-www speaks plain HTTP. Put it behind nginx, Caddy or a CDN that terminates TLS.

## Make it yours

The site's settings are constants at the top of `src/main.rs`: title, description, links, the email, the captcha and the message limits. Change them and rebuild. The look is `www/`, where `index.html` (`/`), `software.html` (`/software`) and `contact.html` (`/contact`) are templates with `{{title}}`, `{{description}}`, `{{url}}`, `{{github}}`, `{{telegram}}` and `{{blog}}` filled in by the server.

## Look at the numbers

Open `--admin-path` on your site and log in. Or straight from the database:

```bash
sqlite3 pouriya-www.db 'SELECT count(*) FROM visit; SELECT count(*) FROM device; SELECT link, count(*) FROM click GROUP BY link; SELECT name, text, contact_kind, contact FROM message WHERE seen IS NULL; SELECT * FROM robot;'
```
