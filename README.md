# pouriya-www

[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**The source of [pouriya.net](https://pouriya.net).**

One binary and one SQLite file. It serves the page, counts each page open and each browser privately, and shows the contact email only after a captcha is solved, so scrapers don't find it.

## Run it

With Docker. `www/` is baked into the image, and the database lives in the `pouriya-www` volume:

```bash
make docker
docker run -d --name pouriya-www -p 8080:8080 -v pouriya-www:/var/lib/pouriya-www pouriya-www:latest --url https://example.com
```

From source, with a Rust toolchain. `make release` puts the binary in `build/`:

```bash
make release
build/pouriya-www-<version>-<target> --www www --db pouriya-www.db --port 8080 --url https://example.com
```

Every flag is required. `--url` is the address visitors use, and it becomes the page's canonical and link-preview URL.

pouriya-www speaks plain HTTP. Put it behind nginx, Caddy or a CDN that terminates TLS.

## Make it yours

The site's settings are constants at the top of `src/main.rs`: title, description, links, the email and the captcha limits. Change them and rebuild. The look is `www/`, where `index.html` is a template with `{{title}}`, `{{description}}`, `{{url}}`, `{{github}}`, `{{telegram}}` and `{{blog}}` filled in by the server.

## Look at the numbers

```bash
sqlite3 pouriya-www.db 'SELECT count(*) FROM visit; SELECT count(*) FROM device; SELECT * FROM robot;'
```
