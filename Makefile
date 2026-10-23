TARGET ?= $(shell rustc -vV 2>/dev/null | awk '$$1 == "host:"{print $$2}')
BUILD_DIR=$(CURDIR)/build
CARGO_TARGET_DIR ?= $(CURDIR)/target
VERSION=$(shell cat Cargo.toml | awk 'BEGIN{FS="[ \"]"}$$1 == "version"{print $$4;exit}')
CMD=${BUILD_DIR}/pouriya-www-${VERSION}-${TARGET}
DEV_CMD=${BUILD_DIR}/pouriya-www-${VERSION}-${TARGET}-dev

DIST_DIR=${BUILD_DIR}/dist
DIST_NAME=pouriya-www-${VERSION}-${TARGET}

# What `make start-dev` serves, and where it keeps its database.
WWW_DIR ?= $(CURDIR)/www
DB_FILE ?= $(CURDIR)/pouriya-www.db
PORT ?= 8080
# The canonical link and link-preview URL in index.html.
URL ?= http://localhost:${PORT}
# The admin page and its password, for development only: production passes
# its own --admin-path and --admin-hash.
ADMIN_PATH ?= /dev-admin
ADMIN_PASSWORD ?= dev

# `make docker`. DOCKER_REGISTRY prefixes both base images, with its trailing
# slash, for a machine that cannot reach Docker Hub; empty means whatever the
# local daemon resolves (a cached image counts). Both base images are pinned to
# exact versions, so a build reuses what is cached instead of pulling a moved
# tag; bump them here on purpose, not by accident.
DOCKER_REGISTRY ?=
DOCKER_RUST_IMAGE ?= rust:1.98.1-alpine3.24
DOCKER_ALPINE_IMAGE ?= alpine:3.24.2
DOCKER_IMAGE ?= pouriya-www
DOCKER_IMAGE_VERSION ?= ${VERSION}


all: dev clippy test check-style


release: ${BUILD_DIR}
	cargo build --release --target ${TARGET}
	@ cp ${CARGO_TARGET_DIR}/${TARGET}/release/pouriya-www ${CMD}
	@ ls -sh ${BUILD_DIR}/pouriya-www-*


dev: ${BUILD_DIR}
	cargo build --target ${TARGET}
	@ cp ${CARGO_TARGET_DIR}/${TARGET}/debug/pouriya-www ${DEV_CMD}
	@ ls -sh ${BUILD_DIR}/pouriya-www-*dev*


start-dev: dev
	${DEV_CMD} --debug --www ${WWW_DIR} --db ${DB_FILE} --port ${PORT} --url ${URL} \
		--admin-path ${ADMIN_PATH} \
		--admin-hash "$$(printf '%s\n' '${ADMIN_PASSWORD}' | ${DEV_CMD} hash)"


# The release binary and www/ in ${DIST_DIR}/${DIST_NAME}.tar.gz, plus its
# .sha256. The database is the operator's: --db.
dist:
	cargo build --release --target ${TARGET}
	@ rm -rf ${DIST_DIR}/${DIST_NAME}
	@ mkdir -p ${DIST_DIR}/${DIST_NAME}
	@ cp ${CARGO_TARGET_DIR}/${TARGET}/release/pouriya-www ${DIST_DIR}/${DIST_NAME}/
	@ cp -r www ${DIST_DIR}/${DIST_NAME}/
	@ cd ${DIST_DIR} && COPYFILE_DISABLE=1 tar czf ${DIST_NAME}.tar.gz ${DIST_NAME}
	@ cd ${DIST_DIR} && { command -v sha256sum >/dev/null && sha256sum ${DIST_NAME}.tar.gz \
		|| shasum -a 256 ${DIST_NAME}.tar.gz ; } > ${DIST_NAME}.tar.gz.sha256
	@ echo ${DIST_DIR}/${DIST_NAME}.tar.gz


# Alpine image ${DOCKER_IMAGE}:<version> and :latest, with www/ baked in.
# Builds inside Docker with `make release`. Pushing is not here.
docker:
	docker build --build-arg DOCKER_REGISTRY=${DOCKER_REGISTRY} \
		--build-arg DOCKER_RUST_IMAGE=${DOCKER_RUST_IMAGE} \
		--build-arg DOCKER_ALPINE_IMAGE=${DOCKER_ALPINE_IMAGE} \
		--build-arg POURIYA_WWW_VERSION=${VERSION} \
		--force-rm -t ${DOCKER_IMAGE}:${DOCKER_IMAGE_VERSION} -t ${DOCKER_IMAGE}:latest .


# The Cargo.toml version, for scripts and the image tag.
version:
	@ echo ${VERSION}


clippy:
	cargo clippy --all-targets --no-deps -- -D warnings


check-style:
	cargo fmt --check --verbose


fmt:
	cargo fmt


lint: clippy check-style


test:
	cargo test --target ${TARGET}


clean:
	@ cargo clean


dist-clean: clean
	@ rm -rf ${BUILD_DIR}


${BUILD_DIR}:
	@ mkdir -p ${BUILD_DIR}


.PHONY: all release dev start-dev dist docker version clippy check-style fmt lint test clean dist-clean
