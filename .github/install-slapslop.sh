#!/usr/bin/env bash
set -euo pipefail
gh release download --repo tempoxyz/slapslop --pattern 'slapslop-v*-x86_64-unknown-linux-gnu.tar.gz' --dir "$RUNNER_TEMP"
tar -xzf "$RUNNER_TEMP"/slapslop-v*-x86_64-unknown-linux-gnu.tar.gz -C "$RUNNER_TEMP"
echo "$(echo "$RUNNER_TEMP"/slapslop-v*-x86_64-unknown-linux-gnu/bin)" >> "$GITHUB_PATH"
