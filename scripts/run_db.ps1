<#
.SYNOPSIS
  Full sweep for one database: up -> healthy -> load -> closed sweep ->
  open sweep -> down -v.

  No Docker VM CPU affinity is applied: it was tested (scripts\affinity_ab.ps1)
  and rejected in M3. bench records the VM's actual mask in meta.json when it
  can read it (reading needs elevation; otherwise the error is recorded).

.PARAMETER Db
  redis | redis-ping | mongo | cassandra | neo4j. redis-ping runs inside the
  redis profile, loads nothing and only runs the closed sweep.
.PARAMETER KeepUp
  Leave the container running at the end (default: down -v).
.PARAMETER Force
  Overwrite existing result directories.
.PARAMETER Scenario
  normal (default) | celebrity. Selects the dataset/op-stream scenario for both
  the load and the runs; results go to <Out>\<scenario>\<db>\.
.PARAMETER NoOpen
  Skip the open-mode sweep.
.PARAMETER WarmupSecs
  Override run.warmup_secs (default: from config).
.PARAMETER MeasureSecs
  Override run.measure_secs (default: from config).
#>
# No [Parameter()] attributes: they would make this an advanced script whose
# common parameter -Debug (alias -db) collides with -Db.
param(
    [ValidateSet('redis', 'redis-ping', 'mongo', 'cassandra', 'neo4j')]
    [string]$Db,
    # Comma-separated lists. Strings, not [int[]]: via `powershell -File`,
    # "8,32" arrives as one string and [int[]] would parse it as 832.
    [string]$Levels = '1,8,32,128',
    [int]$Runs = 3,
    [string]$OpenPercents = '50,80,95',
    [string]$Out = 'results',
    [switch]$KeepUp,
    [switch]$Force,
    [ValidateSet('normal', 'celebrity')]
    [string]$Scenario = 'normal',
    [switch]$NoOpen,
    [int]$WarmupSecs = -1,
    [int]$MeasureSecs = -1
)

$ErrorActionPreference = 'Stop'
if (-not $Db) { throw 'Usage: run_db.ps1 -Db <redis|redis-ping|mongo|cassandra|neo4j>' }
function ConvertTo-IntList {
    param([string]$Text, [string]$Name)
    $items = @($Text -split '[,\s]+' | Where-Object { $_ -ne '' })
    $out = @()
    foreach ($i in $items) {
        $v = 0
        if (-not [int]::TryParse($i, [ref]$v) -or $v -lt 1) { throw "-$Name must be a comma-separated list of positive integers, got '$Text'" }
        $out += $v
    }
    if ($out.Count -eq 0) { throw "-$Name is empty" }
    return $out
}
$levelList = ConvertTo-IntList $Levels 'Levels'
$percentList = ConvertTo-IntList $OpenPercents 'OpenPercents'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
$bench = Join-Path $root 'target\release\bench.exe'

# Run a native command; throw on non-zero exit. Native stderr (progress output)
# must not be turned into terminating errors, so relax the preference locally.
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

function Wait-Healthy {
    param([string]$Container, [int]$TimeoutSecs = 600)
    $deadline = (Get-Date).AddSeconds($TimeoutSecs)
    while ($true) {
        $prev = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        $status = docker inspect --format '{{.State.Health.Status}}' $Container 2>$null
        $code = $LASTEXITCODE
        $ErrorActionPreference = $prev
        if ($code -ne 0) { throw "docker inspect $Container failed with exit code $code" }
        if ($status -eq 'healthy') { Write-Host "$Container is healthy"; return }
        if ((Get-Date) -gt $deadline) { throw "$Container not healthy after $TimeoutSecs s (status: $status)" }
        Start-Sleep -Seconds 2
    }
}

function Get-AllThroughput {
    param([string]$Dir)
    $row = Import-Csv (Join-Path $Dir 'summary.csv') | Where-Object { $_.op -eq 'ALL' }
    return [double]$row.throughput_ops_s
}

function Get-Median {
    param([double[]]$Values)
    $s = $Values | Sort-Object
    $n = $s.Count
    if ($n % 2 -eq 1) { return $s[[int][math]::Floor($n / 2)] }
    return ($s[$n / 2 - 1] + $s[$n / 2]) / 2
}

$composeProfile = if ($Db -eq 'redis-ping') { 'redis' } else { $Db }
$container = "nosqlbench-$composeProfile"
$extraArgs = @('--set', "workload.scenario=`"$Scenario`"")
if ($Force) { $extraArgs += @('--force') }
if ($WarmupSecs -ge 0) { $extraArgs += @('--warmup-secs', "$WarmupSecs") }
if ($MeasureSecs -ge 1) { $extraArgs += @('--measure-secs', "$MeasureSecs") }

Invoke-Native 'powershell.exe' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $PSScriptRoot 'sysinfo.ps1'), '-RequireAC')
if (-not (Test-Path $bench)) { Invoke-Native 'cargo' @('build', '--release') }

# Refuse to start if another process owns one of the profile's host ports:
# the benchmark would silently talk to that process instead of the container.
$ports = @{ redis = @(6379); mongo = @(27018); cassandra = @(9042); neo4j = @(7687, 7474) }
foreach ($port in $ports[$composeProfile]) {
    foreach ($c in @(Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue)) {
        $owner = (Get-Process -Id $c.OwningProcess -ErrorAction SilentlyContinue).ProcessName
        if ($owner -notmatch '^(com\.docker\.backend|wslrelay|vpnkit|docker)') {
            throw "Host port $port is already in use by $owner (pid $($c.OwningProcess)); stop it before running $Db."
        }
    }
}

Invoke-Native 'docker' @('compose', '--profile', $composeProfile, 'up', '-d')
try {
    Wait-Healthy $container
    if (Test-Path (Join-Path $root '.vm_affinity.json')) {
        Write-Warning '.vm_affinity.json exists: a VM affinity may still be applied. The default is no affinity; run scripts\set_vm_affinity.ps1 -Reset (elevated) to clear it.'
    }

    if ($Db -ne 'redis-ping') {
        Invoke-Native $bench @('load', '--db', $Db, '--set', "workload.scenario=`"$Scenario`"")
    }

    # Closed sweep.
    $perLevel = @{}
    foreach ($c in $levelList) {
        $tp = @()
        for ($r = 1; $r -le $Runs; $r++) {
            Invoke-Native $bench (@('run', '--db', $Db, '--mode', 'closed', '--concurrency', "$c", '--run', "$r", '--out', $Out) + $extraArgs)
            $tp += Get-AllThroughput (Join-Path $Out "$Scenario\$Db\closed_${c}_run$r")
        }
        $perLevel[$c] = Get-Median $tp
        Write-Host ("closed c={0}: median ALL throughput {1:N1} ops/s" -f $c, $perLevel[$c])
    }

    # Open sweep at percentages of the best closed level (median of its runs).
    if ($Db -ne 'redis-ping' -and -not $NoOpen) {
        $best = ($perLevel.Values | Measure-Object -Maximum).Maximum
        Write-Host ("best closed ALL throughput (median per level): {0:N1} ops/s" -f $best)
        foreach ($pct in $percentList) {
            $rate = [int64][math]::Floor($best * $pct / 100)
            if ($rate -lt 1) { throw "open-mode rate for $pct% is < 1 ops/s" }
            for ($r = 1; $r -le $Runs; $r++) {
                Invoke-Native $bench (@('run', '--db', $Db, '--mode', 'open', '--rate', "$rate", '--level', "${pct}pct", '--run', "$r", '--out', $Out) + $extraArgs)
            }
        }
    }
} finally {
    if (-not $KeepUp) {
        Invoke-Native 'docker' @('compose', '--profile', $composeProfile, 'down', '-v')
    }
}
Write-Host "Sweep for $Db ($Scenario) complete."

