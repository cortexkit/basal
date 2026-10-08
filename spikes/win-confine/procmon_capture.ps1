$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force evidence | Out-Null
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$admin = ([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { throw 'Process Monitor capture requires an administrator runner' }
$zip = Join-Path $PWD 'evidence/ProcessMonitor.zip'
Invoke-WebRequest https://download.sysinternals.com/files/ProcessMonitor.zip -OutFile $zip
Expand-Archive $zip evidence/procmon -Force
$procmon = (Resolve-Path evidence/procmon/Procmon64.exe).Path
$pml = Join-Path $PWD 'evidence/startup.pml'
$csv = Join-Path $PWD 'evidence/startup.csv'
$metadata = [ordered]@{
    administrator = $admin
    download_url = 'https://download.sysinternals.com/files/ProcessMonitor.zip'
    zip_sha256 = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    executable_sha256 = (Get-FileHash $procmon -Algorithm SHA256).Hash.ToLower()
    version = (Get-Item $procmon).VersionInfo.FileVersion
    capture_tool = 'Process Monitor'
}
$etl = Join-Path $PWD 'evidence/startup.etl'
$etw = $false
try {
    Start-Process $procmon -ArgumentList @('/AcceptEula', '/Quiet', '/Minimized', '/BackingFile', $pml)
    & $procmon /AcceptEula /WaitForIdle
    $metadata['wait_for_idle_exit'] = $LASTEXITCODE
    if ($LASTEXITCODE -ne 0) { throw "Process Monitor WaitForIdle: $LASTEXITCODE" }
    # The returned idle signal means the driver is ready before any worker birth.
    & target/release/win-confine.exe --output evidence/report.json
    $measurementExit = $LASTEXITCODE
} catch {
    $metadata['capture_error'] = $_.ToString()
    $metadata['capture_tool'] = 'WPR GeneralProfile fallback'
    & $procmon /Terminate
    & wpr -start GeneralProfile -filemode
    if ($LASTEXITCODE -ne 0) { throw "WPR start failed: $LASTEXITCODE" }
    $etw = $true
    & target/release/win-confine.exe --output evidence/report.json
    $measurementExit = $LASTEXITCODE
} finally {
    if ($etw) {
        & wpr -stop $etl
        $metadata['wpr_stop_exit'] = $LASTEXITCODE
    } else {
        & $procmon /Terminate
        $metadata['terminate_exit'] = $LASTEXITCODE
    }
    $metadata | ConvertTo-Json -Depth 5 | Out-File -Encoding utf8 evidence/procmon-metadata.json
}
if (Test-Path $pml) {
    & $procmon /AcceptEula /Quiet /OpenLog $pml /SaveAs $csv
    if ($LASTEXITCODE -ne 0) { throw "Process Monitor CSV export failed: $LASTEXITCODE" }
    python procmon_summary.py evidence/report.json $csv evidence/procmon-events.json evidence/procmon-events.md
    if ($LASTEXITCODE -ne 0) {
        # No early worker events are not a denial result. Keep a separate ETW run.
        & wpr -start GeneralProfile -filemode
        if ($LASTEXITCODE -ne 0) { throw "WPR start failed: $LASTEXITCODE" }
        & target/release/win-confine.exe --output evidence/etw-report.json
        & wpr -stop $etl
        $metadata['early_activity_fallback'] = 'WPR GeneralProfile; separate etw-report.json PID mapping'
        $metadata | ConvertTo-Json -Depth 5 | Out-File -Encoding utf8 evidence/procmon-metadata.json
        throw 'Process Monitor did not capture all requested early windows; ETW retained, no denial inferred'
    }
    # Keep raw child rows and the lossless failures, not the runner-wide CSV with
    # unrelated processes. The binary PML is retained for stack/object inspection.
    Remove-Item $csv
}
exit $measurementExit
