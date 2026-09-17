# Ferroma build environment
#
# This host has two constraints that the build has to work around:
#   1. Windows schannel is broken (SEC_E_NO_CREDENTIALS), so any TLS done by
#      cargo/curl/git fails. `tools/crates-proxy.mjs` serves the crates.io sparse
#      index and crate archives over plain HTTP on localhost using Node's OpenSSL.
#   2. The session file sandbox only allows writes inside the project tree, so
#      CARGO_HOME is redirected into `.cargo-home/` instead of `~/.cargo`.
#
# Everything therefore lives inside the repository. Dot-source this file (or use
# scripts/cargo.ps1) before running cargo:
#
#     . .\scripts\env.ps1
#     cargo build --workspace

$FerromaRoot = Split-Path -Parent $PSScriptRoot
if (-not $FerromaRoot) { $FerromaRoot = (Get-Location).Path }

$env:FERROMA_ROOT = $FerromaRoot
$env:CARGO_HOME = Join-Path $FerromaRoot '.cargo-home'
$env:CARGO_TARGET_DIR = Join-Path $FerromaRoot 'target'
$env:FERROMA_PROXY_ROOT = $FerromaRoot
$env:FERROMA_PROXY_PORT = '8931'

# Keep the rustup toolchain where it is; only the registry/cache moves.
$env:RUSTUP_HOME = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }

New-Item -ItemType Directory -Force -Path $env:CARGO_HOME | Out-Null

Write-Host "FERROMA_ROOT   = $env:FERROMA_ROOT"
Write-Host "CARGO_HOME     = $env:CARGO_HOME"
Write-Host "CARGO_TARGET_DIR = $env:CARGO_TARGET_DIR"
