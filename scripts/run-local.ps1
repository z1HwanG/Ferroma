<#
.SYNOPSIS
    Start Ferroma locally, against the development PostgreSQL in this repository.

.DESCRIPTION
    The `ferroma` binary reads its configuration from `config/ferroma.toml` (embedded
    in the binary, overridable by a file) and then from the environment. On a normal
    deployment the only thing you supply is `DATABASE_URL`; everything else has a
    working default.

    This machine is not normal (see AGENTS.md): the development PostgreSQL listens on
    5433 rather than 5432, and port 8080 is already claimed on loopback by another
    process, so the API goes on 8180. This script encodes exactly that, so the server
    starts with one command instead of five environment variables.

.EXAMPLE
    .\scripts\run-local.ps1                # start in the foreground
    .\scripts\run-local.ps1 -Port 9000     # a different API port
    .\scripts\run-local.ps1 -Release       # the release binary
#>
[CmdletBinding()]
param(
    # API/Webmail/Admin port. 8080 is taken on loopback on this machine.
    [int]$Port = 8180,
    # Use target/release/ferroma instead of target/debug/ferroma.
    [switch]$Release,
    # Start with this database. `ferroma database init` creates it if missing.
    [string]$Database = 'ferroma'
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'env.ps1')

$profile = if ($Release) { 'release' } else { 'debug' }
$exe = Join-Path $env:CARGO_TARGET_DIR "$profile/ferroma.exe"
if (-not $Release) {
    $exe = Join-Path $env:CARGO_TARGET_DIR "debug/ferroma.exe"
}

if (-not (Test-Path $exe)) {
    Write-Host "no binary at $exe" -ForegroundColor Red
    Write-Host "build it first:  cargo build -p ferroma-server"
    exit 1
}

# The database has to exist before `serve` will start. Creating it is idempotent.
$env:DATABASE_URL = "postgres://ferroma@127.0.0.1:5433/$Database"
$env:FERROMA_DATA_DIR = Join-Path $env:FERROMA_ROOT 'data'
$env:FERROMA_API_PORT = "$Port"
# Without this every restart invalidates all sessions. Generate your own for real use:
#   openssl rand -base64 48
$env:FERROMA_JWT_SECRET = 'local-development-secret-of-at-least-32-characters'

Write-Host "database  $env:DATABASE_URL"
Write-Host "data dir  $env:FERROMA_DATA_DIR"
Write-Host "webmail   http://127.0.0.1:$Port/"
Write-Host "admin     http://127.0.0.1:$Port/admin"
Write-Host ''

& $exe --config (Join-Path $env:FERROMA_ROOT 'config/ferroma.toml') serve
exit $LASTEXITCODE
