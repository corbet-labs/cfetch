# bench-speed.ps1 — per-tool speed benchmark on a REAL repo clone.
# Measures: first index, cached re-index, query latency (p50/p95), state size.
#
# Usage (one call per arm x repo; run on ONE idle machine, sequentially):
#   .\bench-speed.ps1 -Arm cfetch   -RepoSrc C:\repos\lodash -Terms chunk,debounce,isEqual -RepoName lodash
#   .\bench-speed.ps1 -Arm openwolf -RepoSrc C:\repos\lodash -Terms chunk,debounce,isEqual -RepoName lodash
# Appends rows to ..\results\speed.csv
param(
    [ValidateSet("cfetch", "openwolf")][string]$Arm,
    [string]$RepoSrc,
    [string]$RepoName = (Split-Path $RepoSrc -Leaf),
    [string[]]$Terms = @("test"),
    [int]$QueryRuns = 20,
    [string]$Mode = "find"   # find = code search; recall = memory search
)
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
$res = Join-Path $root "results"
New-Item -ItemType Directory -Force -Path $res | Out-Null
$csv = Join-Path $res "speed.csv"
if (-not (Test-Path $csv)) {
    "arm,repo,metric,value,unit" | Set-Content $csv
}

# isolated env per arm (never touch your real brain / .wolf)
$work = Join-Path $root ("work\speed\" + $Arm + "\" + $RepoName)
if (Test-Path $work) { Remove-Item $work -Recurse -Force }
$repo = Join-Path $work "repo"
$brain = Join-Path $work "brain"
$home_ = Join-Path $work "home"
New-Item -ItemType Directory -Force -Path $repo, $brain, $home_ | Out-Null
Copy-Item $RepoSrc $repo -Recurse
$env:USERPROFILE = $home_
$env:HOME = $home_
$env:CFETCH_BRAIN = $brain
$env:CFETCH_STATE_DIR = Join-Path $work "cfetch-state"

function Timed([scriptblock]$b) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    & $b 2>&1 | Out-Null
    $sw.Stop()
    return $sw.Elapsed.TotalSeconds
}
function Pct([double[]]$v, [double]$p) {
    $s = $v | Sort-Object
    $i = [Math]::Min([int][Math]::Ceiling($p * $s.Count), $s.Count) - 1
    return [Math]::Round($s[[Math]::Max($i, 0)] * 1000, 1)
}

if ($Arm -eq "cfetch") {
    # Machine-layer config (CFETCH_CONFIG is the fully trusted layer):
    # index this one repo, inject nothing, keep it quiet.
    $cfg = Join-Path $work "machine-config.json"
    ('{{"resident": [], "code_roots": ["{0}"], "capture": {{"enabled": false}}}}' -f ($repo -replace "\\", "/")) | Set-Content $cfg
    $env:CFETCH_CONFIG = $cfg
    $t0 = Timed { cfetch scan }
    $t1 = Timed { cfetch scan }
    Add-Content $csv "$Arm,$RepoName,first_scan,$([Math]::Round($t0,2)),s"
    Add-Content $csv "$Arm,$RepoName,cached_scan,$([Math]::Round($t1,2)),s"
} else {
    Push-Location $repo
    $ow = if (Get-Command openwolf -ErrorAction SilentlyContinue) { "openwolf" } else { "npx openwolf-enhanced" }
    $t0 = Timed { & $ow init }
    $t1 = Timed { & $ow scan }
    $t2 = Timed { & $ow scan }
    Add-Content $csv "$Arm,$RepoName,first_scan,$([Math]::Round($t0 + $t1, 2)),s"
    Add-Content $csv "$Arm,$RepoName,cached_scan,$([Math]::Round($t2,2)),s"
    Pop-Location
}

# query latency: N runs per term, median of each batch -> p50/p95 across batches
if ($Arm -eq "cfetch") { $cmdFind = "cfetch $Mode" } else { $ow2 = if (Get-Command openwolf -ErrorAction SilentlyContinue) { "openwolf" } else { "npx openwolf-enhanced" }; $cmdFind = "$ow2 $Mode" }
foreach ($term in $Terms) {
    $batch = @()
    for ($i = 0; $i -lt $QueryRuns; $i++) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        cmd /c "$cmdFind `"$term`" >nul 2>nul"
        $sw.Stop()
        $batch += $sw.Elapsed.TotalSeconds
    }
    $ms = [double[]]($batch | ForEach-Object { $_ })
    Add-Content $csv "$Arm,$RepoName,$($Mode)_p50_$term,$(Pct $ms 0.5),ms"
    Add-Content $csv "$Arm,$RepoName,$($Mode)_p95_$term,$(Pct $ms 0.95),ms"
}

# state footprint: everything the tool wrote outside the repo itself
$stateDirs = if ($Arm -eq "cfetch") { @($brain, $env:CFETCH_STATE_DIR) } else { @(Join-Path $repo ".wolf") }
$mb = 0.0
foreach ($d in $stateDirs) {
    if (Test-Path $d) {
        $mb += ((Get-ChildItem $d -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum / 1MB)
    }
}
Add-Content $csv "$Arm,$RepoName,state_size,$([Math]::Round($mb, 1)),MB"
Write-Host "appended $Arm / $RepoName rows to $csv"
