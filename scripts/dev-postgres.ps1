<#
.SYNOPSIS
    Development PostgreSQL for Ferroma.

.DESCRIPTION
    This machine has no Docker, so Ferroma's tests run against a minimal
    PostgreSQL 16 distribution unpacked into .cache/pgsql with its cluster in
    .cache/pgdata (see AGENTS.md).

    `pg_ctl` cannot be used here: it tries to create a restricted token and fails
    with "could not create restricted token: error code 87". This script therefore
    launches postgres.exe directly and records its PID so `stop`/`status` work.

.EXAMPLE
    .\scripts\dev-postgres.ps1 start
    .\scripts\dev-postgres.ps1 psql      # not available in this distribution
    .\scripts\dev-postgres.ps1 stop
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('start', 'stop', 'status', 'restart', 'reinit', 'url', 'logs')]
    [string]$Action = 'status',

    [int]$Port = 5433
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not $root) { $root = (Get-Location).Path }

$PgBin = Join-Path $root '.cache/pgsql/bin'
$PgData = Join-Path $root '.cache/pgdata'
$PgLog = Join-Path $root '.cache/pg.log'
$PidFile = Join-Path $root '.cache/pg.pid'
$Postgres = Join-Path $PgBin 'postgres.exe'
$InitDb = Join-Path $PgBin 'initdb.exe'

function Get-PgProcess {
    Get-Process -Name postgres -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $Postgres }
}

function Test-Listening {
    param([int]$p)
    try {
        $client = New-Object System.Net.Sockets.TcpClient
        $client.Connect('127.0.0.1', $p)
        $client.Close()
        return $true
    } catch { return $false }
}

switch ($Action) {
    'url' {
        Write-Output "postgres://ferroma@127.0.0.1:$Port/postgres"
    }

    'status' {
        if (-not (Test-Path $Postgres)) {
            Write-Host "PostgreSQL is not unpacked. Run scripts/dev-postgres.ps1 start after downloading it:" -ForegroundColor Yellow
            Write-Host "  node tools/fetch.mjs <embedded-postgres-binaries jar url> .cache/pg.zip"
            exit 1
        }
        $procs = Get-PgProcess
        if ($procs -and (Test-Listening $Port)) {
            Write-Host "running on 127.0.0.1:$Port (pids: $($procs.Id -join ', '))" -ForegroundColor Green
            Write-Host "url: postgres://ferroma@127.0.0.1:$Port/postgres"
            exit 0
        }
        Write-Host 'not running' -ForegroundColor Yellow
        exit 1
    }

    'start' {
        if (Test-Listening $Port) {
            Write-Host "already listening on 127.0.0.1:$Port" -ForegroundColor Green
            exit 0
        }
        if (-not (Test-Path (Join-Path $PgData 'PG_VERSION'))) {
            Write-Host 'initialising a new cluster...'
            New-Item -ItemType Directory -Force -Path $PgData | Out-Null
            & $InitDb -D $PgData -U ferroma --auth=trust --encoding=UTF8 --locale=C | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "initdb failed with exit code $LASTEXITCODE" }
        }

        Write-Host "starting postgres on 127.0.0.1:$Port ..."
        # fsync is off: this cluster exists for tests, not for data anyone loves.
        $proc = Start-Process -FilePath $Postgres -PassThru -WindowStyle Hidden -ArgumentList @(
            '-D', $PgData,
            '-p', $Port,
            '-c', 'listen_addresses=127.0.0.1',
            '-c', 'fsync=off',
            '-c', 'synchronous_commit=off',
            '-c', 'full_page_writes=off',
            '-c', 'logging_collector=off'
        ) -RedirectStandardError $PgLog -RedirectStandardOutput "$PgLog.out"
        $proc.Id | Set-Content $PidFile

        for ($i = 0; $i -lt 30; $i++) {
            Start-Sleep -Milliseconds 500
            if (Test-Listening $Port) {
                Write-Host "ready: postgres://ferroma@127.0.0.1:$Port/postgres" -ForegroundColor Green
                exit 0
            }
        }
        Write-Host 'postgres did not start; see .cache/pg.log' -ForegroundColor Red
        exit 1
    }

    'stop' {
        $procs = Get-PgProcess
        if (-not $procs) {
            Write-Host 'not running'
            exit 0
        }
        # A fast shutdown is what postgres would do on SIGINT; the pid file is only
        # a convenience, the process list is the source of truth.
        foreach ($p in $procs) {
            if ($p.Id -eq (Get-Content $PidFile -ErrorAction SilentlyContinue)) {
                Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
            }
        }
        Start-Sleep -Seconds 2
        Get-PgProcess | Stop-Process -Force -ErrorAction SilentlyContinue
        Remove-Item $PidFile -ErrorAction SilentlyContinue
        Write-Host 'stopped'
    }

    'restart' {
        & $PSCommandPath stop
        Start-Sleep -Seconds 1
        & $PSCommandPath start -Port $Port
    }

    'reinit' {
        # The escape hatch for an unrecoverable cluster.
        #
        # This sandbox forbids cross-process signalling, so the checkpoint the startup
        # process requests at the end of crash recovery fails with
        # "could not signal for checkpoint: Operation not permitted", the startup
        # process exits, and the server then reloads the same WAL and loops forever.
        # A cluster that stopped *cleanly* has no WAL to replay and never requests a
        # checkpoint, so rebuilding the data directory is what actually fixes it.
        Write-Host 'reinitialising the development cluster (all local test data will be lost)'
        & $PSCommandPath stop
        Start-Sleep -Seconds 2

        if (Test-Path $PgData) {
            $stamp = Get-Date -Format 'yyyyMMddHHmmss'
            $moved = "$PgData.broken-$stamp"
            Move-Item $PgData $moved -Force
            Write-Host "old cluster kept at $moved"
        }

        & $InitDb -D $PgData -U ferroma --auth=trust --encoding=UTF8 --locale=C | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "initdb failed with exit code $LASTEXITCODE" }
        Write-Host 'new cluster initialised'

        & $PSCommandPath start -Port $Port
    }

    'logs' {
        Get-Content $PgLog -Tail 40 -ErrorAction SilentlyContinue
    }
}
