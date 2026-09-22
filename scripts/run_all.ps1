<#
.SYNOPSIS
  Full benchmark: noop calibration -> redis-ping transport baseline -> redis ->
  mongo -> cassandra -> neo4j (scenario "normal"), then a short "celebrity"
  run per database (closed, c=8 and c=32, 1 run, 60 s). Each database runs
  alone (run_db.ps1 tears its container down with -v before the next starts).
#>
param(
    # Comma-separated (a string so it survives `powershell -File`).
    [string]$Dbs = 'redis,mongo,cassandra,neo4j',
    [int]$Runs = 3,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$dbList = @($Dbs -split '[,\s]+' | Where-Object { $_ -ne '' })
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root

function Invoke-Native {
    param([string]$Exe, [string[]]$Arguments)
    Write-Host "> $Exe $($Arguments -join ' ')"
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & $Exe @Arguments
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prev
    if ($code -ne 0) { throw "$Exe $($Arguments -join ' ') failed with exit code $code" }
}

Invoke-Native 'cargo' @('build', '--release')
$bench = Join-Path $root 'target\release\bench.exe'
$forceArg = @()
if ($Force) { $forceArg = @('--force') }

# Only one database container may run at a time.
$prev = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
$running = @(docker ps --filter 'name=nosqlbench-' --format '{{.Names}}')
$ErrorActionPreference = $prev
if ($LASTEXITCODE -ne 0) { throw "docker ps failed with exit code $LASTEXITCODE" }
if ($running.Count -gt 0) { throw "Database containers already running: $($running -join ', '). Stop them first." }

for ($r = 1; $r -le $Runs; $r++) {
    Invoke-Native $bench (@('calibrate', '--run', "$r") + $forceArg)
}

$common = @{ Runs = $Runs }
if ($Force) { $common['Force'] = $true }

& (Join-Path $PSScriptRoot 'run_db.ps1') -Db 'redis-ping' @common
foreach ($db in $dbList) {
    & (Join-Path $PSScriptRoot 'run_db.ps1') -Db $db @common
}

# Celebrity scenario: most-followed users are also the most active.
$celeb = @{ Scenario = 'celebrity'; Levels = '8,32'; Runs = 1; NoOpen = $true; MeasureSecs = 60 }
if ($Force) { $celeb['Force'] = $true }
foreach ($db in $dbList) {
    & (Join-Path $PSScriptRoot 'run_db.ps1') -Db $db @celeb
}
Write-Host 'All sweeps complete.'
