<#
.SYNOPSIS
  Print host facts relevant to benchmarking; optionally abort when on battery.
.PARAMETER RequireAC
  Exit with code 1 if any battery is not on AC power (Win32_Battery.BatteryStatus -ne 2).
#>
param([switch]$RequireAC)

$ErrorActionPreference = 'Stop'

$cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()
$os = Get-CimInstance Win32_OperatingSystem
$ubr = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').UBR
$batteries = @(Get-CimInstance Win32_Battery)
$onBattery = @($batteries | Where-Object { $_.BatteryStatus -ne 2 }).Count -gt 0
if ($batteries.Count -eq 0) { $power = 'AC (no battery)' } elseif ($onBattery) { $power = 'battery' } else { $power = 'AC' }

$scheme = (powercfg /getactivescheme) -join ' '
if ($LASTEXITCODE -ne 0) { throw "powercfg failed with exit code $LASTEXITCODE" }

$prev = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
$docker = docker version --format '{{.Server.Version}}' 2>$null
$dockerExit = $LASTEXITCODE
$ErrorActionPreference = $prev
if ($dockerExit -ne 0) { $docker = '(docker not reachable)' }

$vm = @(Get-Process -Name vmmem, vmmemWSL -ErrorAction SilentlyContinue | ForEach-Object { "$($_.Name) pid $($_.Id)" }) -join ', '

[ordered]@{
    'CPU'            = $cpu
    'Windows'        = "$($os.Caption) $($os.BuildNumber).$ubr"
    'Power source'   = $power
    'Power scheme'   = $scheme
    'Docker server'  = $docker
    'Docker VM proc' = $vm
} | Format-Table -HideTableHeaders -AutoSize | Out-String | Write-Host

# Busy background processes (e.g. Windows Update downloads, Disk Cleanup)
# depress results; list anything above 20% of one core over 3 s.
$before = @{}
Get-Process | ForEach-Object { if ($_.CPU) { $before[$_.Id] = $_.CPU } }
Start-Sleep -Seconds 3
$busy = @(Get-Process | Where-Object { $_.CPU -and $before.ContainsKey($_.Id) } | ForEach-Object {
        [pscustomobject]@{ Name = $_.ProcessName; Id = $_.Id; CpuPct = [math]::Round(($_.CPU - $before[$_.Id]) / 3 * 100, 0) }
    } | Where-Object { $_.CpuPct -ge 20 } | Sort-Object CpuPct -Descending)
if ($busy.Count -gt 0) {
    Write-Warning ('Busy background processes (% of one core): ' + (($busy | ForEach-Object { "$($_.Name)[$($_.Id)] $($_.CpuPct)%" }) -join ', '))
}

if ($RequireAC -and $onBattery) {
    Write-Error 'Running on battery power: benchmark aborted. Plug in the charger.'
    exit 1
}
exit 0
