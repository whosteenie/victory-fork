[CmdletBinding()]
param(
    [ValidateSet('Hash','Parity')]
    [string] $Test = 'Hash',
    [ValidateRange(2,100)]
    [int] $Rounds = 10,
    [switch] $Warp,
    [string] $Output = (Join-Path ([IO.Path]::GetTempPath()) ("victory-replay-" + (Get-Date -Format 'yyyyMMdd-HHmmss') + ".json"))
)

$ErrorActionPreference = 'Stop'
if ($Warp -and $Test -ne 'Parity') { throw 'Use WARP for parity checks. The timing test requires hardware.' }
if ($Test -eq 'Hash' -and (Get-Process -Name 'victory','victory-diag-*' -ErrorAction SilentlyContinue)) {
    throw 'Close the VIctory game session to measure on an idle GPU.'
}
$root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$outputPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Output)
$testName = if ($Test -eq 'Hash') { 'host::card_replay::replay_card_hashes' } else { 'host::card_replay::card_hash_parity' }
$envNames = 'VICTORY_D3D12_WARP','VICTORY_D3D12_DEBUG','VICTORY_REPLAY_ROUNDS'
$saved = @{}
foreach ($name in $envNames) { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }

function Stats([double[]] $values) {
    $sorted = @($values | Sort-Object)
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

Push-Location -LiteralPath $root
try {
    $env:VICTORY_D3D12_WARP = if ($Warp) { '1' } else { $null }
    $env:VICTORY_D3D12_DEBUG = if ($Test -eq 'Parity') { '1' } else { $null }
    $env:VICTORY_REPLAY_ROUNDS = [string]$Rounds
    $whole = [Diagnostics.Stopwatch]::StartNew()
    $build = [Diagnostics.Stopwatch]::StartNew()
    $artifacts = & cargo test --locked --profile diag -p victory-gpu --features diagnostics --lib --no-run --message-format=json
    $build.Stop()
    if ($LASTEXITCODE -ne 0) {
        $artifacts | ForEach-Object {
            $row = ConvertFrom-Json -InputObject $_
            if ($row.reason -eq 'compiler-message') { Write-Host $row.message.rendered }
        }
        throw 'The replay test failed to build.'
    }
    $exe = $null
    foreach ($line in $artifacts) {
        $row = ConvertFrom-Json -InputObject $line
        if ($row.reason -eq 'compiler-artifact' -and $row.executable -and $row.target.name -eq 'victory_gpu') {
            $exe = $row.executable
        }
    }
    if (!$exe) { throw 'Cargo did not report the GPU test executable.' }
    $run = [Diagnostics.Stopwatch]::StartNew()
    $lines = & $exe $testName --exact --ignored --nocapture --test-threads=1
    $run.Stop()
    $exitCode = $LASTEXITCODE
    $lines | ForEach-Object { Write-Host $_ }
    if ($exitCode -ne 0 -or !($lines -match '1 passed; 0 failed')) { throw 'The requested replay test did not pass.' }
    $records = @(
        foreach ($line in $lines) {
            $at = $line.IndexOf('{"kind":')
            if ($at -ge 0) { ConvertFrom-Json -InputObject $line.Substring($at) }
        }
    )
    $groups = @($records | Where-Object { $_.kind -eq 'sample' -and !$_.warmup } | Group-Object mode)
    $metrics = [ordered]@{}
    foreach ($g in $groups) {
        if ($g.Count -ne $Rounds) { throw "Incomplete sample set for $($g.Name)." }
        $metrics[$g.Name] = [ordered]@{
            upload_hash_us = Stats @($g.Group.upload_hash_us)
            compare_us = Stats @($g.Group.compare_us)
            gpu_total_us = Stats @($g.Group | ForEach-Object { $_.upload_hash_us + $_.compare_us })
            record_us = Stats @($g.Group.record_us)
            submit_us = Stats @($g.Group.submit_us)
            wait_us = Stats @($g.Group.wait_us)
        }
    }
    if ($Test -eq 'Hash' -and $groups.Count -ne 3) { throw 'Missing one or more hash variants.' }
    $whole.Stop()
    [ordered]@{
        schema = 1
        test = $Test
        warp = [bool]$Warp
        commit = (git -c ('safe.directory=' + $root.Replace('\', '/')) rev-parse HEAD)
        adapter = ($lines | Where-Object { $_ -match 'Replay adapter:' } | Select-Object -First 1) -replace '^.*Replay adapter: ', ''
        binary_sha256 = (Get-FileHash -LiteralPath $exe).Hash
        build_seconds = [Math]::Round($build.Elapsed.TotalSeconds, 3)
        run_seconds = [Math]::Round($run.Elapsed.TotalSeconds, 3)
        total_seconds = [Math]::Round($whole.Elapsed.TotalSeconds, 3)
        records = $records
        statistics = $metrics
    } | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $outputPath -Encoding UTF8
    Write-Host "Replay result saved to $outputPath"
}
finally {
    foreach ($name in $envNames) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
    Pop-Location
}

