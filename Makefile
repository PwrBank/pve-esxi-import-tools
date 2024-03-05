include /usr/share/dpkg/default.mk

DESTDIR=
PREFIX = /usr
LIBDIR = $(PREFIX)/lib
LIBEXECDIR = $(LIBDIR)
DATAROOTDIR = $(PREFIX)/share

PACKAGE := pve-esxi-import-tools
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

CARGO := /usr/bin/cargo
RUSTC := /usr/bin/rustc

.PHONY: all
all: $(BINARY)

$(BINARY):
	$(CARGO) build $(CARGO_BUILD_ARGS)

.PHONY: check test
check: test
test:
	$(CARGO) test $(CARGO_BUILD_ARGS)

.PHONY: install
install: $(BINARY) $(SCRIPT)
	install -m755 -d $(DESTDIR)$(LIBEXECDIR)/pve-esxi-import-tools
	install -m755 -t $(DESTDIR)$(LIBEXECDIR)/pve-esxi-import-tools $(BINARY)
	install -m755 -d $(DESTDIR)$(LIBDIR)/pve-esxi-import-tools
	install -m755 -t $(DESTDIR)$(LIBDIR)/pve-esxi-import-tools $(SCRIPT)

build:
	rm -rf build
	mkdir build
	mkdir build/rust-pve-esxi-import-tools-$(DEB_VERSION)
	echo system >build/rust-pve-esxi-import-tools-$(DEB_VERSION)/rust-toolchain
	cp -t build/rust-pve-esxi-import-tools-$(DEB_VERSION) -a \
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
	(cd build/rust-pve-esxi-import-tools-$(DEB_VERSION) && \
	  CARGO=$(CARGO) RUSTC=$(RUSTC) dpkg-buildpackage -b -uc -us)
	lintian build/*.deb

.PHONY: dsc
dsc:
	rm -rf build
	$(MAKE) build/$(DSC)
build/$(DSC): build
	(cd build/rust-pve-esxi-import-tools-$(DEB_VERSION) && \
	  CARGO=$(CARGO) RUSTC=$(RUSTC) dpkg-buildpackage -S -uc -us)
	lintian build/*.dsc

.PHONY: clean
clean:
	rm -rf build
	$(CARGO) clean

.PHONY: upload
upload: UPLOAD_DIST ?= $(DEB_DISTRIBUTION)
upload: build/$(DEB)
	cd build; \
	    dcmd --deb rust-pve-esxi-import-tools_*.changes \
	    | grep -v '.changes$$' \
	    | tar -cf- -T- \
	    | ssh -X repoman@repo.proxmox.com upload --product pve --dist $(UPLOAD_DIST)
