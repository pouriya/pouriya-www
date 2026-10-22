ARG DOCKER_REGISTRY
ARG DOCKER_RUST_IMAGE=rust:1.98.1-alpine3.24
ARG DOCKER_ALPINE_IMAGE=alpine:3.24.2
FROM ${DOCKER_REGISTRY}${DOCKER_RUST_IMAGE} AS builder

# gcc and musl-dev because SQLite is compiled from C by its build script; make
# because the build goes through the Makefile like every other one.
RUN apk add --no-cache make gcc musl-dev

WORKDIR /pouriya-www

COPY Cargo.toml Cargo.lock Makefile ./
COPY src src
RUN make release && mv build/pouriya-www-* build/pouriya-www


FROM ${DOCKER_REGISTRY}${DOCKER_ALPINE_IMAGE}
ARG POURIYA_WWW_VERSION
LABEL "org.opencontainers.image.authors"="pouriya.jahanbakhsh@gmail.com"
LABEL "org.opencontainers.image.title"="pouriya-www"
LABEL "org.opencontainers.image.description"="pouriya.net source code"
LABEL "org.opencontainers.image.version"="${POURIYA_WWW_VERSION}"

# The sqlite3 shell, to look into the database from inside the container:
# `docker exec -it <container> sqlite3 /var/lib/pouriya-www/pouriya-www.db`.
RUN apk add --no-cache sqlite

# The front end is baked in: changing the site is building a new image. The
# database is the only state, and lives in the volume.
COPY --from=builder /pouriya-www/build/pouriya-www /usr/local/bin/pouriya-www
COPY www /usr/share/pouriya-www/www
VOLUME /var/lib/pouriya-www
EXPOSE 8080

ENTRYPOINT ["pouriya-www", \
	"--www", "/usr/share/pouriya-www/www", \
	"--db", "/var/lib/pouriya-www/pouriya-www.db", \
	"--port", "8080"]
