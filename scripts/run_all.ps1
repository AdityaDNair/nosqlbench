<#
.SYNOPSIS
  Full benchmark: noop calibration -> redis-ping transport baseline -> redis ->
  mongo -> cassandra -> neo4j. Each database runs alone (run_db.ps1 tears its
  container down with -v before the next starts).
#>
param(
    [string[]]$Dbs = @('redis', 'mongo', 'cassandra', 'neo4j'),
    [int]$Runs = 3,
    [switch]$NoVmAffinity,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
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
if ($NoVmAffinity) { $common['NoVmAffinity'] = $true }
if ($Force) { $common['Force'] = $true }

& (Join-Path $PSScriptRoot 'run_db.ps1') -Db 'redis-ping' @common
foreach ($db in $Dbs) {
    & (Join-Path $PSScriptRoot 'run_db.ps1') -Db $db @common
}
Write-Host 'All sweeps complete.'
