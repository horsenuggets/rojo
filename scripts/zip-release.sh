#!/usr/bin/env bash

BIN_NAME="rojo"
BIN_EXT=""
CWD="$PWD"

TARGET_TRIPLE="$1"
if [ -z "$TARGET_TRIPLE" ]; then
    echo "Usage: $0 <TARGET_TRIPLE>"
    exit 1
fi
TARGET_DIR="target/$TARGET_TRIPLE/release"
if [ ! -d "$TARGET_DIR" ]; then
    echo "Target directory '$TARGET_DIR' does not exist"
    exit 1
fi

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$OS" in
    darwin) OS="macos" ;;
    linux) OS="linux" ;;
    cygwin*|mingw*|msys*) OS="windows" ;;
    *)
        echo "Unsupported OS: $OS" >&2
        exit 1 ;;
esac
if [ "$OS" = "windows" ]; then
    BIN_EXT=".exe"
fi

rm -rf staging
rm -rf release.zip

mkdir -p staging
cp "$TARGET_DIR/$BIN_NAME$BIN_EXT" staging/
cd staging

if [ "$OS" = "macos" ]; then
    codesign -s - --force "$BIN_NAME"
fi

if [ "$OS" = "windows" ]; then
    7z a ../release.zip *
else
    chmod +x "$BIN_NAME"
    zip ../release.zip *
fi

cd "$CWD"
rm -rf staging
