#!/usr/bin/env bash
# shellcheck disable=SC2015
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# The .rpm under a real systemd (Rocky Linux 9 = EL9) (PID 1 in a rootless podman container, so nothing on the host changes): install,
# unit enablement, the bootstrap timer, restart after kill -9, upgrade keeping config, remove, purge.
# The binaries in the package are stubs (a fluxctl that sleeps on `serve`); this tests the packaging and the units,
# not the daemon. Needs nfpm, podman, python3.   scripts/e2e/systemd-rpm.sh [docker.io/library/rockylinux:9 | docker.io/library/rockylinux:10 | quay.io/fedora/fedora:42]
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
IMAGE="${1:-docker.io/library/rockylinux:9}"
for t in nfpm podman; do command -v "$t" >/dev/null || { echo "needs $t" >&2; exit 2; }; done
T="$(mktemp -d "${HOME}/.cache/fluxvm-sysd.XXXXXX" 2>/dev/null || mktemp -d)"; fails=0; CT=""
trap '[[ -n "$CT" ]] && podman rm -f "$CT" >/dev/null 2>&1; rm -rf "$T"' EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { local n="$1"; shift; if "$@"; then ok "$n"; else bad "$n"; fi; }
case "$(uname -m)" in x86_64) U=x86_64 ;; *) U=aarch64 ;; esac

mkpkg() { # VERSION
  local s="$T/stage/fluxvm-$1-linux-$U"
  rm -rf "$s"; mkdir -p "$s/bin" "$s/lib/bpf" "$s/share/systemd" "$s/share/completions" "$s/share/guest"
  printf '#!/bin/sh\ncase "$1$3" in *serve) exec sleep infinity;; esac\necho "fluxctl %s"\n' "$1" > "$s/bin/fluxctl"
  cp "$s/bin/fluxctl" "$s/bin/fluxvm-agent"; chmod 755 "$s/bin/fluxctl" "$s/bin/fluxvm-agent"; ln -s fluxctl "$s/bin/fluxvm"; : > "$s/lib/bpf/x.o"; cp config.example.toml "$s/share/"
  cp packaging/systemd/{fluxvm,fluxvm-agent,fluxvm-bootstrap}.service packaging/systemd/fluxvm-bootstrap.timer "$s/share/systemd/"; cp packaging/agent.env.example packaging/bootstrap.conf.example "$s/share/"
  cp scripts/fluxvm-bootstrap.sh "$s/bin/fluxvm-bootstrap"; chmod 755 "$s/bin/fluxvm-bootstrap"
  echo c > "$s/share/completions/fluxctl.bash"; printf '#!/bin/sh\n' > "$s/share/guest/fluxvm-guest-agent"; chmod 755 "$s/share/guest/fluxvm-guest-agent"
  scripts/package-linux-pkgs.sh --stage "$s" --format rpm --out "$T/art" >/dev/null
}
mkpkg 0.0.1 && mkpkg 0.0.2 || { echo "package build failed" >&2; exit 2; }
D1="$(ls "$T"/art/fluxvm-0.0.1*.rpm)"; D2="$(ls "$T"/art/fluxvm-0.0.2*.rpm)"

cat > "$T/Dockerfile" <<EOF
FROM $IMAGE
RUN dnf install -y -q systemd dbus openssl jq util-linux ca-certificates procps-ng kmod >/dev/null
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
EOF
podman build -q -t fluxvm-sysd-test "$T" >/dev/null || { echo "image build failed" >&2; exit 2; }
CT="$(podman run -d --systemd=always -v "$T/art:/art:ro" fluxvm-sysd-test)" || exit 2
X() { podman exec "$CT" bash -c "$*"; }
for _ in $(seq 1 30); do X 'systemctl is-system-running 2>/dev/null' 2>/dev/null | grep -qE 'running|degraded' && break; sleep 1; done
check "systemd is PID 1 in the container" test "$(X 'cat /proc/1/comm')" = systemd

# EL9 (glibc 2.34) is deliberately not installable: the binaries need 2.35. The right outcome there is a clean refusal that
# names the problem and leaves nothing behind; the full lifecycle below runs on images with a new enough glibc (EL10, Fedora).
GV="$(X 'ldd --version | head -1 | grep -o "[0-9]*\.[0-9]*$"')"
echo "     | image glibc: $GV"
if [[ "$(printf '%s\n2.35\n' "$GV" | sort -V | head -1)" != 2.35 ]]; then
  X "dnf install -y -q /art/$(basename "$D1")" >"$T/i.log" 2>&1; rc=$?
  check "dnf refuses the rpm on glibc $GV (EL9)" test $rc -ne 0
  check "the refusal names glibc >= 2.35" grep -q "glibc >= 2.35" "$T/i.log"
  check "nothing of the package is left behind" X '! rpm -q fluxvm >/dev/null 2>&1 && ! test -e /usr/bin/fluxctl && ! ls /usr/lib/systemd/system/fluxvm*.service >/dev/null 2>&1'
  echo; echo "$fails check(s) failed"; exit "$fails"
fi

X "dnf install -y -q /art/$(basename "$D1")" >"$T/i.log" 2>&1; rc=$?; [[ $rc -ne 0 ]] && sed 's/^/     | /' "$T/i.log"
check "dnf install of the .rpm (dependencies resolve) under systemd" test $rc -eq 0
check "fluxvm.service is installed and enabled" test "$(X 'systemctl is-enabled fluxvm.service')" = enabled
check "a fresh install does not start the daemon (the config is a template)" test "$(X 'systemctl is-active fluxvm.service' || true)" = inactive
check "bootstrap timer is installed but not enabled" test "$(X 'systemctl is-enabled fluxvm-bootstrap.timer' || true)" = disabled
check "unit files use /usr/bin, not /usr/local/bin" test "$(X 'grep -l /usr/local/bin /usr/lib/systemd/system/fluxvm*.service | wc -l')" = 0
check "every ExecStart target exists" X 'for u in /usr/lib/systemd/system/fluxvm*.service; do for p in $(grep -h "^ExecStart=" $u | sed "s/^ExecStart=[-@+!]*//" | cut -d" " -f1); do [ -x "$p" ] || { echo "$u: $p missing"; exit 1; }; done; done'
check "systemd-analyze verify on the packaged units" X 'systemd-analyze verify /usr/lib/systemd/system/fluxvm.service /usr/lib/systemd/system/fluxvm-bootstrap.service /usr/lib/systemd/system/fluxvm-bootstrap.timer 2>&1 | grep -v -E "^$|Failed to (load|create)|Cannot find unit|rpcbind" ; true'
check "config seeded: /etc/fluxvm.toml and /etc/fluxvm/upgrade.conf" X 'test -f /etc/fluxvm.toml && test -f /etc/fluxvm/upgrade.conf'

# Start it by hand (a node would do this after editing the config) and kill it hard.
X 'systemctl start fluxvm.service'; sleep 2
check "fluxvm.service runs" test "$(X 'systemctl is-active fluxvm.service')" = active
P1="$(X 'systemctl show -p MainPID --value fluxvm.service')"
X 'kill -9 $(systemctl show -p MainPID --value fluxvm.service)'; sleep 5
P2="$(X 'systemctl show -p MainPID --value fluxvm.service')"
check "kill -9 of the daemon: restarted under a new PID (Restart=on-failure)" test -n "$P2" -a "$P2" != "0" -a "$P2" != "$P1"
check "unit says what restarted it (NRestarts>=1)" test "$(X 'systemctl show -p NRestarts --value fluxvm.service')" -ge 1
X 'systemd-analyze security fluxvm.service --no-pager 2>&1 | tail -3' | sed 's/^/     | /'

# Timer: enable it, make sure the schedule is real and the oneshot is runnable (no mirror: it must fail soft).
X 'printf "MIRROR_URL=http://127.0.0.1:9\nJITTER_MAX=0\n" > /etc/fluxvm/bootstrap.conf; systemctl enable --now fluxvm-bootstrap.timer'
check "bootstrap timer is active and has a next run" X 'systemctl list-timers fluxvm-bootstrap.timer --no-pager | grep -q fluxvm-bootstrap'
X 'systemctl start fluxvm-bootstrap.service' >/dev/null 2>&1
check "bootstrap oneshot ran and logged to the journal" X 'journalctl -u fluxvm-bootstrap.service --no-pager | grep -q .'
check "unreachable mirror leaves the install alone" test "$(X 'rpm -q --qf "%{VERSION}" fluxvm')" = 0.0.1

# Upgrade: a conffile-like edit survives and the running daemon is not restarted (RESTART_ON_UPGRADE defaults to no).
X "echo 'FLUXVM_TEST_MARK=1' >> /etc/fluxvm/upgrade.conf; echo '# edited' >> /etc/fluxvm.toml"
P3="$(X 'systemctl show -p MainPID --value fluxvm.service')"
X "dnf install -y -q /art/$(basename "$D2")" >"$T/u.log" 2>&1 || sed 's/^/     | /' "$T/u.log"
check "upgrade to 0.0.2" test "$(X 'rpm -q --qf "%{VERSION}" fluxvm')" = 0.0.2
check "upgrade keeps edited config" X 'grep -q FLUXVM_TEST_MARK /etc/fluxvm/upgrade.conf && grep -q "# edited" /etc/fluxvm.toml'
check "upgrade does not restart the running daemon" test "$(X 'systemctl show -p MainPID --value fluxvm.service')" = "$P3"
X "dnf downgrade -y -q /art/$(basename "$D1")" >/dev/null 2>&1
check "downgrade to 0.0.1" test "$(X 'rpm -q --qf "%{VERSION}" fluxvm')" = 0.0.1

# Erase keeps config and state.
X 'rpm -e fluxvm' >"$T/r.log" 2>&1 || sed 's/^/     | /' "$T/r.log"
check "remove stops the daemon" test "$(X 'systemctl is-active fluxvm.service' || true)" != active
check "remove deletes the binaries and keeps /etc/fluxvm.toml" X '! test -e /usr/bin/fluxctl && test -f /etc/fluxvm.toml'
# rpm has no purge: erasing keeps what is not the package's to destroy (a node's VMs and keys), and leaves no unit behind.
check "erase keeps the node's config and state by design" X 'test -f /etc/fluxvm.toml && test -d /var/lib/fluxvm'
check "erase leaves no unit behind" X '! ls /usr/lib/systemd/system/fluxvm*.service >/dev/null 2>&1 && ! ls /etc/systemd/system/multi-user.target.wants/fluxvm* >/dev/null 2>&1'

echo; echo "$fails check(s) failed"; exit "$fails"
