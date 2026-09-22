# Ferroma — Windows entry point for scripts/docker-publish.sh
#
# Why this file exists: Windows has no association for the `.sh` extension, so
# `./scripts/docker-publish.sh` from PowerShell is treated as a *document* to open
# rather than a program to run. Standalone it prints nothing at all and does
# nothing; inside a pipeline it fails with "Cannot run a document in the middle of
# a pipeline". Either way the release silently does not happen.
#
# There is deliberately no publish logic here. The single source of truth is
# `docker-publish.sh` — the only release path, and what `tools/check-deploy.mjs`
# cross-checks against the compose files.
# This wrapper only finds a shell that can execute it and forwards every argument
# verbatim.
#
#   ./scripts/docker-publish.ps1 --dry-run
#   ./scripts/docker-publish.ps1
#   ./scripts/docker-publish.ps1 --repo me/ferroma --platforms linux/amd64
#
# If script execution is disabled — the Windows default, where `Get-ExecutionPolicy
# -List` reports `Undefined` at every scope and the effective policy is Restricted —
# PowerShell refuses to load this file at all. That is a machine policy, not a
# per-file setting, and this wrapper cannot talk its way around it. Either name the
# policy for one run:
#
#   powershell -ExecutionPolicy Bypass -File scripts/docker-publish.ps1 --dry-run
#
# or skip the wrapper entirely: Git for Windows puts `sh` on PATH, and it runs the
# POSIX script directly, with no policy involved:
#
#   sh scripts/docker-publish.sh --dry-run
#
# `bash` on PATH is NOT used directly: on a machine with the WSL launcher it
# resolves to C:\Windows\System32\bash.exe, which is a different distribution
# entirely (and here, a broken one). Only a real Git for Windows shell will do.

[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $Arguments
)

$ErrorActionPreference = 'Stop'

$script = Join-Path $PSScriptRoot 'docker-publish.sh'
if (-not (Test-Path -LiteralPath $script)) {
    Write-Error "docker-publish.sh not found next to this wrapper ($script)"
    exit 1
}

# Git for Windows, wherever this machine put it. `git.exe` on PATH is the most
# reliable hint — the installation directory is two levels above it — with the
# usual install paths as fallbacks.
function Find-GitBash {
    $candidates = @()

    $git = Get-Command git.exe -ErrorAction SilentlyContinue
    if ($git) {
        $candidates += (Join-Path (Split-Path -Parent (Split-Path -Parent $git.Source)) 'bin\bash.exe')
    }
    foreach ($base in @($env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:LOCALAPPDATA)) {
        if ($base) { $candidates += (Join-Path $base 'Programs\Git\bin\bash.exe') }
        if ($base) { $candidates += (Join-Path $base 'Git\bin\bash.exe') }
    }
    $candidates += 'C:\Program Files\Git\bin\bash.exe'

    foreach ($candidate in $candidates) {
        # A WSL launcher is not a Git shell: it would run the script against a
        # different filesystem with no docker on PATH.
        if ($candidate -and (Test-Path -LiteralPath $candidate) -and
            ($candidate -notmatch '\\System32\\bash\.exe$')) {
            return $candidate
        }
    }
    return $null
}

$bash = Find-GitBash
if (-not $bash) {
    Write-Host 'No Git for Windows shell found.' -ForegroundColor Red
    Write-Host 'Install Git for Windows (https://git-scm.com/download/win), or run the'
    Write-Host 'POSIX script from a shell you already have:' -ForegroundColor Yellow
    Write-Host '    sh scripts/docker-publish.sh --dry-run'
    exit 1
}

# A Git shell wants a POSIX path; handing it `C:\a\b` makes it a relative path
# named `C:a\b`.
function ConvertTo-PosixPath([string] $Path) {
    $full = [System.IO.Path]::GetFullPath($Path)
    if ($full -match '^([A-Za-z]):\\(.*)$') {
        return '/' + $Matches[1].ToLowerInvariant() + '/' + ($Matches[2] -replace '\\', '/')
    }
    return ($full -replace '\\', '/')
}

$posixScript = ConvertTo-PosixPath $script

if ($Arguments -and $Arguments.Count -gt 0) {
    & $bash $posixScript @Arguments
} else {
    & $bash $posixScript
}

# Forward the exit code unchanged: a caller (or a wrapper's wrapper) must be able
# to tell a refused publish from a successful one.
exit $LASTEXITCODE
