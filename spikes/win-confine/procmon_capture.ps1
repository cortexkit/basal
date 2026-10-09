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
$objectTrace = $false
$session = "BasalStartup-$PID"
$objectEtl = Join-Path $PWD 'evidence/kernel-objects.etl'
# Procmon covers files/registry, not every object-manager call. Retain the
# kernel providers in the same birth window to expose that coverage gap.
@('Microsoft-Windows-Kernel-File 0xffffffffffffffff 0xff',
  'Microsoft-Windows-Kernel-Registry 0xffffffffffffffff 0xff',
  'Microsoft-Windows-Kernel-Object 0xffffffffffffffff 0xff') | Out-File -Encoding ascii evidence/kernel-providers.txt
& logman create trace $session -o $objectEtl -pf evidence/kernel-providers.txt 2>&1 | Out-File evidence/kernel-trace-setup.txt
$metadata['logman_create_exit'] = $LASTEXITCODE
if ($LASTEXITCODE -eq 0) {
    & logman start $session -ets 2>&1 | Out-File -Append evidence/kernel-trace-setup.txt
    $metadata['logman_start_exit'] = $LASTEXITCODE
    $objectTrace = $LASTEXITCODE -eq 0
}
function Invoke-Procmon([string[]]$Arguments) {
    # GUI executables do not reliably update PowerShell's LASTEXITCODE. Wait for
    # this exact instance and read its ExitCode before consuming its output.
    $process = Start-Process $procmon -ArgumentList $Arguments -PassThru
    $process.WaitForExit()
    return $process.ExitCode
}
try {
    $capture = Start-Process $procmon -ArgumentList @('/AcceptEula', '/Quiet', '/Minimized', '/NoFilter', '/BackingFile', $pml) -PassThru
    $idleExit = Invoke-Procmon @('/AcceptEula', '/WaitForIdle')
    $metadata['wait_for_idle_exit'] = $idleExit
    if ($idleExit -ne 0) { throw "Process Monitor WaitForIdle: $idleExit" }
    # The returned idle signal means the driver is ready before any worker birth.
    & target/release/win-confine.exe --output evidence/report.json
    $measurementExit = $LASTEXITCODE
} catch {
    $metadata['capture_error'] = $_.ToString()
    $metadata['capture_tool'] = 'WPR GeneralProfile fallback'
    $null = Invoke-Procmon @('/Terminate')
    & wpr -start GeneralProfile -filemode
    if ($LASTEXITCODE -ne 0) { throw "WPR start failed: $LASTEXITCODE" }
    $etw = $true
    & target/release/win-confine.exe --output evidence/report.json
    $measurementExit = $LASTEXITCODE
} finally {
    if ($objectTrace) {
        & logman stop $session -ets 2>&1 | Out-File -Append evidence/kernel-trace-setup.txt
        $metadata['logman_stop_exit'] = $LASTEXITCODE
    }
    & logman delete $session 2>&1 | Out-File -Append evidence/kernel-trace-setup.txt
    if ($etw) {
        & wpr -stop $etl
        $metadata['wpr_stop_exit'] = $LASTEXITCODE
    } else {
        $metadata['terminate_exit'] = Invoke-Procmon @('/Terminate')
        if ($capture) { $capture.WaitForExit(); $metadata['capture_exit'] = $capture.ExitCode }
    }
    $metadata | ConvertTo-Json -Depth 5 | Out-File -Encoding utf8 evidence/procmon-metadata.json
}
if ($objectTrace) {
    & tracerpt $objectEtl -o evidence/kernel-objects.xml -of XML -y 2>&1 | Out-File evidence/kernel-trace-export.txt
    $metadata['tracerpt_exit'] = $LASTEXITCODE
    $metadata | ConvertTo-Json -Depth 5 | Out-File -Encoding utf8 evidence/procmon-metadata.json
}
if (Test-Path $pml) {
    $exportExit = Invoke-Procmon @('/AcceptEula', '/Quiet', '/OpenLog', $pml, '/SaveAs', $csv)
    $metadata['export_exit'] = $exportExit
    $metadata | ConvertTo-Json -Depth 5 | Out-File -Encoding utf8 evidence/procmon-metadata.json
    if ($exportExit -ne 0 -or -not (Test-Path $csv)) { throw "Process Monitor CSV export failed: $exportExit" }
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
    $denied = Get-Content evidence/procmon-events.json -Raw | ConvertFrom-Json
    $acls = @($denied | ForEach-Object {
        $sequence = $_.sequence
        foreach ($event in $_.non_success) {
            if ($event.Result -notin @('ACCESS DENIED', 'PRIVILEGE NOT HELD')) { continue }
            $path = $event.Path
            $aclPath = $path
            if ($path -match '^HKLM\\') { $aclPath = 'Registry::HKEY_LOCAL_MACHINE\' + $path.Substring(5) }
            if ($path -match '^HKCU\\') { $aclPath = 'Registry::HKEY_CURRENT_USER\' + $path.Substring(5) }
            try {
                $acl = Get-Acl -LiteralPath $aclPath
                [ordered]@{ sequence = $sequence; path = $path; operation = $event.Operation; detail = $event.Detail; sddl = $acl.Sddl }
            } catch {
                [ordered]@{ sequence = $sequence; path = $path; operation = $event.Operation; detail = $event.Detail; acl_error = $_.ToString() }
            }
        }
    })
    ConvertTo-Json -InputObject $acls -Depth 8 | Out-File -Encoding utf8 evidence/denied-object-acls.json
    # Keep raw child rows and the lossless failures, not the runner-wide CSV with
    # unrelated processes. The binary PML is retained for stack/object inspection.
    Remove-Item $csv
}
exit $measurementExit
