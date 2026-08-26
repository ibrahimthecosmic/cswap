# Install cswap on Windows. Usage:
#   irm https://raw.githubusercontent.com/ibrahimthecosmic/cswap/main/install.ps1 | iex
#
# Set these beforehand to override:
#   $env:CSWAP_INSTALL_DIR   where to put the binary
#   $env:CSWAP_VERSION       a tag such as v0.1.0 (default: the latest release)

$ErrorActionPreference = 'Stop'

$Repo    = 'ibrahimthecosmic/cswap'
$Version = if ($env:CSWAP_VERSION) { $env:CSWAP_VERSION } else { 'latest' }
$Dir     = if ($env:CSWAP_INSTALL_DIR) { $env:CSWAP_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\cswap' }

function Get-OSArch {
    # Three sources, most precise first. PowerShell yields $null for a static
    # member it cannot resolve instead of throwing, so a missing type here has
    # to be caught by the `if`, not by `try`.
    try {
        $dotnet = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
        if ($dotnet) { return "$dotnet".ToUpperInvariant() }
    } catch {
        # Older hosts have no RuntimeInformation type at all. Fall through.
    }

    # On a 32-bit PowerShell running on 64-bit Windows, PROCESSOR_ARCHITECTURE
    # describes the *process* and PROCESSOR_ARCHITEW6432 describes the OS.
    $env_arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $env_arch) { $env_arch = $env:PROCESSOR_ARCHITECTURE }
    if ($env_arch) {
        switch ($env_arch.ToUpperInvariant()) {
            'AMD64' { return 'X64' }
            'ARM64' { return 'ARM64' }
            'X86'   { if ([Environment]::Is64BitOperatingSystem) { return 'X64' } else { return 'X86' } }
        }
    }

    if ([Environment]::Is64BitOperatingSystem) { return 'X64' }
    return 'UNKNOWN'
}

$Arch = Get-OSArch
switch ($Arch) {
    'X64'   { }
    'ARM64' { Write-Warning "Windows on ARM detected. Installing the x64 build, which runs under emulation on Windows 11." }
    'X86'   { throw "cswap needs 64-bit Windows (detected 32-bit). Build from source: cargo install --git https://github.com/$Repo" }
    default { Write-Warning "Could not determine the OS architecture (got '$Arch'). Assuming x64." }
}

$Asset = 'cswap-windows-x86_64.exe'
$Url = if ($Version -eq 'latest') {
    "https://github.com/$Repo/releases/latest/download/$Asset"
} else {
    "https://github.com/$Repo/releases/download/$Version/$Asset"
}

Write-Host "Downloading $Asset ($Version)..."
$Temp = Join-Path ([System.IO.Path]::GetTempPath()) "cswap-$([guid]::NewGuid()).exe"
try {
    # Windows PowerShell 5.1 does not negotiate TLS 1.2 by default; PowerShell 7
    # does, and setting ServicePointManager there is obsolete.
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    }
    Invoke-WebRequest -Uri $Url -OutFile $Temp -UseBasicParsing
} catch {
    throw "Download failed from $Url : $($_.Exception.Message)"
}

# Refuse an HTML error page renamed to look like a binary.
$Bytes = [System.IO.File]::ReadAllBytes($Temp)
if ($Bytes.Length -lt 100000 -or $Bytes[0] -ne 0x4D -or $Bytes[1] -ne 0x5A) {
    Remove-Item $Temp -Force
    throw "Downloaded file is not a Windows executable ($($Bytes.Length) bytes from $Url)"
}

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$Target = Join-Path $Dir 'cswap.exe'
Move-Item -Force $Temp $Target

Write-Host "Installed $Target"
& $Target --version

# Put it on the user PATH for future sessions, and on this session's PATH now.
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($UserPath -notlike "*$Dir*") {
    [Environment]::SetEnvironmentVariable('Path', "$UserPath;$Dir", 'User')
    Write-Host "Added $Dir to your user PATH - open a new terminal to pick it up."
}
$env:Path = "$env:Path;$Dir"
