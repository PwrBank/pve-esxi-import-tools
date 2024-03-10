include /usr/share/dpkg/default.mk

DESTDIR=
PREFIX = /usr
LIBEXECDIR = $(PREFIX)/libexec
DATAROOTDIR = $(PREFIX)/share

PACKAGE := pve-esxi-import-tools
ARCH := $(DEB_BUILD_ARCH)

# build in separate directory but output resulting package artefacts top-level by default
# allow to override by passing OUTPUT_DIR explicitly, e.g.: make OUTPUT_DIR=build/ deb
OUTPUT_DIR :=
BUILD_DIR := $(OUTPUT_DIR)$(PACKAGE)-$(DEB_VERSION)

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

CARGO ?= cargo

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
	install -m755 -t $(DESTDIR)$(LIBEXECDIR)/pve-esxi-import-tools $(SCRIPT)

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
	rm -rf $(BUILD_DIR)
	$(MAKE) $(OUTPUT_DIR)$(DEB)

$(OUTPUT_DIR)$(DEB_DBGSYM): $(OUTPUT_DIR)$(DEB)
$(OUTPUT_DIR)$(DEB): $(BUILD_DIR)
	cd $(BUILD_DIR) && dpkg-buildpackage -b -uc -us
	lintian $@

.PHONY: dsc
dsc:
	rm -rf $(BUILD_DIR)
	$(MAKE) $(OUTPUT_DIR)$(DSC)
	lintian $(OUTPUT_DIR)$(DSC)

$(OUTPUT_DIR)$(DSC): $(BUILD_DIR)
	cd $(BUILD_DIR) && dpkg-buildpackage -S -uc -us

sbuild: $(OUTPUT_DIR)$(DSC)
	[ -z "$(OUTPUT_DIR)" ] || cd $(OUTPUT_DIR); sbuild $(DSC)

.PHONY: clean
clean:
	rm -rf $(BUILD_DIR)
	[ -z "$(OUTPUT_DIR)" ] || rm -rf $(OUTPUT_DIR)
	rm -f *.deb *.dsc *.buildinfo *.build *.changes  $(PACKAGE)*.tar*
	$(CARGO) clean

.PHONY: upload
upload: UPLOAD_DIST ?= $(DEB_DISTRIBUTION)
upload: $(OUTPUT_DIR)$(DEB) $(OUTPUT_DIR)$(DEB_DBGSYM)
	[ -z "$(OUTPUT_DIR)" ] || cd $(OUTPUT_DIR); \
	  tar cf - $(DEB) $(DEB_DBGSYM) | ssh -X repoman@repo.proxmox.com upload --product pve --dist $(UPLOAD_DIST)
