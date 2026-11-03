#!/usr/bin/env bash
set -euo pipefail
name=slapslop-v2.3.0-x86_64-unknown-linux-gnu
gh release download v2.3.0 --repo tempoxyz/slapslop --pattern "$name.tar.gz" --dir "$RUNNER_TEMP"
tar -xzf "$RUNNER_TEMP/$name.tar.gz" -C "$RUNNER_TEMP"
echo "$RUNNER_TEMP/$name/bin" >> "$GITHUB_PATH"
