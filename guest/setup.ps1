# Runs as SYSTEM at the end of Windows setup. It installs the agent, stops the machine going to sleep,
# and puts the driver where the guest looks for it. finish.ps1 does the part that needs the account.
$ErrorActionPreference = 'Continue'
New-Item -ItemType Directory -Force C:\gpupv, C:\gpupv\cap | Out-Null
Copy-Item (Join-Path $PSScriptRoot 'agent.ps1') C:\gpupv\agent.ps1 -Force
Copy-Item (Join-Path $PSScriptRoot 'token.txt') C:\gpupv\token.txt -Force
Copy-Item (Join-Path $PSScriptRoot 'setmode.ps1') C:\gpupv\setmode.ps1 -Force
New-NetFirewallRule -DisplayName 'gpupv-agent' -Direction Inbound -Protocol TCP -LocalPort 8765 -RemoteAddress 10.0.2.2 -Action Allow | Out-Null
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -File C:\gpupv\agent.ps1'
$trigger = New-ScheduledTaskTrigger -AtStartup
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit 0
Register-ScheduledTask -TaskName 'gpupv-agent' -Action $action -Trigger $trigger -User 'SYSTEM' -RunLevel Highest -Settings $settings -Force | Out-Null
powercfg /change standby-timeout-ac 0
powercfg /change monitor-timeout-ac 0
powercfg /hibernate off
# The largest firmware mode wins with this, and the app picks that with the VGA memory size.
bcdedit /set '{current}' highestmode on
# Windows looks here for the driver's user-mode half, under the name the daemon gives it. The disc
# has one maker's driver, in a folder named for the maker.
$stores = @{ nvidia = 'nv_dispi.inf_amd64_gpupv'; amd = 'amdxc.inf_amd64_gpupv'; intel = 'iigd_dch.inf_amd64_gpupv' }
foreach ($maker in $stores.Keys) {
    $from = Join-Path $PSScriptRoot $maker
    if (-not (Test-Path $from)) { continue }
    $store = Join-Path 'C:\Windows\System32\HostDriverStore\FileRepository' $stores[$maker]
    New-Item -ItemType Directory -Force $store | Out-Null
    robocopy $from $store /E /NP /NFL /NDL /NJH /NJS
    # The loaders and runtime the driver's INF would put in the system folders, which the app lists.
    $list = Join-Path $store 'system-files.txt'
    $placer = Join-Path $PSScriptRoot 'nv-files.ps1'
    if ($maker -eq 'intel' -and (Test-Path $list) -and (Test-Path $placer)) {
        & $placer $list -StoreFolder $stores[$maker] | Out-File C:\gpupv\system-files.log
    }
}
# A machine on an Intel card asks Intel's Direct3D 12 driver to load its classic module, so that what
# a test run shows doesn't depend on the driver's own choice. 2 is the value said to do that, and
# nothing here has confirmed it.
if (Test-Path (Join-Path $PSScriptRoot 'intel')) {
    $intelKey = 'HKLM:\SOFTWARE\INTEL\IGFX\D3D12'
    New-Item -Path $intelKey -Force | Out-Null
    New-ItemProperty -Path $intelKey -Name 'EnableTrinity' -PropertyType DWord -Value 2 -Force | Out-Null
}
Start-ScheduledTask -TaskName 'gpupv-agent'
'setup done' | Out-File C:\gpupv\setup.log
