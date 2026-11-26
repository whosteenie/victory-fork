[CmdletBinding()]
param(
    [string] $Test = 'Hash',
    [ValidateRange(2,100)]
    [int] $Rounds = 10,
    [switch] $Warp,
    [switch] $List,
    [string] $CompareTo,
    [string] $Output = (Join-Path ([IO.Path]::GetTempPath()) ("victory-replay-" + (Get-Date -Format 'yyyyMMdd-HHmmss-fff') + ".json"))
)

$ErrorActionPreference = 'Stop'
$workloads = Import-PowerShellDataFile -Path (Join-Path $PSScriptRoot 'Replay-Workloads.psd1')
if ($List) {
    $workloads.Keys | Sort-Object | ForEach-Object {
        [pscustomobject]@{ Name = $_; Backend = $workloads[$_].Backend; Description = $workloads[$_].Description }
    }
    return
}
if (!$workloads.ContainsKey($Test)) { throw "Unknown workload '$Test'. Use -List to see available workloads." }
$workload = $workloads[$Test]
if ($Warp -and $workload.Backend -ne 'selectable') { throw 'This workload already specifies its backend.' }
$backend = if ($Warp) { 'warp' } elseif ($workload.Backend -eq 'selectable') { 'hardware' } else { $workload.Backend }
if ($workload.SampleKind -and $backend -eq 'hardware' -and (Get-Process -Name 'victory','victory-diag-*' -ErrorAction SilentlyContinue)) {
    throw 'Close the VIctory game session to measure on an idle GPU.'
}
$root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$gitRoot = 'safe.directory=' + $root.Replace('\', '/')
$outputPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Output)
$reference = $null
if ($CompareTo) {
    $referencePath = (Resolve-Path -LiteralPath $CompareTo).Path
    if ($referencePath -eq $outputPath) { throw 'Use a different output file for the new result.' }
    $reference = Get-Content -LiteralPath $referencePath -Raw -Encoding UTF8 | ConvertFrom-Json
}
$package = $workload.Package
$targetName = $package.Replace('-', '_')
$instrumentation = 'VICTORY_RENDERDOC','VICTORY_D3D12_DRED','VICTORY_D3D12_GBV'
$envNames = @('VICTORY_D3D12_WARP','VICTORY_D3D12_DEBUG','VICTORY_REPLAY_ROUNDS') + $instrumentation
$saved = @{}
foreach ($name in $envNames) { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }

function Get-Statistics([double[]] $Values) {
    $sorted = @($Values | Sort-Object)
    $n = $sorted.Count
    $middle = [int][Math]::Floor($n / 2)
    $median = if ($n % 2) { $sorted[$middle] } else { ($sorted[$middle-1] + $sorted[$middle]) / 2 }
    $mean = ($sorted | Measure-Object -Average).Average
    $sumSquared = 0.0
    foreach ($v in $sorted) { $sumSquared += [Math]::Pow($v - $mean, 2) }
    [ordered]@{
        mean = [Math]::Round($mean, 2)
        median = [Math]::Round($median, 2)
        p95 = $sorted[[int][Math]::Ceiling(.95 * $n) - 1]
        min = $sorted[0]
        max = $sorted[-1]
        standard_deviation = [Math]::Round([Math]::Sqrt($sumSquared / $n), 2)
    }
}

function Write-Result {
    $report['total_seconds'] = [Math]::Round($whole.Elapsed.TotalSeconds, 3)
    $report | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $outputPath -Encoding UTF8
    Write-Host "Replay result saved to $outputPath"
}

Push-Location -LiteralPath $root
try {
    foreach ($name in $instrumentation) { [Environment]::SetEnvironmentVariable($name, $null, 'Process') }
    $env:VICTORY_D3D12_WARP = if ($backend -eq 'warp') { '1' } else { $null }
    $env:VICTORY_D3D12_DEBUG = if ($workload.Debug) { '1' } else { $null }
    $env:VICTORY_REPLAY_ROUNDS = [string]$Rounds
    $whole = [Diagnostics.Stopwatch]::StartNew()
    $report = [ordered]@{
        schema = 2
        workload = $Test
        filter = $workload.Filter
        outcome = 'build_failed'
        commit = (git -c $gitRoot rev-parse HEAD)
        worktree_dirty = [bool](git -c $gitRoot status --porcelain)
        backend = $backend
        rounds = if ($workload.SampleKind) { $Rounds } else { $null }
    }
    $build = [Diagnostics.Stopwatch]::StartNew()
    $artifacts = & cargo test --locked --profile diag -p $package --features diagnostics --lib --no-run --message-format=json
    $build.Stop()
    $report['build_seconds'] = [Math]::Round($build.Elapsed.TotalSeconds, 3)
    if ($LASTEXITCODE -ne 0) {
        $report['errors'] = @($artifacts | ForEach-Object {
            $row = ConvertFrom-Json -InputObject $_
            if ($row.reason -eq 'compiler-message') { $row.message.rendered }
        })
        $report.errors | ForEach-Object { Write-Host $_ }
        Write-Result
        throw 'The replay test failed to build.'
    }
    $exe = $null
    foreach ($line in $artifacts) {
        $row = ConvertFrom-Json -InputObject $line
        if ($row.reason -eq 'compiler-artifact' -and $row.executable -and $row.target.name -eq $targetName) {
            $exe = $row.executable
        }
    }
    if (!$exe) { Write-Result; throw 'Cargo did not report the test executable.' }
    $argsForTest = @($workload.Filter, '--exact', '--nocapture', '--test-threads=1')
    if ($workload.Ignored) { $argsForTest += '--ignored' }
    $report['outcome'] = 'test_failed'
    $run = [Diagnostics.Stopwatch]::StartNew()
    $preference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $lines = & $exe @argsForTest 2>&1 | ForEach-Object { $_.ToString() }
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $preference
    }
    $run.Stop()
    $report['run_seconds'] = [Math]::Round($run.Elapsed.TotalSeconds, 3)
    $report['binary_sha256'] = (Get-FileHash -LiteralPath $exe).Hash
    if ($exitCode -ne 0 -or !($lines -match '1 passed; 0 failed')) { throw 'The requested test did not pass.' }
    $records = @(
        foreach ($line in $lines) {
            $at = $line.IndexOf('{"kind":')
            if ($at -ge 0) { ConvertFrom-Json -InputObject $line.Substring($at) }
        }
    )
    $report['records'] = $records
    if ($workload.RequiredCheck -and !($records | Where-Object check -eq $workload.RequiredCheck)) {
        throw 'The test returned without completing its required correctness checks.'
    }
    $adapter = $null
    $adapterLine = $lines | Where-Object { $_ -match 'Replay adapter:' } | Select-Object -First 1
    if ($adapterLine) { $adapter = $adapterLine -replace '^.*Replay adapter: ', '' }
    $driver = $null
    if ($backend -eq 'warp') { $adapter = 'Microsoft WARP' }
    if ($adapter -and $backend -eq 'hardware') {
        $driver = Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue |
            Where-Object Name -eq $adapter | Select-Object -First 1 -ExpandProperty DriverVersion
    }
    $report['hardware'] = [ordered]@{
        os_version = [Environment]::OSVersion.Version.ToString()
        cpu = $env:PROCESSOR_IDENTIFIER
        adapter = $adapter
        driver_version = $driver
    }
    $report['configuration'] = @(
        $records | Where-Object { $_.kind -in $workload.ConfigurationKinds } | ForEach-Object {
            $_ | Select-Object * -ExcludeProperty initialization_us
        }
    )
    $metrics = [ordered]@{}
    foreach ($mode in $workload.Modes) {
        $samples = @($records | Where-Object { $_.kind -eq $workload.SampleKind -and !$_.warmup -and $_.mode -eq $mode })
        if ($samples.Count -ne $Rounds) { throw "Incomplete sample set for $mode." }
        $metrics[$mode] = [ordered]@{}
        foreach ($field in $workload.Metrics) {
            $values = @($samples | ForEach-Object { $_.$field })
            if ($values.Count -ne $Rounds -or $values -contains $null) { throw "Missing metric $field for $mode." }
            $metrics[$mode][$field] = Get-Statistics $values
        }
        foreach ($field in $workload.SumMetrics.Keys) {
            $terms = $workload.SumMetrics[$field]
            $values = foreach ($sample in $samples) {
                ($terms | ForEach-Object { $sample.$_ } | Measure-Object -Sum).Sum
            }
            $metrics[$mode][$field] = Get-Statistics @($values)
        }
    }
    $report['statistics'] = $metrics
    $report['outcome'] = 'passed'
    if ($reference) {
        $comparison = [ordered]@{ reference = $referencePath; comparable = $false }
        $report['comparison'] = $comparison
        $reason = if ($reference.schema -ne 2 -or $reference.outcome -ne 'passed' -or $reference.workload -ne $Test -or $reference.backend -ne $backend) {
            'The reference run must have passed with the same workload, schema and backend.'
        } elseif ($reference.rounds -ne $report.rounds) {
            'The measured round counts differ.'
        } elseif (($reference.configuration | ConvertTo-Json -Compress -Depth 8) -ne ($report.configuration | ConvertTo-Json -Compress -Depth 8)) {
            'The replay configurations differ.'
        } elseif (($reference.hardware | ConvertTo-Json -Compress) -ne ($report.hardware | ConvertTo-Json -Compress)) {
            'The hardware, OS or GPU driver metadata differs.'
        } elseif ($workload.SampleKind -and $backend -eq 'hardware' -and !$driver) {
            'The GPU driver version could not be established.'
        }
        if ($reason) {
            $comparison['reason'] = $reason
        } else {
            $comparison['comparable'] = $true
            $comparison['deltas'] = @(
                foreach ($mode in $metrics.Keys) {
                    foreach ($field in $metrics[$mode].Keys) {
                        $old = $reference.statistics.$mode.$field
                        if ($null -eq $old) { throw "Reference lacks $mode/$field." }
                        $new = $metrics[$mode][$field]
                        [ordered]@{
                            mode = $mode
                            metric = $field
                            reference_median = $old.median
                            current_median = $new.median
                            median_delta = [Math]::Round($new.median - $old.median, 2)
                            p95_delta = [Math]::Round($new.p95 - $old.p95, 2)
                        }
                    }
                }
            )
        }
    }
    Write-Result
}
catch {
    if ($report -and $report.outcome -eq 'test_failed') {
        $report['error'] = $_.Exception.Message
        $report['output'] = $lines
        $lines | ForEach-Object { Write-Host $_ }
        Write-Result
    }
    throw
}
finally {
    foreach ($name in $envNames) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
    Pop-Location
}
