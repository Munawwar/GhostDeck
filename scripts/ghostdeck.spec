%global debug_package %{nil}

Name:       ghostdeck
Version:    %{version}
Release:    1%{?dist}
Summary:    GPU-accelerated terminal workspace manager for Linux
License:    MIT
URL:        https://github.com/Munawwar/GhostDeck
Vendor:     Will R <will@limux.dev>
ExclusiveArch: x86_64 aarch64
AutoReq:    yes
Source0:    ghostdeck-%{version}.tar.gz

%description
GhostDeck is a terminal workspace manager powered by Ghostty's GPU-rendered
terminal engine, with split surfaces and tabbed workspaces.

%prep
%setup -q

%build

%install
rm -rf %{buildroot}
mkdir -p %{buildroot}
cp -a %{_builddir}/ghostdeck-%{version}/usr %{buildroot}/
cp -a %{_builddir}/ghostdeck-%{version}/etc %{buildroot}/

%post
is_legacy_ghostdeck_host() {
    path="$1"
    [ -x "$path" ] || return 1
    help="$("$path" --help 2>&1 || true)"
    echo "$help" | grep -q "ghostdeck CLI" && return 1
    echo "$help" | grep -q "GApplication" && return 0
    "$path" --json identify >/tmp/ghostdeck-postinst-probe.log 2>&1 && return 1
    grep -q "Unknown option --json" /tmp/ghostdeck-postinst-probe.log
}

ldconfig 2>/dev/null || true
rm -f %{_libexecdir}/ghostdeck/ghostdeck
rm -f /usr/local/libexec/ghostdeck/ghostdeck
if is_legacy_ghostdeck_host /usr/local/bin/ghostdeck; then
    rm -f /usr/local/bin/ghostdeck
fi
rm -f %{_datadir}/applications/ghostdeck.desktop
gtk-update-icon-cache -f -t %{_datadir}/icons/hicolor 2>/dev/null || true
update-desktop-database %{_datadir}/applications 2>/dev/null || true
appstreamcli refresh-cache --force 2>/dev/null || true

%postun
ldconfig 2>/dev/null || true
gtk-update-icon-cache -f -t %{_datadir}/icons/hicolor 2>/dev/null || true
update-desktop-database %{_datadir}/applications 2>/dev/null || true
appstreamcli refresh-cache --force 2>/dev/null || true

%files
%{_bindir}/ghostdeck
%{_libexecdir}/ghostdeck/ghostdeck-host
/usr/lib/ghostdeck/libghostty.so
%{_datadir}/ghostdeck/
%{_datadir}/applications/dev.ghostdeck.linux.desktop
%{_datadir}/metainfo/dev.ghostdeck.linux.metainfo.xml
%{_datadir}/icons/hicolor/
%{_sysconfdir}/ld.so.conf.d/ghostdeck.conf

%changelog
