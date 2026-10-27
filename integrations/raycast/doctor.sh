#!/bin/bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# @raycast.schemaVersion 1
# @raycast.title Kubernetes: Doctor
# @raycast.mode fullOutput
# @raycast.packageName FluxVM Kubernetes
# @raycast.icon 🩺

set -euo pipefail
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
FLUXCTL="${FLUXCTL:-$(command -v fluxctl || true)}"
[ -n "$FLUXCTL" ] || { echo "fluxctl not found: brew install zyvorai/tap/fluxvm"; exit 1; }

"$FLUXCTL" kube doctor || true
