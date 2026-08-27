PREFIX ?= $(HOME)/.local
GROK_BUILD ?= $(CURDIR)/../grok-build

.PHONY: inject build install smoke

inject:
	python3 scripts/inject.py "$(CURDIR)" "$(GROK_BUILD)"

build: inject
	cd "$(GROK_BUILD)" && cargo build -p hands

install:
	./install.sh

smoke: build
	"$(GROK_BUILD)/target/debug/hands" --version
	"$(GROK_BUILD)/target/debug/hands" --help
	"$(GROK_BUILD)/target/debug/hands" status --json >/dev/null
