#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Fetch the real-game INI corpus (GeneralsZH data from GeneralsGamePatch2)
# into the gitignored corpus/ directory via a shallow, sparse clone.
#
# Usage: scripts/fetch-corpus.sh [--force]

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dest="$root/corpus/GeneralsGamePatch2"
repo="https://github.com/TheSuperHackers/GeneralsGamePatch2.git"
sparse_path="GeneralsZH/Data/INI"

if [ -d "$dest" ]; then
    if [ "${1:-}" = "--force" ]; then
        rm -rf "$dest"
    else
        echo "corpus already present at $dest (use --force to refetch)"
        exit 0
    fi
fi

git clone --depth 1 --filter=blob:none --sparse "$repo" "$dest"
git -C "$dest" sparse-checkout set "$sparse_path"

count=$(find "$dest/$sparse_path" -name '*.ini' | wc -l)
echo "fetched $count INI files into $dest/$sparse_path"
