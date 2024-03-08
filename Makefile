include /usr/share/dpkg/default.mk

DESTDIR=
PREFIX = /usr
LIBDIR = $(PREFIX)/lib
LIBEXECDIR = $(LIBDIR)
DATAROOTDIR = $(PREFIX)/share

PACKAGE := pve-esxi-import-tools
ARCH := $(DEB_BUILD_ARCH)

OUTPUT_DIR := build
BUILD_DIR := $(OUTPUT_DIR)/$(PACKAGE)-$(DEB_VERSION)

ifeq ($(BUILD_MODE), release)
CARGO_BUILD_ARGS += --release
COMPILEDIR := target/release
else
COMPILEDIR := target/debug
endif

DEB=$(PACKAGE)_$(DEB_VERSION)_$(ARCH).deb
DEB_DBGSYM=$(PACKAGE)-dbgsym_$(DEB_VERSION)_$(ARCH).deb
DSC=$(PACKAGE)_$(DEB_VERSION).dsc

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

build-dir:

$(BUILD_DIR):
	rm -rf $@ $@.tmp
	mkdir -p $@.tmp
	echo system >$@.tmp/rust-toolchain
	cp -t $@.tmp -a \
	  debian \
	  Makefile \
	  listvms.py \
	  Cargo.toml \
	  src
	rm -f $@.tmp/Cargo.lock
	mv $@.tmp $@

.PHONY: deb
deb:
	rm -rf $(OUTPUT_DIR)
	$(MAKE) $(OUTPUT_DIR)/$(DEB)

$(OUTPUT_DIR)/$(DEB_DBGSYM): $(OUTPUT_DIR)/$(DEB)
$(OUTPUT_DIR)/$(DEB): $(BUILD_DIR)
	cd $(BUILD_DIR) && CARGO=$(CARGO) RUSTC=$(RUSTC) dpkg-buildpackage -b -uc -us
	lintian $@

.PHONY: dsc
dsc:
	rm -rf $(OUTPUT_DIR)
	$(MAKE) $(OUTPUT_DIR)/$(DSC)
	lintian $(OUTPUT_DIR)/$(DSC)

$(OUTPUT_DIR)/$(DSC): $(BUILD_DIR)
	cd $(BUILD_DIR) && CARGO=$(CARGO) RUSTC=$(RUSTC) dpkg-buildpackage -S -uc -us


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
