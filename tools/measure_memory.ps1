param(
    [string]$ProcessName = 'verify_playback',
    [string]$OutputPath = 'target/verification/memory.csv',
    [int]$IntervalSeconds = 60
)
$ErrorActionPreference = 'Stop'
$resolvedOutputPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
$outputDirectory = Split-Path -Path $resolvedOutputPath -Parent
New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null
$startedAt = Get-Date
$deadline = $startedAt.AddSeconds(60)
do {
    $playerProcess = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $playerProcess) { Start-Sleep -Milliseconds 200 }
} while (-not $playerProcess -and (Get-Date) -lt $deadline)
if (-not $playerProcess) { throw "Process $ProcessName was not found" }
$playerId = $playerProcess.Id
while ($playerProcess = Get-Process -Id $playerId -ErrorAction SilentlyContinue) {
    $children = Get-CimInstance Win32_Process -Filter "ParentProcessId = $playerId"
    $childPrivateBytes = 0L
    foreach ($child in $children) {
        $childProcess = Get-Process -Id $child.ProcessId -ErrorAction SilentlyContinue
        if ($childProcess) { $childPrivateBytes += $childProcess.PrivateMemorySize64 }
    }
    $row = [pscustomobject]@{
        ElapsedSeconds = [math]::Round(((Get-Date) - $startedAt).TotalSeconds, 1)
        PlayerId = $playerId
        PlayerPrivateBytes = $playerProcess.PrivateMemorySize64
        PlayerWorkingSetBytes = $playerProcess.WorkingSet64
        DecoderPrivateBytes = $childPrivateBytes
    }
    $row | Export-Csv -LiteralPath $resolvedOutputPath -NoTypeInformation -Append -Encoding utf8
    $row | ConvertTo-Json -Compress
    Start-Sleep -Seconds $IntervalSeconds
}
