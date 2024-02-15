include /usr/share/dpkg/default.mk

DESTDIR=
PREFIX = /usr
LIBDIR = $(PREFIX)/lib
LIBEXECDIR = $(LIBDIR)
DATAROOTDIR = $(PREFIX)/share

PACKAGE := proxmox-esxi-import
ARCH := $(DEB_BUILD_ARCH)

ifeq ($(BUILD_MODE), release)
CARGO_BUILD_ARGS += --release
COMPILEDIR := target/release
else
COMPILEDIR := target/debug
endif

DEB=$(PACKAGE)_$(DEB_VERSION)_$(ARCH).deb
DSC=rust-$(PACKAGE)_$(DEB_VERSION)_$(ARCH).dsc

BINARY = $(COMPILEDIR)/esxi-folder-fuse
SCRIPT = listvms.py

.PHONY: all
all: $(BINARY)

$(BINARY):
	cargo build $(CARGO_BUILD_ARGS)

.PHONY: check test
check: test
test:
	cargo test $(CARGO_BUILD_ARGS)

.PHONY: install
install: $(BINARY) $(SCRIPT)
	install -m755 -d $(DESTDIR)$(LIBEXECDIR)/proxmox-esxi-import
	install -m755 -t $(DESTDIR)$(LIBEXECDIR)/proxmox-esxi-import $(BINARY)
	install -m755 -d $(DESTDIR)$(LIBDIR)/proxmox-esxi-import
	install -m755 -t $(DESTDIR)$(LIBDIR)/proxmox-esxi-import $(SCRIPT)

build:
	rm -rf build
	mkdir build
	mkdir build/rust-proxmox-esxi-import-$(DEB_VERSION)
	cp -t build/rust-proxmox-esxi-import-$(DEB_VERSION) -a \
	  debian \
	  Makefile \
	  listvms.py \
	  Cargo.toml src
	rm -f build/Cargo.lock

.PHONY: deb
deb:
	rm -rf build
	$(MAKE) build/$(DEB)
build/$(DEB): build
	(cd build/rust-proxmox-esxi-import-$(DEB_VERSION) && \
	  CARGO=/usr/bin/cargo RUSTC=/usr/bin/rustc dpkg-buildpackage -b -uc -us)
	lintian build/*.deb

.PHONY: dsc
dsc:
	rm -rf build
	$(MAKE) build/$(DSC)
build/$(DSC): build
	(cd build/rust-proxmox-esxi-import-$(DEB_VERSION) && \
	  CARGO=/usr/bin/cargo RUSTC=/usr/bin/rustc dpkg-buildpackage -S -uc -us)
	lintian build/*.dsc

.PHONY: clean
clean:
	rm -rf build
	cargo clean
