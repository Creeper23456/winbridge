# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
Installs and registers the WinBridge Guest Agent in Windows 11 as a service.

.DESCRIPTION
Copies winbridge-agent.exe to Program Files and registers the Windows service WinBridgeAgent
(LocalSystem, starts at boot). The service starts the agent as SYSTEM in the console session, in
the logged-on user's environment, from the logon screen on, and starts it again when it ends or
when another user logs on. It replaces the scheduled task of earlier versions, which is removed.

Run it from an elevated PowerShell.

.PARAMETER Uninstall
Stops and removes the service and ends the agent; the files stay.
#>

[CmdletBinding()]
param (
    [string]$SourceBinary = ".\winbridge-agent.exe",
    [string]$InstallDir = "C:\Program Files\WinBridge",
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
$ServiceName = "WinBridgeAgent"

function Stop-Agent {
    # The service ends its child with it; an agent started by hand or by the old task must go too,
    # or it keeps the serial port the new agent needs.
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
    Get-Process -Name "winbridge-agent" -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Seconds 1
}

if ($Uninstall) {
    Write-Host "=== Removing the WinBridge Guest Agent service ===" -ForegroundColor Cyan
    Stop-Agent
    & sc.exe delete $ServiceName | Out-Null
    Write-Host "Done." -ForegroundColor Green
    exit 0
}

Write-Host "=== Installing WinBridge Guest Agent ===" -ForegroundColor Cyan

if (-not (Test-Path $SourceBinary)) {
    Write-Error "Source binary '$SourceBinary' not found. Please provide the path to winbridge-agent.exe."
    exit 1
}

if (-not (Test-Path $InstallDir)) {
    Write-Host "Creating installation directory: $InstallDir"
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

# The old per-user task would start a second agent at logon.
if (Get-ScheduledTask -TaskName $ServiceName -ErrorAction SilentlyContinue) {
    Write-Host "Removing the scheduled task of earlier versions..."
    Unregister-ScheduledTask -TaskName $ServiceName -Confirm:$false
}

Stop-Agent

$DestBinary = Join-Path $InstallDir "winbridge-agent.exe"
Write-Host "Copying binary to $DestBinary..."
Copy-Item -Path $SourceBinary -Destination $DestBinary -Force

# New-Service and the registry take the command line as it is; sc.exe would lose the quotes
# around a path with spaces when PowerShell hands it over.
$BinPath = "`"$DestBinary`" --service"
$ServiceKey = "HKLM:\SYSTEM\CurrentControlSet\Services\$ServiceName"
if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
    Write-Host "Updating the service '$ServiceName'..."
    Set-ItemProperty -Path $ServiceKey -Name ImagePath -Value $BinPath
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

Write-Host "=== WinBridge Guest Agent installation completed successfully ===" -ForegroundColor Green
Write-Host "Logs: %ProgramData%\WinBridge\service.log (service), %LOCALAPPDATA%\WinBridge\agent.log of the logged-on user (agent)."
