# Writes a driver package into Windows that binds its own virtual render driver to the GPU device under
# the card maker's vendor number, so the next start can show that number. Runs as SYSTEM, sent by the host, which
# puts $vendor, $device and $driverVersion in front of this. The package is made here from this Windows' own
# copy of the driver, nothing is brought in, and the catalog is signed the way the display driver's is: with a
# certificate made here, trusted only here, whose private key is deleted straight after.
# The package gives the device the card driver's version, $driverVersion, as some games read the device's
# driver version and refuse the one Windows' own driver has. With no $driverVersion that line stays as it is.
# A package of ours with another version is replaced, and 'identity updated' says the device was running on
# it, so Windows has to restart.
# The last line says 'identity staged', or 'stay: ' and why Windows keeps Microsoft's number.
$ErrorActionPreference = 'Continue'
$inboxInf = 'C:\Windows\INF\vrd.inf'
$driverStore = 'C:\Windows\System32\DriverStore\FileRepository'
$folder = 'C:\gpupv\render-identity'
$package = "$folder\pkg"
# One package for each vendor number. Packages from before AMD cards are named without it.
$infName = "guesthouse-render-$vendor.inf"
$catalogName = "guesthouse-render-$vendor.cat"
$ourPackages = '*\guesthouse-render*.inf'
$subject = 'CN=Guesthouse render identity, this machine only'
# Windows puts the name in quotes in what it reads back, as it has a comma in it.
$ourCertificates = '*Guesthouse render identity, this machine only*'
$yearsValid = 10
$hardwareId = "PCI\VEN_$vendor&DEV_$device"
# The inbox INF's model line, which is the only line that changes. Anything else there is kept as it is.
$modelLine = '^(\s*%VirtualRender\.DeviceDesc%\s*=\s*VirtualRender_Inst\s*,\s*)PCI\\VEN_1414&DEV_008E\s*$'
$versionLine = '^\s*DriverVer\s*='
# Windows takes the package with the newer date when two bind one device, so a new one is dated today.
$ourVersionLine = "DriverVer = $(Get-Date -Format 'MM/dd/yyyy'),$driverVersion"
# The line as an earlier run wrote it, on whatever day that was.
$sameVersionLine = '^\s*DriverVer\s*=\s*[\d/]+,' + [regex]::Escape($driverVersion) + '\s*$'

function Remove-OurCertificates {
    foreach ($store in 'Root', 'TrustedPublisher') {
        Get-ChildItem "Cert:\LocalMachine\$store" | Where-Object Subject -like $ourCertificates | Remove-Item
    }
    Get-ChildItem Cert:\LocalMachine\My | Where-Object Subject -like $ourCertificates |
        ForEach-Object { Remove-Item "Cert:\LocalMachine\My\$($_.Thumbprint)" -DeleteKey }
}

# Our packages already in Windows that bind this number.
function Find-Packages {
    Get-WindowsDriver -Online -ea 0 | Where-Object OriginalFileName -like $ourPackages |
        Where-Object { (Get-Content -LiteralPath $_.OriginalFileName -ea 0) -match [regex]::Escape($hardwareId) }
}

# Whether a package gives the device the version wanted. Any version will do when none is wanted.
function Test-Version($found) {
    if (-not $driverVersion) { return $true }
    [bool]((Get-Content -LiteralPath $found.OriginalFileName -ea 0) -match $sameVersionLine)
}

function Find-Package { Find-Packages | Where-Object { Test-Version $_ } | Select-Object -First 1 }
function Find-OldPackages { @(Find-Packages | Where-Object { -not (Test-Version $_) }) }

# Whether the GPU device is in this Windows under the maker's number now.
function Test-DevicePresent {
    [bool](Get-PnpDevice -PresentOnly -ea 0 | Where-Object InstanceId -like "$hardwareId*")
}

# Takes our packages with another version out of Windows. One a device still holds is taken from the
# device too, which then goes to the package that's left.
function Remove-OldPackages($old) {
    foreach ($found in $old) {
        pnputil /delete-driver $found.Driver 2>&1
        if ($LASTEXITCODE -ne 0) { pnputil /delete-driver $found.Driver /uninstall 2>&1 }
    }
}

# Everything that can refuse returns from here, as the agent's shell must go on to send the answer.
function Stage-Package {
    $already = Find-Package
    if ($already) {
        "package in: $($already.Driver)"
        return 'identity staged'
    }
    $old = Find-OldPackages

    if (-not (Test-Path -LiteralPath $inboxInf)) { return "stay: Windows has no virtual render driver INF at $inboxInf." }
    $lines = Get-Content -LiteralPath $inboxInf
    $models = @($lines | Where-Object { $_ -match $modelLine })
    if ($models.Count -ne 1) { return "stay: Windows' virtual render driver INF isn't shaped as expected, so no package was made from it." }
    if (-not ($lines -match '^\s*\[Version\]\s*$')) { return "stay: Windows' virtual render driver INF has no [Version] section." }
    if ($driverVersion -and @($lines | Where-Object { $_ -match $versionLine }).Count -ne 1) { return "stay: Windows' virtual render driver INF has no one DriverVer line to change." }
    if ($lines -match '^\s*CatalogFile\s*=') { return "stay: Windows' virtual render driver INF names a catalog of its own, which wasn't expected." }
    $source = Get-ChildItem $driverStore -Directory -Filter 'vrd.inf_*' -ea 0 |
        Where-Object { Test-Path -LiteralPath "$($_.FullName)\vrd.sys" } | Select-Object -First 1
    if (-not $source) { return "stay: Windows' driver store has no copy of vrd.sys." }
    $signature = Get-AuthenticodeSignature -LiteralPath "$($source.FullName)\vrd.sys"
    if ($signature.Status -ne 'Valid') { return "stay: The copy of vrd.sys in Windows' driver store isn't validly signed: $($signature.Status)." }

    New-Item -ItemType Directory -Force $package | Out-Null
    Remove-Item "$package\*" -Force -ea 0
    Copy-Item -LiteralPath "$($source.FullName)\vrd.sys" "$package\vrd.sys"
    $made = foreach ($line in $lines) {
        if ($line -match $modelLine) { $Matches[1] + $hardwareId }
        elseif ($line -match '^\s*\[Version\]\s*$') { $line; "CatalogFile = $catalogName" }
        elseif ($line -match '^\s*Provider\s*=') { 'Provider = %Guesthouse%' }
        elseif ($line -match '^\s*\[Strings\]\s*$') { $line; 'Guesthouse = "Guesthouse"' }
        elseif ($driverVersion -and $line -match $versionLine) { $ourVersionLine }
        else { $line }
    }
    Set-Content -LiteralPath "$package\$infName" -Value $made -Encoding ASCII

    # Only the newest certificate is ever needed, and a run that died part way could have left a key behind.
    Remove-OurCertificates
    $catalog = "$folder\$catalogName"
    Remove-Item -LiteralPath $catalog -ea 0
    New-FileCatalog -Path $package -CatalogFilePath $catalog -CatalogVersion 2.0 | Out-Null
    Move-Item -LiteralPath $catalog "$package\$catalogName" -Force
    $certificate = New-SelfSignedCertificate -Type CodeSigningCert -Subject $subject -CertStoreLocation Cert:\LocalMachine\My `
        -KeyExportPolicy NonExportable -NotAfter (Get-Date).AddYears($yearsValid)
    $public = New-Object Security.Cryptography.X509Certificates.X509Certificate2 (, $certificate.Export('Cert'))
    foreach ($name in 'Root', 'TrustedPublisher') {
        $store = New-Object Security.Cryptography.X509Certificates.X509Store $name, 'LocalMachine'
        $store.Open('ReadWrite')
        $store.Add($public)
        $store.Close()
    }
    $signed = Set-AuthenticodeSignature -FilePath "$package\$catalogName" -Certificate $certificate -HashAlgorithm SHA256
    "signing: $($signed.Status) $($signed.StatusMessage)"
    Remove-Item "Cert:\LocalMachine\My\$($certificate.Thumbprint)" -DeleteKey
    if ($signed.Status -ne 'Valid') { Remove-OurCertificates; return 'stay: The package could not be signed here.' }

    # A device running on an older package of ours is moved to this one. Any other gets it at the next start.
    $running = $old.Count -gt 0 -and (Test-DevicePresent)
    if ($running) { pnputil /add-driver "$package\$infName" /install 2>&1 }
    else { pnputil /add-driver "$package\$infName" 2>&1 }
    $added = Find-Package
    if (-not $added) { Remove-OurCertificates; return 'stay: Windows did not take the package.' }
    "package in: $($added.Driver)"
    Remove-OldPackages $old
    if ($running) { 'identity updated' }
    return 'identity staged'
}

Stage-Package
