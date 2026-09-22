<#
.SYNOPSIS
  Full benchmark: noop calibration -> redis-ping transport baseline -> redis ->
  mongo -> cassandra -> neo4j (scenario "normal"), then a short "celebrity"
  run per database (closed, c=8 and c=32, 1 run, 60 s). Each database runs
  alone (run_db.ps1 tears its container down with -v before the next starts).

  This is the run that produces the final numbers: run it once, in a single
  session (same boot), so every database is measured under the same host
  conditions. Everything goes to -Out (default results\final): the noop
  calibration and the redis-ping transport floor come first.
#>
param(
    # Comma-separated (a string so it survives `powershell -File`).
    [string]$Dbs = 'redis,mongo,cassandra,neo4j',
    [int]$Runs = 3,
    [string]$Out = 'results\final',
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

# Pre-flight: Docker must be running and the benchmark ports must be free
# (a leftover container or a native service would otherwise be measured).
$prev = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
docker info --format '{{.ServerVersion}}' 2>$null | Out-Null
$dockerExit = $LASTEXITCODE
$ErrorActionPreference = $prev
if ($dockerExit -ne 0) { throw 'Docker is not running (docker info failed). Start Docker Desktop first.' }
$busyPorts = @()
foreach ($port in 6379, 27018, 9042, 7687, 7474) {
    foreach ($c in @(Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue)) {
        $owner = (Get-Process -Id $c.OwningProcess -ErrorAction SilentlyContinue).ProcessName
        $busyPorts += "port $port in use by $owner (pid $($c.OwningProcess))"
    }
}
if ($busyPorts.Count -gt 0) { throw ("Benchmark ports are not free: " + ($busyPorts -join '; ')) }
Write-Host 'Pre-flight OK: Docker running; ports 6379, 27018, 9042, 7687, 7474 free.'

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
    Invoke-Native $bench (@('calibrate', '--run', "$r", '--out', $Out) + $forceArg)
}

$common = @{ Runs = $Runs; Out = $Out }
if ($Force) { $common['Force'] = $true }

& (Join-Path $PSScriptRoot 'run_db.ps1') -Db 'redis-ping' @common
foreach ($db in $dbList) {
    & (Join-Path $PSScriptRoot 'run_db.ps1') -Db $db @common
}

# Celebrity scenario: most-followed users are also the most active.
$celeb = @{ Scenario = 'celebrity'; Levels = '8,32'; Runs = 1; NoOpen = $true; MeasureSecs = 60; Out = $Out }
if ($Force) { $celeb['Force'] = $true }
foreach ($db in $dbList) {
    & (Join-Path $PSScriptRoot 'run_db.ps1') -Db $db @celeb
}
Write-Host 'All sweeps complete.'
