CFLAGS ?= -O2 -Wall
XDG_SHELL := $(shell pkg-config --variable=pkgdatadir wayland-protocols)/stable/xdg-shell/xdg-shell.xml

all: overlay gbmprobe

# The layer-shell protocol references xdg_popup, so its generated code needs
# xdg-shell's symbols even though the probe never opens a popup.
wlr-layer-shell-unstable-v1-client-protocol.h: wlr-layer-shell-unstable-v1.xml
	wayland-scanner client-header $< $@

wlr-layer-shell-unstable-v1-protocol.c: wlr-layer-shell-unstable-v1.xml
	wayland-scanner private-code $< $@

xdg-shell-protocol.c:
	wayland-scanner private-code $(XDG_SHELL) $@

overlay: overlay.c wlr-layer-shell-unstable-v1-client-protocol.h \
         wlr-layer-shell-unstable-v1-protocol.c xdg-shell-protocol.c
	$(CC) $(CFLAGS) -o $@ overlay.c wlr-layer-shell-unstable-v1-protocol.c \
	    xdg-shell-protocol.c \
	    $(shell pkg-config --cflags --libs wayland-client wayland-egl egl glesv2)

gbmprobe: gbmprobe.c
	$(CC) $(CFLAGS) -o $@ $< $(shell pkg-config --cflags --libs gbm libdrm)

clean:
	rm -f overlay gbmprobe *-protocol.c *-client-protocol.h

.PHONY: all clean
