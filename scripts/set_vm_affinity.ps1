<#
.SYNOPSIS
  Pin the Docker Desktop VM process (vmmem / vmmemWSL) to a set of logical CPUs
  so it never shares a physical core with the benchmark workers.

.DESCRIPTION
  Must run in an Administrator PowerShell (Windows denies non-elevated access
  to the VM process). Sets ProcessorAffinity, reads it back, and writes
  .vm_affinity.json in the repo root with the mask and VM PIDs. run_db.ps1 and
  bench read that file; a restarted VM (new PID) invalidates it.

  The affinity lasts only as long as the VM process. Docker Desktop's Resource
  Saver stops the VM when no containers have run for a while, and the next
  container start creates a new vmmemWSL process without the affinity; disable
  Resource Saver (Settings > Resources) or run run_db.ps1 elevated, which
  re-applies the affinity after every container start.

  Default mask 0xFC03 = logical CPUs 0,1,10,11,12,13,14,15: P-core 0, P-core 5
  and the four E-cores. Workers run on CPUs 2,4,6,8 (P-cores 1-4).

.PARAMETER Mask
  Hex affinity mask. Default 0xFC03.
.PARAMETER Reset
  Restore the VM process to all logical CPUs and delete .vm_affinity.json.
#>
param(
    [string]$Mask = '0xFC03',
    [switch]$Reset
)

$ErrorActionPreference = 'Stop'

$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'set_vm_affinity.ps1 must run in an Administrator PowerShell (setting the VM process affinity is denied otherwise).'
}

$root = Split-Path $PSScriptRoot -Parent
$stateFile = Join-Path $root '.vm_affinity.json'

$procs = @(Get-Process -Name vmmem, vmmemWSL -ErrorAction SilentlyContinue)
if ($procs.Count -eq 0) {
    throw 'No vmmem / vmmemWSL process found. Is Docker Desktop running?'
}

$nLogical = [Environment]::ProcessorCount
if ($Reset) {
    $value = [int64]([math]::Pow(2, $nLogical) - 1)
} else {
    $value = [Convert]::ToInt64(($Mask -replace '^0x', ''), 16)
}
$hex = '0x{0:X}' -f $value

$records = @()
foreach ($p in $procs) {
    try {
        $p.ProcessorAffinity = [IntPtr]$value
    } catch {
        throw "Setting affinity $hex on $($p.Name) (pid $($p.Id)) failed: $($_.Exception.Message)"
    }
    $p.Refresh()
    $readBack = [int64]$p.ProcessorAffinity
    if ($readBack -ne $value) {
        throw ("Affinity read-back mismatch on {0} (pid {1}): wanted {2}, got 0x{3:X}" -f $p.Name, $p.Id, $hex, $readBack)
    }
    Write-Host ("{0} pid {1}: affinity set to {2} (read back 0x{3:X})" -f $p.Name, $p.Id, $hex, $readBack)
    $records += [ordered]@{ name = $p.Name; pid = $p.Id; readback = ('0x{0:X}' -f $readBack) }
}

if ($Reset) {
    if (Test-Path $stateFile) { Remove-Item $stateFile }
    Write-Host 'Reset to all logical CPUs; removed .vm_affinity.json'
} else {
    $state = [ordered]@{
        mask      = $hex
        set_at    = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
        processes = $records
    }
    $state | ConvertTo-Json -Depth 4 | Set-Content -Path $stateFile -Encoding UTF8
    Write-Host "Wrote $stateFile"
}
