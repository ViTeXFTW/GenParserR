# SPDX-License-Identifier: GPL-3.0-or-later
# Fetch the real-game INI corpus (GeneralsZH data from GeneralsGamePatch2)
# into the gitignored corpus/ directory via a shallow, sparse clone.
#
# Usage: pwsh scripts/fetch-corpus.ps1 [-Force]

param([switch]$Force)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root "corpus/GeneralsGamePatch2"
$repo = "https://github.com/TheSuperHackers/GeneralsGamePatch2.git"
$sparsePath = "GeneralsZH/Data/INI"

if (Test-Path $dest) {
    if ($Force) {
        Remove-Item -Recurse -Force $dest
    } else {
        Write-Host "corpus already present at $dest (use -Force to refetch)"
        exit 0
    }
}

git clone --depth 1 --filter=blob:none --sparse $repo $dest
git -C $dest sparse-checkout set $sparsePath

$iniDir = Join-Path $dest $sparsePath
$count = (Get-ChildItem -Recurse -File -Filter *.ini $iniDir | Measure-Object).Count
Write-Host "fetched $count INI files into $iniDir"
