# Build + system-wide install for helium-shell and its sibling binaries.
#
#   make                 # cargo build --release
#   sudo make install    # install into $(DESTDIR)$(PREFIX)/bin
#   sudo make uninstall
#
# PREFIX defaults to /usr (i.e. binaries land in /usr/bin). DESTDIR is for
# staged/packaging installs (e.g. `make install DESTDIR=/tmp/pkg`) and is
# empty for a normal `sudo make install` onto the running system.
#
# helium-locker is intentionally NOT installed here: under niri it's
# unusable right now (an upstream layer-shika/niri incompatibility cancels
# the session lock ~30ms after activation — see the README's Locker
# section). It still builds fine; install it by hand if your compositor
# doesn't hit that bug:
#   install -Dm755 target/release/helium-locker $(PREFIX)/bin/helium-locker
#   install -Dm644 pam.d/helium-locker /etc/pam.d/helium-locker

PREFIX  ?= /usr
DESTDIR ?=

BINDIR := $(DESTDIR)$(PREFIX)/bin

BINS := helium-shell helium-launcher helium-session helium-osd helium-wallpaper helium-backdrop

.PHONY: all build install uninstall clean

all: build

build:
	cargo build --release --locked

install: build
	install -d $(BINDIR)
	$(foreach bin,$(BINS),install -Dm755 target/release/$(bin) $(BINDIR)/$(bin);)

uninstall:
	$(foreach bin,$(BINS),rm -f $(BINDIR)/$(bin);)

clean:
	cargo clean
