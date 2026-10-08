$ErrorActionPreference = 'Stop'
$chrome = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
if (!(Test-Path $chrome)) {
    choco install googlechrome --yes --no-progress
    if ($LASTEXITCODE) { throw "Chrome install failed: $LASTEXITCODE" }
}
$profile = Join-Path $env:TEMP ('basal-chrome-' + [guid]::NewGuid())
$binary = (Resolve-Path 'target/release/win-confine.exe').Path
$browser = $null
$seen = @()
try {
    $browser = Start-Process $chrome -ArgumentList "--headless=new --user-data-dir=`"$profile`" about:blank" -PassThru
    # Wait for renderer creation, not a fixed assumption about browser startup.
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    do {
        $all = @(Get-CimInstance Win32_Process -Filter "Name='chrome.exe'")
        $owned = @($browser.Id)
        do {
            $before = $owned.Count
            $owned += @($all | Where-Object { $_.ParentProcessId -in $owned -and $_.ProcessId -notin $owned } | ForEach-Object { $_.ProcessId })
        } while ($owned.Count -ne $before)
        $seen = @($all | Where-Object { $_.ProcessId -in $owned })
        $renderers = @($seen | Where-Object { $_.CommandLine -match '--type=renderer' })
        if (!$renderers.Count) { Start-Sleep -Milliseconds 500 }
    } while (!$renderers.Count -and [DateTime]::UtcNow -lt $deadline)
    # A newly observed renderer may still hold its startup impersonation token.
    # A later sample distinguishes that transient observation without claiming
    # that a timer alone proves completion of every sandbox transition.
    Start-Sleep -Seconds 10
    $all = @(Get-CimInstance Win32_Process -Filter "Name='chrome.exe'")
    $owned = @($browser.Id)
    do {
        $before = $owned.Count
        $owned += @($all | Where-Object { $_.ParentProcessId -in $owned -and $_.ProcessId -notin $owned } | ForEach-Object { $_.ProcessId })
    } while ($owned.Count -ne $before)
    $seen = @($all | Where-Object { $_.ProcessId -in $owned })
    $renderers = @($seen | Where-Object { $_.CommandLine -match '--type=renderer' })
    $inventory = @()
    foreach ($process in $seen) {
        $kind = if ($process.CommandLine -match '--type=renderer') { 'renderer' }
            elseif ($process.CommandLine -match '--type=gpu-process') { 'gpu' }
            elseif ($process.CommandLine -match 'network\.mojom\.NetworkService') { 'network' }
            elseif ($process.ProcessId -eq $browser.Id) { 'browser' }
            else { 'other' }
        $file = "evidence/chrome-$kind-$($process.ProcessId).json"
        if ($kind -ne 'other') {
            & $binary --observe $process.ProcessId --broker $browser.Id --output $file
            if ($LASTEXITCODE) { throw "External attestation failed: $LASTEXITCODE" }
        }
        $inventory += [pscustomobject]@{ pid=$process.ProcessId; parent_pid=$process.ParentProcessId; kind=$kind; command_line=$process.CommandLine; attestation=if($kind -ne 'other') { $file } else { $null } }
    }
    [pscustomobject]@{ image=$chrome; version=(Get-Item $chrome).VersionInfo.FileVersion; profile=$profile; sandbox_disabled=$false; browser_pid=$browser.Id; renderer_count=$renderers.Count; sample_delay_seconds=10; processes=$inventory } | ConvertTo-Json -Depth 10 | Set-Content evidence/chrome-inventory.json
    if (!$renderers.Count) { throw 'No sandbox-enabled Chrome renderer observed' }
} finally {
    foreach ($process in $seen) { Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue }
    if ($browser) { Stop-Process -Id $browser.Id -Force -ErrorAction SilentlyContinue }
    if (Test-Path $profile) { Remove-Item $profile -Recurse -Force -ErrorAction SilentlyContinue }
}
