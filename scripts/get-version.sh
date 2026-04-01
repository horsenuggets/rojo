#!/usr/bin/env bash
set -euo pipefail
MANIFEST=$(cargo read-manifest)
VERSION=$(echo $MANIFEST | jq -r .version)
echo $VERSION
