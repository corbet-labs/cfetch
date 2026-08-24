#Requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("directml", "qnn")]
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
}
$package = $packages[$Provider]
if ($Provider -eq "qnn" -and $architecture -ne "arm64") {
    throw "the QNN HTP evidence package requires native Windows ARM64"
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

    $env:CFETCH_ORT_DISTRIBUTION = $package.Distribution
    $env:CFETCH_ORT_ARCHIVE_SHA256 = $package.Sha256
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
    Copy-Item -LiteralPath (Join-Path $root "LICENSE.md") -Destination (Join-Path $licenseOut "cfetch-LICENSE.md")
    Copy-Item -LiteralPath (Join-Path $root "THIRD-PARTY-LICENSES.txt") -Destination $licenseOut
    foreach ($notice in @("LICENSE", "ThirdPartyNotices.txt", "Privacy.md", "Qualcomm_LICENSE.pdf")) {
        $source = Join-Path $expanded $notice
        if (Test-Path -LiteralPath $source) {
            Copy-Item -LiteralPath $source -Destination $licenseOut
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
    $manifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $outputPath "runtime-manifest.json") -Encoding utf8NoBOM
    Write-Output $outputPath
}
finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
