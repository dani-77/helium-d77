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
# helium-locker's PAM file goes to $(DESTDIR)/etc/pam.d, not under PREFIX:
# PAM only reads /etc/pam.d. Without it the locker refuses to start.

PREFIX  ?= /usr
DESTDIR ?=

BINDIR  := $(DESTDIR)$(PREFIX)/bin
SHAREDIR := $(DESTDIR)$(PREFIX)/share/helium-d77
PAMDIR  := $(DESTDIR)/etc/pam.d

BINS := helium-shell helium-launcher helium-session helium-osd helium-wallpaper helium-backdrop helium-ollama helium-locker

.PHONY: all build install uninstall clean

all: build

build:
	cargo build --release --locked

install: build
	install -d $(BINDIR)
	$(foreach bin,$(BINS),install -Dm755 target/release/$(bin) $(BINDIR)/$(bin);)
	install -Dm644 assets/d77-logo.svg $(SHAREDIR)/d77-logo.svg
	install -Dm755 monitor/d77-monitor $(SHAREDIR)/monitor/d77-monitor
	install -Dm644 monitor/eww.yuck $(SHAREDIR)/monitor/eww.yuck
	install -Dm644 monitor/eww.scss $(SHAREDIR)/monitor/eww.scss
	install -Dm644 pam.d/helium-locker $(PAMDIR)/helium-locker

uninstall:
	$(foreach bin,$(BINS),rm -f $(BINDIR)/$(bin);)
	rm -f $(SHAREDIR)/d77-logo.svg
	rm -f $(SHAREDIR)/monitor/d77-monitor $(SHAREDIR)/monitor/eww.yuck $(SHAREDIR)/monitor/eww.scss
	rm -f $(PAMDIR)/helium-locker

clean:
	cargo clean
