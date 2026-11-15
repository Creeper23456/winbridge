# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
Installs and registers WinBridge Guest Agent in Windows 11.

.DESCRIPTION
Copies winbridge-agent.exe to Program Files and registers a Scheduled Task
to start automatically at user logon in the active desktop session.
#>

[CmdletBinding()]
param (
    [string]$SourceBinary = ".\winbridge-agent.exe",
    [string]$InstallDir = "C:\Program Files\WinBridge"
)

$ErrorActionPreference = "Stop"

Write-Host "=== Installing WinBridge Guest Agent ===" -ForegroundColor Cyan

if (-not (Test-Path $SourceBinary)) {
    Write-Error "Source binary '$SourceBinary' not found. Please provide the path to winbridge-agent.exe."
    exit 1
}

# Create installation directory
if (-not (Test-Path $InstallDir)) {
    Write-Host "Creating installation directory: $InstallDir"
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

$DestBinary = Join-Path $InstallDir "winbridge-agent.exe"
Write-Host "Copying binary to $DestBinary..."
Copy-Item -Path $SourceBinary -Destination $DestBinary -Force

# Register Scheduled Task to run in interactive user session
$TaskName = "WinBridgeAgent"
Write-Host "Registering Scheduled Task '$TaskName' to run at logon..."

$Action = New-ScheduledTaskAction -Execute $DestBinary
$Trigger = New-ScheduledTaskTrigger -AtLogOn
$Principal = New-ScheduledTaskPrincipal -GroupId "BUILTIN\Users" -RunLevel Highest
$Settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Days 365)

# Unregister old task if exists
Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue

Register-ScheduledTask -TaskName $TaskName -Action $Action -Trigger $Trigger -Principal $Principal -Settings $Settings -Description "WinBridge Guest Desktop Synchronization Agent" | Out-Null

Write-Host "Starting agent task now..."
Start-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue

Write-Host "=== WinBridge Guest Agent installation completed successfully ===" -ForegroundColor Green
