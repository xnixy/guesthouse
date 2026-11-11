# Copies the driver's files to where a normal install puts them, so programs find them by name. The
# app sends the list. A file already there stays unless this one is newer: the file version decides,
# then the date.
# Usage: nv-files.ps1 LIST [-StoreFolder NAME]
# The files are taken from the driver's folder in the host driver store: NVIDIA's unless another is named.
param([Parameter(Mandatory)][string]$List, [string]$StoreFolder = 'nv_dispi.inf_amd64_gpupv')
$store = Join-Path 'C:\Windows\System32\HostDriverStore\FileRepository' $StoreFolder

function Test-Newer($source, $target) {
    if (-not (Test-Path -LiteralPath $target)) { return $true }
    $new = Get-Item -LiteralPath $source
    $old = Get-Item -LiteralPath $target
    if ($target -match '\.(dll|exe)$') {
        $newVersion = $new.VersionInfo.FileVersionRaw
        $oldVersion = $old.VersionInfo.FileVersionRaw
        if ($newVersion -ne $oldVersion) { return $newVersion -gt $oldVersion }
    }
    return $new.LastWriteTimeUtc -gt $old.LastWriteTimeUtc
}

$copied = 0
$kept = 0
$failed = 0
foreach ($line in Get-Content -LiteralPath $List) {
    $folder, $name, $target = $line -split "`t"
    $source = Join-Path $store $name
    $path = Join-Path (Join-Path $env:windir $folder) $target
    try {
        if (Test-Newer $source $path) {
            Copy-Item -LiteralPath $source -Destination $path -Force -ErrorAction Stop
            $copied++
            "copied $folder\$target from $name"
        } else {
            $kept++
            "kept $folder\$target"
        }
    } catch {
        $failed++
        "FAILED $folder\$target from ${name}: $($_.Exception.Message)"
    }
}
"copied $copied, kept $kept, failed $failed"
