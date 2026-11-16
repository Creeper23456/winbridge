# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
Installs and registers the WinBridge Venus Vulkan driver in Windows 11.

.DESCRIPTION
Copies vulkan_virtio.dll (the Mesa Venus driver with the WinBridge renderer backend) to
Program Files, writes its Vulkan ICD manifest next to it and registers the manifest under
HKLM\SOFTWARE\Khronos\Vulkan\Drivers, which is where the Vulkan loader looks for drivers.
With -SourceDll32 the 32-bit build is installed the same way for 32-bit programs (alone when -SourceDll is
not given): into
Program Files (x86) and registered under HKLM\SOFTWARE\WOW6432Node\Khronos\Vulkan\Drivers, the
key that the 32-bit Vulkan loader reads.
The driver talks to the host through an ivshmem device and the virtio-serial port org.winbridge.venus.N, so the VM
must be started with that device and port and the host must run winbridge-venus (venus/README.md).

Run it from an elevated PowerShell. -Uninstall removes the registrations and the files of both builds.
#>

[CmdletBinding()]
param (
    [string]$SourceDll = ".\vulkan_virtio.dll",
    [string]$SourceDll32 = "",
    [string]$InstallDir = "C:\Program Files\WinBridge\Venus",
    [string]$InstallDir32 = "C:\Program Files (x86)\WinBridge\Venus",
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"

$Registry64 = "HKLM:\SOFTWARE\Khronos\Vulkan\Drivers"
$Registry32 = "HKLM:\SOFTWARE\WOW6432Node\Khronos\Vulkan\Drivers"

function Install-Driver([string]$Source, [string]$Dir, [string]$RegistryPath) {
    if (-not (Test-Path $Source)) {
        Write-Error "Driver '$Source' not found. Please provide the path to vulkan_virtio.dll."
        exit 1
    }
    $Manifest = Join-Path $Dir "winbridge_venus_icd.json"
    $Dll = Join-Path $Dir "vulkan_virtio.dll"

    if (-not (Test-Path $Dir)) {
        Write-Host "Creating installation directory: $Dir"
        New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    }

    Write-Host "Copying driver to $Dll..."
    Copy-Item -Path $Source -Destination $Dll -Force

    Write-Host "Writing ICD manifest $Manifest..."
    $Json = [ordered]@{
        file_format_version = "1.0.0"
        ICD = [ordered]@{
            library_path = $Dll
            api_version  = "1.3.0"
        }
    } | ConvertTo-Json
    [System.IO.File]::WriteAllText($Manifest, $Json)

    Write-Host "Registering the manifest with the Vulkan loader ($RegistryPath)..."
    if (-not (Test-Path $RegistryPath)) {
        New-Item -Path $RegistryPath -Force | Out-Null
    }
    New-ItemProperty -Path $RegistryPath -Name $Manifest -Value 0 -PropertyType DWord -Force | Out-Null
}

if ($Uninstall) {
    Write-Host "=== Removing WinBridge Venus Vulkan driver ===" -ForegroundColor Cyan
    Remove-ItemProperty -Path $Registry64 -Name (Join-Path $InstallDir "winbridge_venus_icd.json") -ErrorAction SilentlyContinue
    Remove-ItemProperty -Path $Registry32 -Name (Join-Path $InstallDir32 "winbridge_venus_icd.json") -ErrorAction SilentlyContinue
    Remove-Item -Path $InstallDir -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -Path $InstallDir32 -Recurse -Force -ErrorAction SilentlyContinue
    exit 0
}

Write-Host "=== Installing WinBridge Venus Vulkan driver ===" -ForegroundColor Cyan
# The 64-bit build is installed unless only -SourceDll32 was given (to add the 32-bit build later).
if ($PSBoundParameters.ContainsKey("SourceDll") -or -not $PSBoundParameters.ContainsKey("SourceDll32")) {
    Install-Driver $SourceDll $InstallDir $Registry64
}
if ($SourceDll32 -ne "") {
    Install-Driver $SourceDll32 $InstallDir32 $Registry32
}

Write-Host "=== WinBridge Venus Vulkan driver installation completed successfully ===" -ForegroundColor Green
