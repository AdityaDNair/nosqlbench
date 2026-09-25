<#
.SYNOPSIS
  Produce a portable dump of the loaded `normal` dataset for each database:
  up -> healthy -> bench load -> export -> down -v, one database at a time.

  Output (in dumps\):
    redis.rdb                  redis-cli --rdb (RDB snapshot via replication)
    mongo.archive.gz           mongodump --archive --gzip of social_bench
    cassandra_cql.tar.gz       schema.cql + one CSV per table (cqlsh COPY TO)
    neo4j.dump                 neo4j-admin database dump neo4j (db offline)
    manifest.json              sizes, SHA-256, image digests, git commit, seed

  See dumps\README.md for how to restore each file.

.PARAMETER Db
  Comma-separated subset of redis,mongo,cassandra,neo4j (default: all four).
.PARAMETER Force
  Overwrite existing dump files.
#>
# No [Parameter()] attributes: they would make -Debug (alias -db) collide with -Db.
param(
    [string]$Db = 'redis,mongo,cassandra,neo4j',
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
$bench = Join-Path $root 'target\release\bench.exe'
$dumps = Join-Path $root 'dumps'
$scenario = 'normal'

$dbList = @($Db -split '[,\s]+' | Where-Object { $_ -ne '' })
foreach ($d in $dbList) {
    if ($d -notin @('redis', 'mongo', 'cassandra', 'neo4j')) { throw "Unknown database '$d' in -Db" }
}
$files = @{
    redis     = 'redis.rdb'
    mongo     = 'mongo.archive.gz'
    cassandra = 'cassandra_cql.tar.gz'
    neo4j     = 'neo4j.dump'
}

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

# Same as Invoke-Native but returns stdout as one string.
function Get-NativeOutput {
    param([string]$Exe, [string[]]$Arguments)
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $out = & $Exe @Arguments 2>$null
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prev
    if ($code -ne 0) { throw "$Exe $($Arguments -join ' ') failed with exit code $code" }
    return (@($out) -join "`n").Trim()
}

function Wait-Healthy {
    param([string]$Container, [int]$TimeoutSecs = 600)
    $deadline = (Get-Date).AddSeconds($TimeoutSecs)
    while ($true) {
        $status = Get-NativeOutput 'docker' @('inspect', '--format', '{{.State.Health.Status}}', $Container)
        if ($status -eq 'healthy') { Write-Host "$Container is healthy"; return }
        if ((Get-Date) -gt $deadline) { throw "$Container not healthy after $TimeoutSecs s (status: $status)" }
        Start-Sleep -Seconds 2
    }
}

function Assert-PortsFree {
    param([string]$ProfileName)
    $ports = @{ redis = @(6379); mongo = @(27018); cassandra = @(9042); neo4j = @(7687, 7474) }
    foreach ($port in $ports[$ProfileName]) {
        foreach ($c in @(Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue)) {
            $owner = (Get-Process -Id $c.OwningProcess -ErrorAction SilentlyContinue).ProcessName
            if ($owner -notmatch '^(com\.docker\.backend|wslrelay|vpnkit|docker)') {
                throw "Host port $port is already in use by $owner (pid $($c.OwningProcess)); stop it first."
            }
        }
    }
}

function Export-Redis {
    param([string]$Container, [string]$Dest)
    Invoke-Native 'docker' @('exec', $Container, 'redis-cli', '--rdb', '/tmp/dump.rdb')
    Invoke-Native 'docker' @('cp', "${Container}:/tmp/dump.rdb", $Dest)
}

function Export-Mongo {
    param([string]$Container, [string]$Dest)
    Invoke-Native 'docker' @('exec', $Container, 'mongodump', '--db', 'social_bench', '--archive=/tmp/mongo.archive.gz', '--gzip')
    Invoke-Native 'docker' @('cp', "${Container}:/tmp/mongo.archive.gz", $Dest)
}

function Export-Cassandra {
    param([string]$Container, [string]$Dest)
    # Flush memtables so nothing is lost if anyone inspects the node afterwards;
    # COPY TO reads through CQL, so it sees memtable data either way.
    Invoke-Native 'docker' @('exec', $Container, 'nodetool', 'flush', 'social_bench')
    # No double quotes inside native arguments: Windows PowerShell 5.1 strips them.
    Invoke-Native 'docker' @('exec', $Container, 'sh', '-c', "rm -rf /tmp/cassdump && mkdir -p /tmp/cassdump && cqlsh -e 'DESCRIBE KEYSPACE social_bench' > /tmp/cassdump/schema.cql")
    $tables = Get-NativeOutput 'docker' @('exec', $Container, 'cqlsh', '-e', "SELECT table_name FROM system_schema.tables WHERE keyspace_name = 'social_bench'")
    $names = @($tables -split "`n" | ForEach-Object { $_.Trim() } | Where-Object { $_ -match '^[a-z_]+$' -and $_ -ne 'table_name' })
    if ($names.Count -eq 0) { throw 'No tables found in social_bench' }
    foreach ($t in $names) {
        Invoke-Native 'docker' @('exec', $Container, 'cqlsh', '--request-timeout=600', '-e', "COPY social_bench.$t TO '/tmp/cassdump/$t.csv' WITH HEADER = true")
    }
    Invoke-Native 'docker' @('exec', $Container, 'tar', '-C', '/tmp/cassdump', '-czf', '/tmp/cassandra_cql.tar.gz', '.')
    Invoke-Native 'docker' @('cp', "${Container}:/tmp/cassandra_cql.tar.gz", $Dest)
}

function Export-Neo4j {
    param([string]$Container, [string]$Dest)
    # neo4j-admin dump needs the database offline, and Community cannot
    # STOP DATABASE, so stop the container and dump its volume from a
    # one-off container of the same image.
    $mounts = Get-NativeOutput 'docker' @('inspect', '--format', '{{json .Mounts}}', $Container) | ConvertFrom-Json
    $volume = @($mounts | Where-Object { $_.Destination -eq '/data' })[0].Name
    $image = Get-NativeOutput 'docker' @('inspect', '--format', '{{.Config.Image}}', $Container)
    if (-not $volume) { throw "Could not find the /data volume of $Container" }
    Invoke-Native 'docker' @('compose', '--profile', 'neo4j', 'stop')
    $helper = 'nosqlbench-neo4j-dump'
    Invoke-Native 'docker' @('run', '--name', $helper, '-v', "${volume}:/data", $image,
        'neo4j-admin', 'database', 'dump', 'neo4j', '--to-path=/tmp', '--overwrite-destination=true')
    try {
        Invoke-Native 'docker' @('cp', "${helper}:/tmp/neo4j.dump", $Dest)
    } finally {
        Invoke-Native 'docker' @('rm', '-v', $helper)
    }
}

# Refuse to clobber existing dumps unless -Force.
New-Item -ItemType Directory -Force $dumps | Out-Null
foreach ($d in $dbList) {
    $p = Join-Path $dumps $files[$d]
    if ((Test-Path $p) -and -not $Force) { throw "$p exists; pass -Force to overwrite" }
}
if (-not (Test-Path $bench)) { Invoke-Native 'cargo' @('build', '--release') }

$manifestPath = Join-Path $dumps 'manifest.json'
$manifest = [ordered]@{}
if (Test-Path $manifestPath) {
    $old = Get-Content $manifestPath -Raw | ConvertFrom-Json
    foreach ($prop in $old.PSObject.Properties) { $manifest[$prop.Name] = $prop.Value }
}
$entries = [ordered]@{}
if ($manifest.Contains('files')) {
    foreach ($prop in $manifest['files'].PSObject.Properties) { $entries[$prop.Name] = $prop.Value }
}

$commit = Get-NativeOutput 'git' @('rev-parse', 'HEAD')
$dirty = (Get-NativeOutput 'git' @('status', '--porcelain', '--', 'src', 'config', 'Cargo.toml', 'Cargo.lock', 'docker-compose.yml')) -ne ''
$seed = (Select-String -Path (Join-Path $root 'config\default.toml') -Pattern '^\s*seed\s*=\s*(\d+)').Matches[0].Groups[1].Value

foreach ($d in $dbList) {
    $container = "nosqlbench-$d"
    $dest = Join-Path $dumps $files[$d]
    Write-Host "=== $d ==="
    Assert-PortsFree $d
    Invoke-Native 'docker' @('compose', '--profile', $d, 'up', '-d')
    try {
        Wait-Healthy $container
        $imageId = Get-NativeOutput 'docker' @('inspect', '--format', '{{.Image}}', $container)
        $imageTag = Get-NativeOutput 'docker' @('inspect', '--format', '{{.Config.Image}}', $container)
        $loadStart = Get-Date
        Invoke-Native $bench @('load', '--db', $d, '--set', "workload.scenario=`"$scenario`"")
        $loadSecs = ((Get-Date) - $loadStart).TotalSeconds
        $exportStart = Get-Date
        if (Test-Path $dest) { Remove-Item $dest -Force }
        switch ($d) {
            'redis' { Export-Redis $container $dest }
            'mongo' { Export-Mongo $container $dest }
            'cassandra' { Export-Cassandra $container $dest }
            'neo4j' { Export-Neo4j $container $dest }
        }
        $exportSecs = ((Get-Date) - $exportStart).TotalSeconds
        $item = Get-Item $dest
        $entries[$files[$d]] = [ordered]@{
            db          = $d
            bytes       = $item.Length
            sha256      = (Get-FileHash $dest -Algorithm SHA256).Hash.ToLower()
            image       = $imageTag
            image_id    = $imageId
            load_secs   = [math]::Round($loadSecs, 1)
            export_secs = [math]::Round($exportSecs, 1)
            created     = (Get-Date).ToUniversalTime().ToString('o')
        }
        Write-Host ("{0}: {1:N1} MB" -f $files[$d], ($item.Length / 1MB))
    } finally {
        Invoke-Native 'docker' @('compose', '--profile', $d, 'down', '-v')
    }
    $manifest['scenario'] = $scenario
    $manifest['seed'] = [int64]$seed
    $manifest['git_commit'] = $commit
    $manifest['git_dirty'] = $dirty
    $manifest['files'] = $entries
    $manifest | ConvertTo-Json -Depth 5 | Out-File -Encoding utf8 $manifestPath
}

Write-Host 'Dumps:'
foreach ($k in $entries.Keys) { Write-Host ("  {0,-24} {1,10:N1} MB" -f $k, ($entries[$k].bytes / 1MB)) }
