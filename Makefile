# Production packages of libvds (libvds0/libvds-dev, libvds/libvds-devel).
#
#   make deb     Debian packages in build/deb/
#   make rpm     binary and source RPMs in build/rpm/RPMS/ and build/rpm/SRPMS/
#   make clean   remove build/
#
# Both build the committed source (git archive HEAD) offline against the crates
# vendored from its Cargo.lock; only vendoring needs the network. rustc/cargo
# come from PATH (rustup, rust-toolchain.toml).
# Pass extra options with DEB_BUILD_FLAGS=... or RPMBUILD_FLAGS=...

NAME := libvds
VERSION := $(shell sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
BUILDDIR := build
SRCDIR := $(BUILDDIR)/src/$(NAME)-$(VERSION)
SRCTAR := $(BUILDDIR)/$(NAME)-$(VERSION).tar.gz
VENDORTAR := $(BUILDDIR)/$(NAME)-$(VERSION)-vendor.tar.gz
DEBDIR := $(BUILDDIR)/deb
DEBSRC := $(DEBDIR)/$(NAME)-$(VERSION)
RPMTOP := $(abspath $(BUILDDIR)/rpm)
SPEC := packaging/rpm/$(NAME).spec

.PHONY: deb rpm clean sources

deb: sources
	@deb_version=$$(dpkg-parsechangelog -l packaging/debian/changelog -SVersion); \
	if [ "$${deb_version%-*}" != "$(VERSION)" ]; then \
		echo "packaging/debian/changelog is at $$deb_version, Cargo.toml at $(VERSION)" >&2; exit 1; \
	fi
	rm -rf $(DEBDIR)
	mkdir -p $(DEBDIR)
	tar -C $(DEBDIR) -xzf $(SRCTAR)
	tar -C $(DEBSRC) -xzf $(VENDORTAR)
	rm -rf $(DEBSRC)/debian
	cp -a packaging/debian $(DEBSRC)/debian
	cp packaging/vds-1.pc.in $(DEBSRC)/debian/
	cd $(DEBSRC) && dpkg-buildpackage -b -us -uc $(DEB_BUILD_FLAGS)
	@ls -l $(DEBDIR)/*.deb

rpm: sources
	@rpm_version=$$(sed -n 's/^Version: *//p' $(SPEC)); \
	if [ "$$rpm_version" != "$(VERSION)" ]; then \
		echo "$(SPEC) is at $$rpm_version, Cargo.toml at $(VERSION)" >&2; exit 1; \
	fi
	rm -rf $(RPMTOP)
	mkdir -p $(RPMTOP)/SOURCES
	cp $(SRCTAR) $(VENDORTAR) packaging/vds-1.pc.in $(RPMTOP)/SOURCES/
	rpmbuild -ba --define "_topdir $(RPMTOP)" $(RPMBUILD_FLAGS) $(SPEC)
	@find $(RPMTOP)/RPMS $(RPMTOP)/SRPMS -name '*.rpm' -exec ls -l {} +

# Internal: the source and vendor tarballs both packaging formats start from.
sources:
	@git diff --quiet HEAD -- || echo "warning: uncommitted changes are not packaged (git archive HEAD)" >&2
	rm -rf $(BUILDDIR)/src $(SRCTAR) $(VENDORTAR)
	mkdir -p $(BUILDDIR)/src
	git archive --format=tar --prefix=$(NAME)-$(VERSION)/ HEAD | tar -C $(BUILDDIR)/src -x
	# git archive leaves submodules out: add FFmpeg at the commit HEAD records
	test -e third_party/ffmpeg/configure || git submodule update --init --depth 1 third_party/ffmpeg
	git -C third_party/ffmpeg archive --format=tar \
		--prefix=$(NAME)-$(VERSION)/third_party/ffmpeg/ $$(git rev-parse HEAD:third_party/ffmpeg) \
		| tar -C $(BUILDDIR)/src -x
	tar -C $(BUILDDIR)/src -czf $(SRCTAR) $(NAME)-$(VERSION)
	mkdir -p $(SRCDIR)/.cargo
	cd $(SRCDIR) && cargo vendor --locked --versioned-dirs vendor > .cargo/config.toml
	tar -C $(SRCDIR) -czf $(VENDORTAR) vendor .cargo

clean:
	rm -rf $(BUILDDIR)
