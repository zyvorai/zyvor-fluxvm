#!/bin/bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# @raycast.schemaVersion 1
# @raycast.title Kubernetes: Checkpoint
# @raycast.mode compact
# @raycast.packageName FluxVM Kubernetes
# @raycast.icon 📍
# @raycast.argument1 { "type": "text", "placeholder": "label (optional)", "optional": true }

set -euo pipefail
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
FLUXCTL="${FLUXCTL:-$(command -v fluxctl || true)}"
[ -n "$FLUXCTL" ] || { echo "fluxctl not found: brew install zyvorai/tap/fluxvm"; exit 1; }

label="${1:-}"
label="$(printf "%s" "${label:-manual-$(date +%H%M%S)}" | tr "[:upper:] _" "[:lower:]--" | tr -cd "a-z0-9-")"
"$FLUXCTL" kube checkpoint "$label" >/dev/null && echo "Saved checkpoint $label"
