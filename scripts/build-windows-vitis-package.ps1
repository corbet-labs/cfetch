#Requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Output,

    [string]$RyzenAiRoot = $env:RYZEN_AI_INSTALLATION_PATH,

    [string]$RuntimeLabel = "amd-ryzen-ai-local-vitis"
)

$ErrorActionPreference = "Stop"
if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
    throw "the Ryzen AI Vitis evidence package requires native Windows x64"
}
if ([string]::IsNullOrWhiteSpace($RyzenAiRoot)) {
    throw "set RYZEN_AI_INSTALLATION_PATH or pass -RyzenAiRoot"
}

$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$outputPath = if ([IO.Path]::IsPathFullyQualified($Output)) {
    [IO.Path]::GetFullPath($Output)
}
else {
    [IO.Path]::GetFullPath((Join-Path (Get-Location) $Output))
}
if (Test-Path -LiteralPath $outputPath) {
    throw "output already exists: $outputPath"
}
$ryzenRoot = (Resolve-Path -LiteralPath $RyzenAiRoot).Path
$deployment = Join-Path $ryzenRoot "deployment"
$requiredDlls = @(
    "aiecompiler_client.dll",
    "DirectML.dll",
    "dyn_dispatch_core.dll",
    "onnxruntime_providers_shared.dll",
    "onnxruntime_providers_vitisai.dll",
    "onnxruntime_vitis_ai_custom_ops.dll",
    "onnxruntime_vitisai_ep.dll",
    "onnxruntime.dll"
)

$sourceFiles = @()
foreach ($name in $requiredDlls) {
    $path = Join-Path $deployment $name
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Ryzen AI deployment is missing required INT8 runtime file: $path"
    }
    $sourceFiles += [ordered]@{
        path = "deployment/$name"
        sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
    }
}

# XDNA2 uses target X2 and must not set an xclbin. Phoenix/Hawk Point use X1
# and need AMD's packaged 4x4.xclbin, so retain those optional files for a
# tester who explicitly selects X1.
$xclbinRoot = Join-Path $ryzenRoot "voe-4.0-win_amd64/xclbins"
if (Test-Path -LiteralPath $xclbinRoot -PathType Container) {
    Get-ChildItem -LiteralPath $xclbinRoot -File -Recurse | Sort-Object FullName | ForEach-Object {
        $relative = [IO.Path]::GetRelativePath($xclbinRoot, $_.FullName).Replace("\", "/")
        $sourceFiles += [ordered]@{
            path = "xclbins/$relative"
            sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant()
        }
    }
}

$identityJson = $sourceFiles | ConvertTo-Json -Depth 4 -Compress
$identityBytes = [Text.UTF8Encoding]::new($false).GetBytes($identityJson)
$runtimeIdentity = [Convert]::ToHexString(
    [Security.Cryptography.SHA256]::HashData($identityBytes)
).ToLowerInvariant()

$env:CFETCH_ORT_DISTRIBUTION = "$RuntimeLabel-fileset"
$env:CFETCH_ORT_ARCHIVE_SHA256 = $runtimeIdentity
Push-Location $root
try {
    & cargo build --release --locked --no-default-features --features inference-vitis
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed"
    }
}
finally {
    Pop-Location
}

$runtimeOut = Join-Path $outputPath "runtime"
$licenseOut = Join-Path $outputPath "licenses"
New-Item -ItemType Directory -Path $runtimeOut, $licenseOut | Out-Null
Copy-Item -LiteralPath (Join-Path $root "target/release/cfetch.exe") -Destination $outputPath
foreach ($name in $requiredDlls) {
    Copy-Item -LiteralPath (Join-Path $deployment $name) -Destination $runtimeOut
}
if (Test-Path -LiteralPath $xclbinRoot -PathType Container) {
    Copy-Item -LiteralPath $xclbinRoot -Destination (Join-Path $runtimeOut "xclbins") -Recurse
}
Copy-Item -LiteralPath (Join-Path $root "LICENSE.md") -Destination (Join-Path $licenseOut "cfetch-LICENSE.md")
Copy-Item -LiteralPath (Join-Path $root "THIRD-PARTY-LICENSES.txt") -Destination $licenseOut
foreach ($notice in @("LICENSE.txt", "LICENSE", "ThirdPartyNotices.txt")) {
    $source = Join-Path $ryzenRoot $notice
    if (Test-Path -LiteralPath $source -PathType Leaf) {
        Copy-Item -LiteralPath $source -Destination (Join-Path $licenseOut "AMD-$notice")
    }
}

$origin = [ordered]@{
    provider = "vitis"
    distribution = $RuntimeLabel
    source = "local AMD Ryzen AI installation; proprietary runtime bytes are not redistributable by cfetch"
    runtime_fileset_sha256 = $runtimeIdentity
    source_files = $sourceFiles
}
$origin | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $outputPath "runtime-origin.json") -Encoding utf8NoBOM

$files = @()
Get-ChildItem -LiteralPath $outputPath -File -Recurse | Sort-Object FullName | ForEach-Object {
    $relative = [IO.Path]::GetRelativePath($outputPath, $_.FullName).Replace("\", "/")
    $files += [ordered]@{
        path = $relative
        sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant()
    }
}
$manifest = [ordered]@{
    schema_version = 1
    provider = "vitis"
    os = "windows"
    arch = "x64"
    cfetch_version = ((& (Join-Path $outputPath "cfetch.exe") --version) -split "\s+")[1]
    cargo_feature = "inference-vitis"
    onnxruntime_distribution = "$RuntimeLabel-fileset"
    onnxruntime_archive_sha256 = $runtimeIdentity
    onnxruntime_library_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $runtimeOut "onnxruntime.dll")).Hash.ToLowerInvariant()
    files = $files
}
$manifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $outputPath "runtime-manifest.json") -Encoding utf8NoBOM
Write-Output $outputPath
