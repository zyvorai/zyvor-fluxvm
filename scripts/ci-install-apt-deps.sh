#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Make sure the given apt packages are installed, touching apt (and sudo) only when something is missing.
#
# The zyvor-build self-hosted runners are not uniform: zyvor-build-a/-b let the runner user run apt-get through sudo,
# while -c/-d/-e/-f answer "sudo: I'm sorry ghrunner. I'm afraid I can't do that" to every apt-get call. A bare
# `sudo apt-get update && sudo apt-get install ...` therefore failed `build-and-test` on whichever runner the job
# landed on, before a single line of Rust ran. Checking first means a runner that already carries the packages needs no
# privileges at all, and one that does not fails with the exact remediation instead of a bare sudo refusal.
#
# usage: scripts/ci-install-apt-deps.sh PACKAGE...
set -euo pipefail

[[ $# -gt 0 ]] || { echo "usage: $0 PACKAGE..." >&2; exit 2; }

# Command substitution instead of `dpkg-query | grep -q`: grep -q exits at the first match and, under pipefail, the
# writer's SIGPIPE turns a successful check into a failure.
missing=()
for pkg in "$@"; do
  status="$(dpkg-query -W -f='${db:Status-Status}' "$pkg" 2>/dev/null || true)"
  [[ $status == installed ]] || missing+=("$pkg")
done

if [[ ${#missing[@]} -eq 0 ]]; then
  echo "apt packages already installed: $*"
  exit 0
fi

echo "apt packages to install: ${missing[*]}"
priv=()
if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
  priv=(sudo -n)
fi

export DEBIAN_FRONTEND=noninteractive
if ! "${priv[@]+"${priv[@]}"}" apt-get update -qq ||
  ! "${priv[@]+"${priv[@]}"}" apt-get install -y -qq "${missing[@]}"; then
  cat >&2 <<MSG
::error::cannot install ${missing[*]} on runner '${RUNNER_NAME:-$(hostname)}' ($(id -un) has no usable sudo for apt-get).
This is a runner problem, not a repository one. Fix it once on that runner, as root:
  apt-get update && apt-get install -y ${missing[*]}
or allow the runner user to do it, e.g. in /etc/sudoers.d/ghrunner-apt:
  $(id -un) ALL=(root) NOPASSWD: /usr/bin/apt-get
MSG
  exit 1
fi
