# Cargo wrapper: applies the Ferroma build environment, then runs cargo.
#
#   .\scripts\cargo.ps1 build --workspace
#   .\scripts\cargo.ps1 test  --workspace
#
# The crates proxy is started automatically if it is not already listening.

param([Parameter(ValueFromRemainingArguments = $true)][string[]]$CargoArgs)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'env.ps1')

function Test-Proxy {
    try {
        $r = Invoke-WebRequest -Uri "http://127.0.0.1:$env:FERROMA_PROXY_PORT/health" -TimeoutSec 2 -UseBasicParsing
        return $r.StatusCode -eq 200
    } catch { return $false }
}

if (-not (Test-Proxy)) {
    Write-Host 'starting crates proxy...'
    Start-Process -FilePath 'node' `
        -ArgumentList (Join-Path $FerromaRoot 'tools/crates-proxy.mjs') `
        -WindowStyle Hidden
    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Milliseconds 500
        if (Test-Proxy) { break }
    }
    if (-not (Test-Proxy)) { throw 'crates proxy failed to start' }
}

& cargo @CargoArgs
exit $LASTEXITCODE
