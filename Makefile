PREFIX ?= $(HOME)/.local
CONFIG ?= $(HOME)/.config/lintel

.PHONY: build install config service test

build:
	cargo build --release

install: build
	install -Dm755 target/release/lintel $(PREFIX)/bin/lintel

# the config files, only where none is yet
config:
	mkdir -p $(CONFIG)
	test -e $(CONFIG)/lintel.toml || cp config/lintel.toml $(CONFIG)/
	test -e $(CONFIG)/style.css || cp config/style.css $(CONFIG)/

service:
	install -Dm644 systemd/lintel.service $(HOME)/.config/systemd/user/lintel.service
	systemctl --user daemon-reload

test:
	cargo test
