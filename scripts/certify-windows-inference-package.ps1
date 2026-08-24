#Requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Package,

    [Parameter(Mandatory = $true)]
    [string]$Bundle,

    [Parameter(Mandatory = $true)]
    [string]$Report,

    [string]$BundleSha256 = "12892e4fb2dea4e60adc03669f32dcee2813d2764c8bf6c25ecf6b95aa5756b1",

    [ValidateSet("X1", "X2")]
    [string]$VitisTarget = "X2",

    [string]$VitisXclbin
)

$ErrorActionPreference = "Stop"
$packagePath = (Resolve-Path -LiteralPath $Package).Path
$bundlePath = (Resolve-Path -LiteralPath $Bundle).Path
$reportPath = if ([IO.Path]::IsPathFullyQualified($Report)) {
    [IO.Path]::GetFullPath($Report)
}
else {
    [IO.Path]::GetFullPath((Join-Path (Get-Location) $Report))
}
$manifestPath = Join-Path $packagePath "runtime-manifest.json"
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ($manifest.schema_version -ne 1 -or $manifest.provider -notin @(
        "directml", "qnn", "vitis", "openvino-cpu", "openvino-gpu", "openvino-npu"
    )) {
    throw "unsupported Windows inference package manifest"
}
if ($manifest.os -ne "windows" -or $manifest.arch -notin @("x64", "arm64")) {
    throw "Windows inference package manifest has an invalid platform"
}
$actualArch = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x64" }
    "ARM64" { "arm64" }
    default { throw "unsupported Windows architecture: $env:PROCESSOR_ARCHITECTURE" }
}
if ($manifest.arch -ne $actualArch) {
    throw "Windows inference package arch $($manifest.arch) does not match native $actualArch"
}
$publishedRuntimes = @{
    directml = @{
        Distribution = "nuget-Microsoft.ML.OnnxRuntime.DirectML-1.24.4"
        Sha256 = "57e9f11b73437bef7a309496135d4c1f96b1a8e9ddba60013fa27bfc1d788681"
    }
    qnn = @{
        Distribution = "nuget-Microsoft.ML.OnnxRuntime.QNN-1.24.4"
        Sha256 = "e4d6eabb9e503d4f3c78494fc9400f02509b2ee315d9f707644a174ece8da17f"
    }
    "openvino-cpu" = @{
        Distribution = "nuget-Intel.ML.OnnxRuntime.OpenVino-1.24.1"
        Sha256 = "f53ad5f90e3d616970a5c65e4880ebbe92c9774e9727020661db591cea74a110"
    }
    "openvino-gpu" = @{
        Distribution = "nuget-Intel.ML.OnnxRuntime.OpenVino-1.24.1"
        Sha256 = "f53ad5f90e3d616970a5c65e4880ebbe92c9774e9727020661db591cea74a110"
    }
    "openvino-npu" = @{
        Distribution = "nuget-Intel.ML.OnnxRuntime.OpenVino-1.24.1"
        Sha256 = "f53ad5f90e3d616970a5c65e4880ebbe92c9774e9727020661db591cea74a110"
    }
}
if ($publishedRuntimes.ContainsKey($manifest.provider)) {
    $published = $publishedRuntimes[$manifest.provider]
    if ($manifest.onnxruntime_distribution -ne $published.Distribution -or
        $manifest.onnxruntime_archive_sha256 -ne $published.Sha256) {
        throw "Windows package does not use the published pinned $($manifest.provider) runtime"
    }
}

$manifestPaths = @($manifest.files.path | Sort-Object -Unique)
if ($manifestPaths.Count -ne @($manifest.files).Count) {
    throw "Windows inference package manifest contains duplicate paths"
}
$actualPaths = @(Get-ChildItem -LiteralPath $packagePath -File -Recurse | ForEach-Object {
    [IO.Path]::GetRelativePath($packagePath, $_.FullName).Replace("\", "/")
} | Where-Object { $_ -ne "runtime-manifest.json" } | Sort-Object -Unique)
if (@(Compare-Object -ReferenceObject $manifestPaths -DifferenceObject $actualPaths).Count -ne 0) {
    throw "Windows inference package file set differs from its manifest"
}

foreach ($file in $manifest.files) {
    $path = [IO.Path]::GetFullPath((Join-Path $packagePath $file.path))
    $packagePrefix = $packagePath.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if (-not $path.StartsWith($packagePrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "package manifest path escapes its root: $($file.path)"
    }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
    if ($actual -ne $file.sha256) {
        throw "package file hash mismatch: $($file.path)"
    }
}
$actualBundleHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $bundlePath).Hash.ToLowerInvariant()
if ($actualBundleHash -ne $BundleSha256) {
    throw "model bundle SHA-256 $actualBundleHash does not match $BundleSha256"
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("cfetch-windows-certify-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null
try {
    & tar.exe -xzf $bundlePath -C $work
    if ($LASTEXITCODE -ne 0) {
        throw "failed to extract the verified model bundle"
    }
    $model = Join-Path $work "cfetch-embeddinggemma-300m-a8w8-v1"
    if (-not (Test-Path -LiteralPath (Join-Path $model "v1-artifact-lock.json"))) {
        throw "verified bundle contains no frozen v1 model root"
    }

    $runtime = Join-Path $packagePath "runtime"
    $env:ORT_DYLIB_PATH = Join-Path $runtime "onnxruntime.dll"
    $env:PATH = "$runtime;$env:PATH"
    if ($manifest.provider -eq "qnn") {
        $env:CFETCH_QNN_HTP_LIBRARY = Join-Path $runtime "QnnHtp.dll"
    }
    if ($manifest.provider -eq "vitis") {
        $env:CFETCH_VITIS_TARGET = $VitisTarget
        if ($VitisTarget -eq "X1") {
            if ([string]::IsNullOrWhiteSpace($VitisXclbin)) {
                $VitisXclbin = Join-Path $runtime "xclbins/phoenix/4x4.xclbin"
            }
            $env:CFETCH_VITIS_XCLBIN = (Resolve-Path -LiteralPath $VitisXclbin).Path
        }
        elseif (-not [string]::IsNullOrWhiteSpace($VitisXclbin)) {
            throw "XDNA2 target X2 must not receive -VitisXclbin"
        }
        else {
            Remove-Item Env:CFETCH_VITIS_XCLBIN -ErrorAction SilentlyContinue
        }
        $vitisCache = Join-Path $work "vitis-cache"
        New-Item -ItemType Directory -Path $vitisCache | Out-Null
        $env:CFETCH_VITIS_REPORT_DIR = $vitisCache
        $env:XLNX_ONNX_EP_REPORT_FILE = "vitisai_ep_report.json"
    }

    $binary = Join-Path $packagePath "cfetch.exe"
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $binary
    $start.UseShellExecute = $false
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    foreach ($argument in @("inference-certify", "--model-dir", $model, "--provider", $manifest.provider, "--json")) {
        $start.ArgumentList.Add($argument)
    }
    $process = [Diagnostics.Process]::Start($start)
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    $process.WaitForExit()
    [IO.File]::WriteAllText($reportPath, $stdout.Result, [Text.UTF8Encoding]::new($false))
    $vitisReports = @()
    if ($manifest.provider -eq "vitis") {
        $placementPath = "$reportPath.vitis-placement"
        $vitisReports = @(Get-ChildItem -LiteralPath $vitisCache -Filter "vitisai_ep_report.json" -File -Recurse)
        if ($vitisReports.Count -gt 0) {
            New-Item -ItemType Directory -Path $placementPath | Out-Null
            foreach ($placement in $vitisReports) {
                $cacheKey = $placement.Directory.Name
                Copy-Item -LiteralPath $placement.FullName -Destination (Join-Path $placementPath "$cacheKey.json")
            }
        }
    }
    if ($process.ExitCode -ne 0) {
        [Console]::Error.Write($stderr.Result)
        throw "cfetch inference certification exited $($process.ExitCode)"
    }

    $certificate = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json
    $passed = @($certificate.known_answers | Where-Object { $_.passed }).Count
    if ($certificate.profile_id -ne "cfetch-embedding-v1" -or
        $certificate.provider -ne $manifest.provider -or
        $certificate.os -ne "windows" -or
        $certificate.onnxruntime_distribution -ne $manifest.onnxruntime_distribution -or
        $certificate.onnxruntime_archive_sha256 -ne $manifest.onnxruntime_archive_sha256 -or
        $certificate.onnxruntime_library_sha256 -ne $manifest.onnxruntime_library_sha256 -or
        -not $certificate.cpu_fallback_disabled -or
        -not $certificate.graph_ownership_enforced -or
        -not $certificate.exact_vector_conformance -or
        @($certificate.known_answers).Count -ne 11 -or
        $passed -ne 11) {
        throw "Windows provider failed exact v1 certification; report preserved at $reportPath"
    }
    if ($manifest.provider -eq "vitis") {
        $bucketCount = @($certificate.known_answers.sequence_bucket | Sort-Object -Unique).Count
        if ($vitisReports.Count -lt $bucketCount) {
            throw "Vitis produced $($vitisReports.Count) placement reports for $bucketCount exercised static buckets"
        }
    }
    Write-Output "CFETCH_INFERENCE_CERTIFICATION_OK provider=$($manifest.provider) report=$reportPath"
}
finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
