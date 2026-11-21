set -eu
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH"
ROOT="$HOME/.local/share/fluxvm"
FLUXCTL="$ROOT/current/bin/fluxctl"
[ -x "$FLUXCTL" ] || FLUXCTL="$(command -v fluxctl || true)"
GUI="gui/$(id -u)"
V=0.4.21
BASE=https://github.com/zyvorai/zyvor-fluxvm/releases
if [ -x "$ROOT/$V/bin/fluxctl" ] && [ "$(readlink "$ROOT/current" 2>/dev/null)" = "$V" ]; then echo "fluxvm $V already installed"; exit 0; fi
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || { echo "not Apple silicon macOS" >&2; exit 2; }
T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
A="fluxvm-$V-macos-arm64.tar.gz"
curl -fsSL --retry 3 -o "$T/$A" "$BASE/download/v$V/$A"
curl -fsSL --retry 3 -o "$T/$A.sha256" "$BASE/download/v$V/$A.sha256"
got="$(shasum -a 256 "$T/$A" | awk '{print $1}')"
want="$(awk '{print $1}' "$T/$A.sha256" | head -n1)"
[ "${#want}" = 64 ] || { echo "the .sha256 file is not a SHA-256" >&2; exit 1; }
[ "$got" = "$want" ] || { echo "checksum mismatch: got $got, the release says $want" >&2; exit 1; }
[ "$got" = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" ] || { echo "sha256 $got is not the pinned aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" >&2; exit 1; }
tar -xzf "$T/$A" -C "$T"
mkdir -p "$ROOT" "$HOME/.local/bin"
rm -rf "$ROOT/$V"
mv "$T/fluxvm-$V-macos-arm64" "$ROOT/$V"
ln -sfn "$V" "$ROOT/current"
for f in "$ROOT/current/bin/"*; do
  b="$HOME/.local/bin/$(basename "$f")"
  if [ ! -e "$b" ] || [ -L "$b" ]; then ln -sfn "$f" "$b"; fi
done
if command -v brew >/dev/null 2>&1 && ! brew list hivex >/dev/null 2>&1; then brew install hivex >/dev/null 2>&1 || echo "note: brew install hivex failed; guest image inspection needs it" >&2; fi
echo "installed fluxvm $V (sha256 $got)"
