#Requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("directml", "qnn", "openvino-cpu", "openvino-gpu", "openvino-npu", "webgpu")]
    [string]$Provider,

    [Parameter(Mandatory = $true)]
    [string]$Output
)

$ErrorActionPreference = "Stop"
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

$architecture = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x64" }
    "ARM64" { "arm64" }
    default { throw "unsupported Windows architecture: $env:PROCESSOR_ARCHITECTURE" }
}

$packages = @{
    directml = @{
        Feature = "inference-directml"
        Runtime = "win-$architecture"
        Url = "https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime.directml/1.24.4/microsoft.ml.onnxruntime.directml.1.24.4.nupkg"
        Sha256 = "57e9f11b73437bef7a309496135d4c1f96b1a8e9ddba60013fa27bfc1d788681"
        Distribution = "nuget-Microsoft.ML.OnnxRuntime.DirectML-1.24.4"
    }
    qnn = @{
        Feature = "inference-qnn"
        Runtime = "win-arm64"
        Url = "https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime.qnn/1.24.4/microsoft.ml.onnxruntime.qnn.1.24.4.nupkg"
        Sha256 = "e4d6eabb9e503d4f3c78494fc9400f02509b2ee315d9f707644a174ece8da17f"
        Distribution = "nuget-Microsoft.ML.OnnxRuntime.QNN-1.24.4"
    }
    openvino = @{
        Feature = "inference-openvino"
        Runtime = "win-x64"
        Url = "https://api.nuget.org/v3-flatcontainer/intel.ml.onnxruntime.openvino/1.24.1/intel.ml.onnxruntime.openvino.1.24.1.nupkg"
        Sha256 = "f53ad5f90e3d616970a5c65e4880ebbe92c9774e9727020661db591cea74a110"
        Distribution = "nuget-Intel.ML.OnnxRuntime.OpenVino-1.24.1"
    }
    webgpu = @{
        Feature = "inference-webgpu"
        Runtime = "win-$architecture"
        Url = "https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/1.28.0/microsoft.ml.onnxruntime.1.28.0.nupkg"
        Sha256 = "769d1d3ea8ab6cd69f737c9dd4d4462aa4ad0ccfa106eaf506efc40d7bead5db"
        Distribution = "nuget-Microsoft.ML.OnnxRuntime-1.28.0"
        Plugin = @{
            Url = "https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime.ep.webgpu/0.2.1/microsoft.ml.onnxruntime.ep.webgpu.0.2.1.nupkg"
            Sha256 = "a707557c86eb1eee0a604146ac4edc473d5af0bfe2fc77fd632217755cbfb282"
            Distribution = "nuget-Microsoft.ML.OnnxRuntime.EP.WebGpu-0.2.1"
            Library = "onnxruntime_providers_webgpu.dll"
        }
    }
}
$packageKey = if ($Provider.StartsWith("openvino-", [StringComparison]::Ordinal)) {
    "openvino"
}
else {
    $Provider
}
$package = $packages[$packageKey]
if ($Provider -eq "qnn" -and $architecture -ne "arm64") {
    throw "the QNN HTP evidence package requires native Windows ARM64"
}
if ($packageKey -eq "openvino" -and $architecture -ne "x64") {
    throw "the Intel OpenVINO evidence package requires native Windows x64"
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("cfetch-windows-package-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $archive = Join-Path $work "runtime.nupkg"
    Invoke-WebRequest -Uri $package.Url -OutFile $archive
    $archiveHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
    if ($archiveHash -ne $package.Sha256) {
        throw "runtime archive SHA-256 $archiveHash does not match $($package.Sha256)"
    }

    $expanded = Join-Path $work "expanded"
    New-Item -ItemType Directory -Path $expanded | Out-Null
    & tar.exe -xf $archive -C $expanded
    if ($LASTEXITCODE -ne 0) {
        throw "failed to extract the verified NuGet runtime"
    }
    $native = Join-Path $expanded "runtimes/$($package.Runtime)/native"
    if (-not (Test-Path -LiteralPath (Join-Path $native "onnxruntime.dll"))) {
        throw "verified runtime contains no $($package.Runtime)/native/onnxruntime.dll"
    }

    $pluginExpanded = $null
    if ($package.ContainsKey("Plugin")) {
        $pluginArchive = Join-Path $work "plugin.nupkg"
        Invoke-WebRequest -Uri $package.Plugin.Url -OutFile $pluginArchive
        $pluginArchiveHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $pluginArchive).Hash.ToLowerInvariant()
        if ($pluginArchiveHash -ne $package.Plugin.Sha256) {
            throw "provider plugin archive SHA-256 $pluginArchiveHash does not match $($package.Plugin.Sha256)"
        }
        $pluginExpanded = Join-Path $work "plugin-expanded"
        New-Item -ItemType Directory -Path $pluginExpanded | Out-Null
        & tar.exe -xf $pluginArchive -C $pluginExpanded
        if ($LASTEXITCODE -ne 0) {
            throw "failed to extract the verified provider plugin NuGet"
        }
        $pluginNative = Join-Path $pluginExpanded "runtimes/$($package.Runtime)/native"
        if (-not (Test-Path -LiteralPath (Join-Path $pluginNative $package.Plugin.Library))) {
            throw "verified provider plugin contains no $($package.Runtime)/native/$($package.Plugin.Library)"
        }
    }

    $env:CFETCH_ORT_DISTRIBUTION = $package.Distribution
    $env:CFETCH_ORT_ARCHIVE_SHA256 = $package.Sha256
    if ($package.ContainsKey("Plugin")) {
        $env:CFETCH_EP_PLUGIN_DISTRIBUTION = $package.Plugin.Distribution
        $env:CFETCH_EP_PLUGIN_ARCHIVE_SHA256 = $package.Plugin.Sha256
    }
    Push-Location $root
    try {
        & cargo build --release --locked --no-default-features --features $package.Feature
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
    Copy-Item -Path (Join-Path $native "*") -Destination $runtimeOut -Recurse
    if ($package.ContainsKey("Plugin")) {
        Copy-Item -Path (Join-Path $pluginNative "*") -Destination $runtimeOut -Recurse
    }
    Copy-Item -LiteralPath (Join-Path $root "LICENSE.md") -Destination (Join-Path $licenseOut "cfetch-LICENSE.md")
    Copy-Item -LiteralPath (Join-Path $root "THIRD-PARTY-LICENSES.txt") -Destination $licenseOut
    foreach ($notice in @("LICENSE", "ThirdPartyNotices.txt", "Privacy.md", "Qualcomm_LICENSE.pdf")) {
        $source = Join-Path $expanded $notice
        if (Test-Path -LiteralPath $source) {
            Copy-Item -LiteralPath $source -Destination $licenseOut
        }
    }
    if ($package.ContainsKey("Plugin")) {
        foreach ($notice in @("LICENSE", "ThirdPartyNotices.txt")) {
            $source = Join-Path $pluginExpanded $notice
            if (Test-Path -LiteralPath $source) {
                Copy-Item -LiteralPath $source -Destination (Join-Path $licenseOut "WebGPU-$notice")
            }
        }
    }

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
        provider = $Provider
        os = "windows"
        arch = $architecture
        cfetch_version = ((& (Join-Path $outputPath "cfetch.exe") --version) -split "\s+")[1]
        cargo_feature = $package.Feature
        onnxruntime_distribution = $package.Distribution
        onnxruntime_archive_sha256 = $package.Sha256
        onnxruntime_library_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $runtimeOut "onnxruntime.dll")).Hash.ToLowerInvariant()
        files = $files
    }
    if ($package.ContainsKey("Plugin")) {
        $manifest.execution_provider_plugin_distribution = $package.Plugin.Distribution
        $manifest.execution_provider_plugin_archive_sha256 = $package.Plugin.Sha256
        $manifest.execution_provider_plugin_library_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $runtimeOut $package.Plugin.Library)).Hash.ToLowerInvariant()
    }
    $manifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $outputPath "runtime-manifest.json") -Encoding utf8NoBOM
    Write-Output $outputPath
}
finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
