# Production build of libvds for RHEL-family distributions, run by `make rpm`
# in the top-level Makefile. Source1 holds the vendored crates (offline build).
#
# rust-toolchain.toml pins a rustc newer than the distributions ship, so by
# default rustc/cargo come from PATH (rustup). Build with --with system_rust
# once the distribution's rust is new enough.
%bcond_with system_rust

# Must match the -soname in crates/vds-capi/build.rs.
%global soversion 0

Name:           libvds
Version:        0.1.0
Release:        1%{?dist}
Summary:        In-process RDP display server library
License:        MIT OR Apache-2.0
URL:            https://github.com/jiangcuo/vds
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.gz
Source2:        vds-1.pc.in

ExclusiveOS:    linux
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  binutils
BuildRequires:  nasm

%if %{with system_rust}
BuildRequires:  rust >= 1.94
BuildRequires:  cargo >= 1.94
%endif

%description
libvds is the in-process remote display server (RDP) used by QEMU's -rdp
option. It encodes the guest display with H.264 (openh264), RemoteFX or
bitmaps and carries input, clipboard and audio.

%package devel
Summary:        Development files for libvds
Requires:       %{name}%{?_isa} = %{version}-%{release}
Requires:       pkgconfig

%description devel
The header and the pkg-config file (vds-1) needed to build QEMU against
libvds.

%prep
%autosetup -n %{name}-%{version} -a 1

%build
# The C code built by the crates (openh264, aws-lc) keeps the hardening flags,
# but each build script picks its own -O level (aws-lc must build jitterentropy
# at -O0), and LTO objects cannot go into the archives rustc links.
export CFLAGS="$(echo '%{optflags}' | sed -e 's/-O[0-9s]*//g' -e 's/-flto[^ ]*//g' -e 's/-ffat-lto-objects//g')"
export CXXFLAGS="$CFLAGS"
unset LDFLAGS
# Line tables are enough for backtraces from the debuginfo package.
export CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
export CARGO_PROFILE_RELEASE_STRIP=none
export CARGO_TARGET_DIR=$PWD/target
cargo build --release --frozen -p vds-capi %{?_smp_mflags}
readelf -d target/release/libvds.so | grep -q 'SONAME.*\[libvds\.so\.%{soversion}\]'

%check
cargo test --release --frozen -p vds-capi %{?_smp_mflags}

%install
install -D -m 755 target/release/libvds.so %{buildroot}%{_libdir}/libvds.so.%{version}
ln -s libvds.so.%{version} %{buildroot}%{_libdir}/libvds.so.%{soversion}
ln -s libvds.so.%{soversion} %{buildroot}%{_libdir}/libvds.so
install -D -m 644 include/vds.h %{buildroot}%{_includedir}/vds.h
install -d %{buildroot}%{_libdir}/pkgconfig
sed -e 's|@prefix@|%{_prefix}|' -e 's|@libdir@|%{_libdir}|' \
    -e 's|@includedir@|%{_includedir}|' -e 's|@version@|%{version}|' \
    %{SOURCE2} > %{buildroot}%{_libdir}/pkgconfig/vds-1.pc

%ldconfig_scriptlets

%files
%{_libdir}/libvds.so.%{soversion}
%{_libdir}/libvds.so.%{version}

%files devel
%{_includedir}/vds.h
%{_libdir}/libvds.so
%{_libdir}/pkgconfig/vds-1.pc

%changelog
* Sun Nov 15 2026 Lierfang Support Team <itsupport@lierfang.com> - 0.1.0-1
- Initial production packaging.
