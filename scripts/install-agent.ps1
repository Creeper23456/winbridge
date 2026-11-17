# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
Installs, updates, checks or removes the WinBridge Guest Agent (a Windows service).

.DESCRIPTION
Copies winbridge-agent.exe to Program Files and registers the Windows service WinBridgeAgent
(LocalSystem, starts at boot). The service starts the agent as SYSTEM in the console session, in
the logged-on user's environment, from the logon screen on, and starts it again when it ends or
when another user logs on. It replaces the scheduled task of earlier versions, which is removed.

Running it again updates the installation. Before anything is touched the script checks that the
file is a 64-bit Windows program of the agent that starts (a wrong file would run as SYSTEM). The
install directory is locked down to SYSTEM and Administrators (the service runs what is in it).
The previous binary is kept as winbridge-agent.previous.exe. If the new service does not start,
or does not bring up the agent in the console session, the previous state is restored and the
script exits with 1.

Run it from an elevated PowerShell.

.PARAMETER SourceBinary
The agent to install. Default: .\winbridge-agent.exe.

.PARAMETER InstallDir
Where the agent goes. Default: Program Files\WinBridge.

.PARAMETER Status
Prints the state of the installation (service, binary, running processes, logs, shared-memory
devices) and changes nothing.

.PARAMETER Uninstall
Stops and removes the service and ends the agent. The files stay.

.PARAMETER Purge
With -Uninstall: also deletes the installed binaries and the service log in ProgramData. The
per-user logs and drop staging folders in each profile (%LOCALAPPDATA%\WinBridge) stay.

.PARAMETER StartTimeoutSeconds
How long to wait for the service and the agent to come up. Default 30.
#>

[CmdletBinding()]
param (
    [string]$SourceBinary = ".\winbridge-agent.exe",
    [string]$InstallDir = (Join-Path $env:ProgramW6432 "WinBridge"),
    [switch]$Status,
    [switch]$Uninstall,
    [switch]$Purge,
    [ValidateRange(5, 300)][int]$StartTimeoutSeconds = 30
)

$ErrorActionPreference = "Stop"
$ServiceName = "WinBridgeAgent"
$ServiceKey = "HKLM:\SYSTEM\CurrentControlSet\Services\$ServiceName"
$DestBinary = Join-Path $InstallDir "winbridge-agent.exe"
$PreviousBinary = Join-Path $InstallDir "winbridge-agent.previous.exe"
$DataDir = Join-Path $env:ProgramData "WinBridge"
$ServiceLog = Join-Path $DataDir "service.log"

function Fail([string]$Message) {
    Write-Host "ERROR: $Message" -ForegroundColor Red
    exit 1
}

function Test-Administrator {
    $principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# The service, the agent it starts in the console session and the drag helper the agent starts.
function Get-AgentProcesses {
    foreach ($p in @(Get-CimInstance Win32_Process -Filter "Name = 'winbridge-agent.exe'")) {
        $role = "agent"
        if ($p.CommandLine -match "--service") { $role = "service" }
        elseif ($p.CommandLine -match "--drag-probe") { $role = "drag helper" }
        $owner = Invoke-CimMethod -InputObject $p -MethodName GetOwner -ErrorAction SilentlyContinue
        $user = "?"
        if ($owner -and $owner.ReturnValue -eq 0) { $user = "$($owner.Domain)\$($owner.User)" }
        [pscustomobject]@{ Id = $p.ProcessId; Role = $role; Session = $p.SessionId; User = $user }
    }
}

function Test-InteractiveSession {
    [bool](Get-Process -Name winlogon -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -ne 0 })
}

function Stop-Agent {
    # The service ends its child with it; an agent started by hand or by the old task must go too,
    # or it keeps the serial port the new agent needs.
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
    Get-Process -Name "winbridge-agent" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    $deadline = (Get-Date).AddSeconds(10)
    while ((Get-Process -Name "winbridge-agent" -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 200
    }
}

function Remove-OldTask {
    # The old per-user task would start a second agent at logon.
    if (Get-ScheduledTask -TaskName $ServiceName -ErrorAction SilentlyContinue) {
        Write-Host "Removing the scheduled task of earlier versions..."
        Unregister-ScheduledTask -TaskName $ServiceName -Confirm:$false
    }
}

# A 64-bit Windows program that is the WinBridge agent. Returns the problem, or $null when fine.
function Test-AgentFile([string]$Path) {
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 0x200 -or $bytes[0] -ne 0x4D -or $bytes[1] -ne 0x5A) {
        return "'$Path' is not a Windows program."
    }
    $pe = [BitConverter]::ToInt32($bytes, 0x3C)
    if ($pe -lt 0 -or $pe + 6 -gt $bytes.Length -or [BitConverter]::ToUInt32($bytes, $pe) -ne 0x00004550) {
        return "'$Path' is not a Windows program."
    }
    if ([BitConverter]::ToUInt16($bytes, $pe + 4) -ne 0x8664) {
        return "'$Path' is not a 64-bit (x64) program."
    }
    # Every build has the module path of its log messages in it.
    if (-not [Text.Encoding]::ASCII.GetString($bytes).Contains("winbridge_agent")) {
        return "'$Path' is not the WinBridge agent."
    }
    # Runs and exits at once; catches a missing runtime or a program the system refuses.
    $run = Start-Process -FilePath $Path -ArgumentList "--version" -Wait -PassThru -WindowStyle Hidden
    if ($run.ExitCode -ne 0) {
        return "'$Path --version' failed with exit code $($run.ExitCode)."
    }
    $null
}

# Warns when the VM has no working virtio serial device: the agent could not reach the host.
function Test-SerialDriver {
    $devices = @(Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.InstanceId -like "PCI\VEN_1AF4&DEV_1003*" -or $_.InstanceId -like "PCI\VEN_1AF4&DEV_1043*" })
    if ($devices.Count -eq 0) {
        Write-Warning "No virtio serial device in this machine. The agent reaches the host through it: add the WinBridge port to the VM (see scripts/launch-qemu-winbridge.sh)."
    } elseif (-not ($devices | Where-Object { $_.Status -eq "OK" })) {
        Write-Warning "The virtio serial device has no working driver (status: $($devices[0].Status)). Install the virtio-win drivers (vioser)."
    }
}

# Only SYSTEM and Administrators may write where the SYSTEM service runs its program from.
function Protect-InstallDir {
    & icacls.exe $InstallDir /inheritance:r /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" "*S-1-5-32-545:(OI)(CI)RX" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "icacls could not restrict '$InstallDir' (exit code $LASTEXITCODE)." }
}

# Waits until the service runs and, when somebody (or the logon screen) has a console session,
# until the service has the agent running in it. Throws when it does not come up.
function Wait-Agent {
    (Get-Service -Name $ServiceName).WaitForStatus("Running", [TimeSpan]::FromSeconds($StartTimeoutSeconds))
    if (-not (Test-InteractiveSession)) {
        Write-Warning "There is no interactive session yet; the service starts the agent when one appears."
        return
    }
    $deadline = (Get-Date).AddSeconds($StartTimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $agent = @(Get-AgentProcesses) | Where-Object { $_.Role -eq "agent" -and $_.Session -ne 0 }
        if ($agent) {
            Write-Host "Agent running: pid $($agent[0].Id) as $($agent[0].User) in session $($agent[0].Session)."
            return
        }
        Start-Sleep -Milliseconds 500
    }
    throw "the service runs but the agent did not start in the console session within $StartTimeoutSeconds s (see $ServiceLog)."
}

function Show-Status {
    Write-Host "=== WinBridge Guest Agent ===" -ForegroundColor Cyan
    $service = Get-CimInstance Win32_Service -Filter "Name = '$ServiceName'"
    if ($service) {
        Write-Host "Service:   $($service.State), start mode $($service.StartMode), account $($service.StartName)"
        Write-Host "Command:   $($service.PathName)"
    } else {
        Write-Host "Service:   not installed"
    }
    if (Get-ScheduledTask -TaskName $ServiceName -ErrorAction SilentlyContinue) {
        Write-Warning "The scheduled task of earlier versions is still registered; install again to remove it."
    }
    if (Test-Path -LiteralPath $DestBinary) {
        $file = Get-Item -LiteralPath $DestBinary
        $hash = (Get-FileHash -LiteralPath $DestBinary -Algorithm SHA256).Hash
        Write-Host "Binary:    $DestBinary, $($file.Length) bytes, $($file.LastWriteTime), SHA-256 $hash"
    } else {
        Write-Host "Binary:    $DestBinary missing"
    }
    if (Test-Path -LiteralPath $PreviousBinary) { Write-Host "Previous:  $PreviousBinary" }
    $processes = @(Get-AgentProcesses)
    if ($processes.Count -eq 0) { Write-Host "Processes: none running" }
    foreach ($p in $processes) {
        Write-Host ("Process:   pid {0}, {1}, {2}, session {3}" -f $p.Id, $p.Role, $p.User, $p.Session)
    }
    if (Test-Path -LiteralPath $ServiceLog) {
        Write-Host "Service log ($ServiceLog), last lines:"
        Get-Content -LiteralPath $ServiceLog -Tail 3 | ForEach-Object { Write-Host "  $_" }
    }
    if (Test-Path -LiteralPath $DestBinary) {
        $report = Join-Path ([IO.Path]::GetTempPath()) "winbridge-ivshmem-status.txt"
        Remove-Item -LiteralPath $report -Force -ErrorAction SilentlyContinue
        Start-Process -FilePath $DestBinary -ArgumentList "--ivshmem-status", "--ivshmem-output", "`"$report`"" -Wait -WindowStyle Hidden
        if (Test-Path -LiteralPath $report) {
            Write-Host "Shared-memory devices:"
            Get-Content -LiteralPath $report | ForEach-Object { Write-Host "  $_" }
            Remove-Item -LiteralPath $report -Force
        }
    }
    Test-SerialDriver
}

if (-not (Test-Administrator)) {
    Fail "Run this script from an elevated PowerShell (Run as administrator)."
}
if ($Purge -and -not $Uninstall) {
    Fail "-Purge only goes with -Uninstall."
}

if ($Status) {
    Show-Status
    exit 0
}

if ($Uninstall) {
    Write-Host "=== Removing the WinBridge Guest Agent ===" -ForegroundColor Cyan
    Remove-OldTask
    Stop-Agent
    if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
        & sc.exe delete $ServiceName | Out-Null
        if ($LASTEXITCODE -ne 0) { Fail "sc.exe delete failed with exit code $LASTEXITCODE." }
        Write-Host "Service removed."
    } else {
        Write-Host "The service was not installed."
    }
    if ($Purge) {
        # Only what the installer put there, so a shared InstallDir stays intact.
        foreach ($file in $DestBinary, $PreviousBinary) { Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue }
        if ((Test-Path -LiteralPath $InstallDir) -and -not (Get-ChildItem -LiteralPath $InstallDir -Force)) {
            Remove-Item -LiteralPath $InstallDir -Force
        }
        Remove-Item -LiteralPath $DataDir -Recurse -Force -ErrorAction SilentlyContinue
        Write-Host "Binaries and $DataDir deleted."
    }
    Write-Host "Done." -ForegroundColor Green
    exit 0
}

Write-Host "=== Installing WinBridge Guest Agent ===" -ForegroundColor Cyan

if (-not (Test-Path -LiteralPath $SourceBinary -PathType Leaf)) {
    Fail "Source binary '$SourceBinary' not found. Pass the path to winbridge-agent.exe with -SourceBinary."
}
$SourceBinary = (Resolve-Path -LiteralPath $SourceBinary).Path
$problem = Test-AgentFile $SourceBinary
if ($problem) { Fail $problem }
$sourceHash = (Get-FileHash -LiteralPath $SourceBinary -Algorithm SHA256).Hash
Write-Host "Agent: $SourceBinary (SHA-256 $sourceHash)"
Test-SerialDriver

$hadService = [bool](Get-Service -Name $ServiceName -ErrorAction SilentlyContinue)
$hadBinary = Test-Path -LiteralPath $DestBinary
$oldImagePath = $null
if ($hadService) { $oldImagePath = (Get-ItemProperty -LiteralPath $ServiceKey).ImagePath }
$sameBinary = $hadBinary -and ((Get-FileHash -LiteralPath $DestBinary -Algorithm SHA256).Hash -eq $sourceHash)
$backedUp = $false
$BinPath = "`"$DestBinary`" --service"

try {
    if (-not (Test-Path -LiteralPath $InstallDir)) {
        Write-Host "Creating installation directory: $InstallDir"
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    }
    Protect-InstallDir
    Remove-OldTask
    Stop-Agent

    if ($hadBinary -and -not $sameBinary) {
        Copy-Item -LiteralPath $DestBinary -Destination $PreviousBinary -Force
        $backedUp = $true
    }
    if ($sameBinary) {
        Write-Host "The installed binary is identical; keeping it."
    } else {
        Write-Host "Copying binary to $DestBinary..."
        Copy-Item -LiteralPath $SourceBinary -Destination $DestBinary -Force
    }

    # New-Service and the registry take the command line as it is; sc.exe would lose the quotes
    # around a path with spaces when PowerShell hands it over.
    if ($hadService) {
        Write-Host "Updating the service '$ServiceName'..."
        Set-ItemProperty -LiteralPath $ServiceKey -Name ImagePath -Value $BinPath
        Set-Service -Name $ServiceName -StartupType Automatic
    } else {
        Write-Host "Creating the service '$ServiceName'..."
        New-Service -Name $ServiceName -BinaryPathName $BinPath -DisplayName "WinBridge Guest Agent" -StartupType Automatic | Out-Null
    }
    Set-Service -Name $ServiceName -Description "Keeps the WinBridge guest agent (display, clipboard, drag and drop) running as SYSTEM in the console session."
    # Restart the service itself if it ever ends (the agent is restarted by the service already).
    & sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/5000/restart/5000 | Out-Null

    Write-Host "Starting the service..."
    Start-Service -Name $ServiceName
    Wait-Agent
} catch {
    Write-Host "Installation failed: $($_.Exception.Message)" -ForegroundColor Red
    Write-Host "Restoring the previous state..." -ForegroundColor Yellow
    Stop-Agent
    if ($backedUp) {
        Copy-Item -LiteralPath $PreviousBinary -Destination $DestBinary -Force
        Set-ItemProperty -LiteralPath $ServiceKey -Name ImagePath -Value $oldImagePath
        Start-Service -Name $ServiceName -ErrorAction SilentlyContinue
    } elseif (-not $hadService) {
        & sc.exe delete $ServiceName | Out-Null
        if (-not $hadBinary) { Remove-Item -LiteralPath $DestBinary -Force -ErrorAction SilentlyContinue }
    } else {
        # Failed before the binary was replaced: the service is as it was, only stopped.
        Start-Service -Name $ServiceName -ErrorAction SilentlyContinue
    }
    exit 1
}

Write-Host "=== WinBridge Guest Agent installation completed successfully ===" -ForegroundColor Green
Write-Host "Logs: $ServiceLog (service), %LOCALAPPDATA%\WinBridge\agent.log of the logged-on user (agent)."
Write-Host "Check later with: install-agent.ps1 -Status. Remove with: install-agent.ps1 -Uninstall [-Purge]."
