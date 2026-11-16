# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
Installs and registers the WinBridge Venus Vulkan driver in Windows 11.

.DESCRIPTION
Copies vulkan_virtio.dll (the Mesa Venus driver with the WinBridge renderer backend) to
Program Files, writes its Vulkan ICD manifest next to it and registers the manifest under
HKLM\SOFTWARE\Khronos\Vulkan\Drivers, which is where the Vulkan loader looks for drivers.
The driver talks to the host through the virtio-serial port org.winbridge.venus.0, so the VM
must be started with that port and the host must run winbridge-venus.

Run it from an elevated PowerShell. -Uninstall removes the registration and the files.
#>

[CmdletBinding()]
param (
    [string]$SourceDll = ".\vulkan_virtio.dll",
    [string]$InstallDir = "C:\Program Files\WinBridge\Venus",
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"

$RegistryPath = "HKLM:\SOFTWARE\Khronos\Vulkan\Drivers"
$Manifest = Join-Path $InstallDir "winbridge_venus_icd.json"
$Dll = Join-Path $InstallDir "vulkan_virtio.dll"

if ($Uninstall) {
    Write-Host "=== Removing WinBridge Venus Vulkan driver ===" -ForegroundColor Cyan
    Remove-ItemProperty -Path $RegistryPath -Name $Manifest -ErrorAction SilentlyContinue
    Remove-Item -Path $InstallDir -Recurse -Force -ErrorAction SilentlyContinue
    exit 0
}

Write-Host "=== Installing WinBridge Venus Vulkan driver ===" -ForegroundColor Cyan

if (-not (Test-Path $SourceDll)) {
    Write-Error "Driver '$SourceDll' not found. Please provide the path to vulkan_virtio.dll."
    exit 1
}

if (-not (Test-Path $InstallDir)) {
    Write-Host "Creating installation directory: $InstallDir"
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

Write-Host "Copying driver to $Dll..."
Copy-Item -Path $SourceDll -Destination $Dll -Force

Write-Host "Writing ICD manifest $Manifest..."
$Json = [ordered]@{
    file_format_version = "1.0.0"
    ICD = [ordered]@{
        library_path = $Dll
        api_version  = "1.3.0"
    }
} | ConvertTo-Json
[System.IO.File]::WriteAllText($Manifest, $Json)

Write-Host "Registering the manifest with the Vulkan loader..."
if (-not (Test-Path $RegistryPath)) {
    New-Item -Path $RegistryPath -Force | Out-Null
}
New-ItemProperty -Path $RegistryPath -Name $Manifest -Value 0 -PropertyType DWord -Force | Out-Null

Write-Host "=== WinBridge Venus Vulkan driver installation completed successfully ===" -ForegroundColor Green
