<#
.SYNOPSIS
    Keeps the development PostgreSQL alive.

.DESCRIPTION
    This sandbox forbids cross-process signalling. PostgreSQL notices when it cannot
    signal its own checkpointer — and, on the paths that force other backends to
    terminate (for example `DROP DATABASE ... WITH (FORCE)`), it can lose a backend
    and shut the whole cluster down:

        ERROR:  could not signal for checkpoint: Operation not permitted
        LOG:    server process (PID 3832) was terminated by exception 0xFFFFFFFF
        LOG:    terminating any other active server processes

    The test suite no longer issues any such statement (it isolates tests with
    schemas instead of databases), but a development cluster that dies mid-run is a
    needless interruption. This supervisor runs `postgres.exe` in a loop: when the
    process exits for any reason, it is restarted after a short pause, and the crash
    recovery in the next start replays the WAL.

    Log to `.cache/pg.log`. Stop it with scripts/dev-postgres.ps1 stop.

.EXAMPLE
    .\scripts\pg-supervisor.ps1
#>
[CmdletBinding()]
param(
    [int]$Port = 5433,
    [int]$MaxRestarts = 50,
    [int]$RestartDelaySeconds = 2
)

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
if (-not $root) { $root = (Get-Location).Path }

$Postgres = Join-Path $root '.cache/pgsql/bin/postgres.exe'
$PgData = Join-Path $root '.cache/pgdata'
$Log = Join-Path $root '.cache/pg.log'

if (-not (Test-Path $Postgres)) {
    Write-Error "PostgreSQL is not unpacked at $Postgres (see scripts/dev-postgres.ps1)"
    exit 1
}

function Test-Listening {
    try {
        $client = New-Object System.Net.Sockets.TcpClient
        $client.Connect('127.0.0.1', $Port)
        $client.Close()
        return $true
    } catch { return $false }
}

# Refuse to run twice: two servers on one data directory is data corruption.
if (Test-Listening) {
    Write-Host "another PostgreSQL is already listening on 127.0.0.1:$Port; supervisor exiting"
    exit 0
}

$restarts = 0
while ($restarts -le $MaxRestarts) {
    if ($restarts -gt 0) {
        Write-Host "restart #$restarts in ${RestartDelaySeconds}s..."
        Start-Sleep -Seconds $RestartDelaySeconds
    }

    $stamp = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    "[supervisor $stamp] starting postgres on 127.0.0.1:$Port" | Add-Content -Path $Log -Encoding utf8

    # Foreground: the loop is the supervision.
    #
    # max_connections is raised well above the default 100 because the test suite
    # runs many test binaries in parallel and each test opens its own pool; at 100
    # the failures appear as unrelated connection timeouts rather than as a clear
    # "too many clients" error.
    & $Postgres -D $PgData -p $Port `
        -c listen_addresses=127.0.0.1 `
        -c max_connections=400 `
        -c fsync=off `
        -c synchronous_commit=off `
        -c full_page_writes=off 2>&1 | Add-Content -Path $Log -Encoding utf8

    $code = $LASTEXITCODE
    $stamp = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    "[supervisor $stamp] postgres exited with code $code" | Add-Content -Path $Log -Encoding utf8

    $restarts++
}

Write-Host "supervisor giving up after $MaxRestarts restarts; see .cache/pg.log"
exit 1
