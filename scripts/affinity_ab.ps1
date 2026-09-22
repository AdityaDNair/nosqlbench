<#
.SYNOPSIS
  Same-boot A/B/C test of the Docker VM CPU affinity using redis-ping
  (closed, c=32). Must run in an Administrator PowerShell.

.DESCRIPTION
  Arms, interleaved per round (A B C, A B C, ...) so drift affects all equally:
    none : VM on all logical CPUs (set_vm_affinity.ps1 -Reset)
    fc03 : 0xFC03 = CPUs 0,1,10,11,12-15 (P-core 0, P-core 5, E-cores)
    0c03 : 0x0C03 = CPUs 0,1,10,11 (P-cores 0 and 5 only, no E-cores)
  Workers stay on CPUs 2,4,6,8. For each run, per-CPU utilisation is sampled
  and written to cpu.csv next to the run's results.
  Results: results\vm_affinity\ab\<arm>\celebrity\redis-ping\closed_32_run<round>\
  At the end the affinity is set back to -FinalMask (default 0xFC03).
#>
param(
    [int]$Rounds = 3,
    [string]$FinalMask = '0xFC03'
)

$ErrorActionPreference = 'Stop'
$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'affinity_ab.ps1 must run in an Administrator PowerShell.'
}
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
$bench = Join-Path $root 'target\release\bench.exe'
$setter = Join-Path $PSScriptRoot 'set_vm_affinity.ps1'

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

$prev = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
$health = docker inspect --format '{{.State.Health.Status}}' nosqlbench-redis 2>$null
$ErrorActionPreference = $prev
if ($health -ne 'healthy') { throw 'Start Redis first: docker compose --profile redis up -d' }

$arms = @(
    @{ name = 'none'; mask = $null },
    @{ name = 'fc03'; mask = '0xFC03' },
    @{ name = '0c03'; mask = '0x0C03' }
)

try {
    for ($round = 1; $round -le $Rounds; $round++) {
        foreach ($arm in $arms) {
            if ($arm.mask) { & $setter -Mask $arm.mask } else { & $setter -Reset }
            $out = "results\vm_affinity\ab\$($arm.name)"
            $benchArgs = @('run', '--db', 'redis-ping', '--concurrency', '32', '--run', "$round",
                '--set', 'workload.scenario="celebrity"', '--out', $out, '--force')
            if ($arm.mask) { $benchArgs += '--require-vm-affinity' }

            # Sample per-CPU load during the measure phase (after warm-up).
            $job = Start-Job {
                $paths = @(0..([Environment]::ProcessorCount - 1) | ForEach-Object { "\Processor($_)\% Processor Time" })
                Start-Sleep 20
                Get-Counter -Counter $paths -SampleInterval 1 -MaxSamples 40 |
                    ForEach-Object { $_.CounterSamples } | Group-Object InstanceName |
                    ForEach-Object { [pscustomobject]@{ cpu = [int]$_.Name; avg_pct = [math]::Round(($_.Group | Measure-Object CookedValue -Average).Average, 1) } } |
                    Sort-Object cpu
            }
            Invoke-Native $bench $benchArgs
            $cpu = Receive-Job $job -Wait
            Remove-Job $job
            $cpu | Export-Csv -NoTypeInformation -Path (Join-Path $out "celebrity\redis-ping\closed_32_run$round\cpu.csv")
        }
    }
} finally {
    & $setter -Mask $FinalMask
}

# Summary.
Write-Host ''
Write-Host 'arm   run   ops/s      p50_us    p99_us    E-core avg %'
foreach ($arm in $arms) {
    for ($round = 1; $round -le $Rounds; $round++) {
        $dir = "results\vm_affinity\ab\$($arm.name)\celebrity\redis-ping\closed_32_run$round"
        $row = Import-Csv "$dir\summary.csv" | Where-Object { $_.op -eq 'ALL' }
        $e = (Import-Csv "$dir\cpu.csv" | Where-Object { [int]$_.cpu -ge 12 } | Measure-Object avg_pct -Average).Average
        Write-Host ('{0,-5} {1,-5} {2,-10:N0} {3,-9} {4,-9} {5:N1}' -f $arm.name, $round, [double]$row.throughput_ops_s, $row.p50_us, $row.p99_us, $e)
    }
}

