# membench runner. Arms x Tasks x Repeats, fresh checkout each run.
# Usage:
#   .\run-battery.ps1 -Arms baseline,openwolf,cfetch -Tasks T01-continuity -Repeats 3
#   .\run-battery.ps1 -DryRun ...   (harness smoke test, no agent calls)
param(
    [string[]]$Arms = @("baseline", "agentsmd", "openwolf", "cfetch"),
    [string[]]$Tasks = @("T01-continuity", "T02-locate", "T03-bugfix", "T04-staleness"),
    [int]$Repeats = 3,
    [int]$BurnIn = 1,
    # proxy = LiteLLM counts tokens (needs API key). claude = parse the usage
    # block from `claude -p --output-format json` (works with a Pro/Max
    # subscription login; no proxy needed).
    [ValidateSet("proxy", "claude")][string]$UsageSource = "proxy",
    [switch]$DryRun
)
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
# [string[]] binds "-Arms a,b" as a clean array; quoted "a,b" lands as one
# element and is split on commas here. Handles both CLI forms identically.
$Arms = @($Arms | ForEach-Object { $_ -split "," } | ForEach-Object { $_.Trim() } | Where-Object { $_ })
$Tasks = @($Tasks | ForEach-Object { $_ -split "," } | ForEach-Object { $_.Trim() } | Where-Object { $_ })
$results = Join-Path $root "results"
New-Item -ItemType Directory -Force -Path $results | Out-Null
$csv = Join-Path $results "battery.csv"
if (-not (Test-Path $csv)) {
    "arm,task,repeat,phase,tests_pass,score,poisoned,normalizer,tokens_in,tokens_out,wall_s" | Set-Content $csv
}

# --- usage counting from the proxy log (time-window match) -----------------
function Get-Tokens([double]$t0, [double]$t1) {
    $log = Join-Path $root "results\usage.jsonl"
    if (-not (Test-Path $log)) { return @{ in_ = 0; out_ = 0 } }
    $in_ = 0; $out_ = 0
    foreach ($line in Get-Content $log) {
        try { $r = $line | ConvertFrom-Json } catch { continue }
        if ($r.ts -ge $t0 -and $r.ts -le $t1) { $in_ += $r.input_tokens; $out_ += $r.output_tokens }
    }
    return @{ in_ = $in_; out_ = $out_ }
}

# --- one session: prepare repo, invoke agent, verify, record ---------------
function Invoke-Session([string]$arm, [string]$task, [int]$rep, [string]$phase, [string]$promptFile) {
    $seed = Join-Path $root "tasks\$task\seed"
    $work = Join-Path $root "work\$arm\$task\r$rep\repo"
    if (Test-Path (Split-Path -Parent $work)) { Remove-Item (Split-Path -Parent $work) -Recurse -Force }
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $work) | Out-Null
    Copy-Item $seed $work -Recurse
    Push-Location $work
    # A real git repo per run: verify scripts diff against it (was the test
    # suite modified?) and the runner can archive agent diffs later. autocrlf
    # is forced off per-repo so global machine config cannot flood stderr
    # (fatal under EAP=Stop); git runs through cmd to swallow its chatter.
    cmd /c "git init -q 2>nul & git config core.autocrlf false & git config user.name bench & git config user.email bench@local & git add -A 2>nul & git commit -qm seed 2>nul"
    if ($LASTEXITCODE -ne 0) { Write-Host "(seed git init skipped)" }

    # Per-arm setup. Three isolation layers so the memory tools cannot see
    # each other: CLAUDE_CONFIG_DIR (hooks/settings), a per-arm USERPROFILE
    # (global tool state: openwolf's machine registry, cfetch's default brain
    # root), and CFETCH_STATE_DIR (cfetch index/grants) under the work dir.
    $armRoot = Join-Path $root "work\$arm"
    $cfgDir = Join-Path $armRoot "claude-config"
    $homeDir = Join-Path $armRoot "home"
    New-Item -ItemType Directory -Force -Path $cfgDir, $homeDir | Out-Null
    $env:CLAUDE_CONFIG_DIR = $cfgDir
    $env:USERPROFILE = $homeDir
    $env:HOME = $homeDir
    $env:CFETCH_STATE_DIR = Join-Path $armRoot "cfetch-state"
    $env:ANTHROPIC_BASE_URL = "http://127.0.0.1:4000"
    $env:ANTHROPIC_AUTH_TOKEN = "sk-bench-master"
    $env:ANTHROPIC_MODEL = "claude-bench"
    if ($UsageSource -eq "claude") {
        # Subscription login: let claude talk to Anthropic directly.
        Remove-Item Env:\ANTHROPIC_BASE_URL, Env:\ANTHROPIC_AUTH_TOKEN, Env:\ANTHROPIC_MODEL -ErrorAction SilentlyContinue
    }
    switch ($arm) {
        "baseline" { }
        "agentsmd" { Copy-Item (Join-Path $root "arms\AGENTS.md") "./AGENTS.md" -ErrorAction SilentlyContinue }
        "openwolf" { if (-not $DryRun) { npx openwolf-enhanced init 2>&1 | Out-Null } }
        "cfetch"   { if (-not $DryRun) { cfetch init 2>&1 | Out-Null } }
    }

    $prompt = Get-Content (Join-Path $root "tasks\$task\$promptFile") -Raw
    $t0 = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $claudeUsage = $null
    if ($DryRun) {
        Write-Host ("[dry-run] {0}/{1}/r{2}/{3} would run: {4}..." -f $arm, $task, $rep, $phase, $prompt.Substring(0, [Math]::Min(70, $prompt.Length)).Replace("`r", "").Replace("`n", " "))
        Start-Sleep -Milliseconds 200
    } else {
        # --permission-mode acceptEdits lets the agent edit files without interactive prompts.
        # --output-format json makes the final message a result object whose
        # usage block is the billed source of truth (same numbers a proxy would count).
        $jsonOut = claude -p $prompt --permission-mode acceptEdits --output-format json 2>$null | Out-String
        $jsonOut | Set-Content (Join-Path (Split-Path -Parent $work) "agent-$phase.json")
        try {
            $result = $jsonOut | ConvertFrom-Json
            $claudeUsage = $result.usage
        } catch { $claudeUsage = $null }
    }
    $t1 = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    if ($UsageSource -eq "claude" -and $claudeUsage) {
        $cacheRead = 0
        if ($claudeUsage.PSObject.Properties["cache_read_input_tokens"]) {
            $cacheRead = [int]$claudeUsage.cache_read_input_tokens
        }
        $tok = @{
            in_  = [int]$claudeUsage.input_tokens + $cacheRead
            out_ = [int]$claudeUsage.output_tokens
        }
    } else {
        $tok = Get-Tokens $t0 $t1
    }

    # verify only in the final phase
    $row = "$arm,$task,$rep,$phase"
    if ($phase -eq "measure") {
        & (Join-Path $root "tasks\$task\verify.ps1") -RepoDir $work 2>&1 | Out-Null
        $vPath = Join-Path (Split-Path -Parent $work) "verdict.json"
        $v = if (Test-Path $vPath) { Get-Content $vPath -Raw | ConvertFrom-Json } else { $null }
        $row += ",$($v.tests_pass),$($v.score),$($v.poisoned_phone_required),$($v.normalizer_added)"
    } else {
        $row += ",,,,,"
    }
    $row += ",$($tok.in_),$($tok.out_),$([Math]::Round(($t1 - $t0) / 1000.0, 1))"
    Add-Content $csv $row
    Pop-Location
}

# --- battery ----------------------------------------------------------------
foreach ($arm in $Arms) {
    foreach ($task in $Tasks) {
        $hasPlant = Test-Path (Join-Path $root "tasks\$task\plant.md")
        for ($rep = 1; $rep -le ($Repeats + $BurnIn); $rep++) {
            $tag = if ($rep -le $BurnIn) { "burn-in" } else { "rep $($rep - $BurnIn)" }
            Write-Host "== $arm / $task / $tag =="
            if ($hasPlant) {
                Invoke-Session $arm $task $rep "seed" "plant.md"      # session 1: teach
                Invoke-Session $arm $task $rep "measure" "task.md"    # session 2: apply
            } else {
                Invoke-Session $arm $task $rep "measure" "task.md"
            }
        }
    }
}
Write-Host "done. results: $csv"
