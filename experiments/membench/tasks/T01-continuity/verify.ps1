
param([string]$RepoDir)
Set-Location $RepoDir
$tests = node --test --test-reporter=tap 2>&1 | Out-String
$pass = ($LASTEXITCODE -eq 0)
$norm = (Select-String -Path "src/normalize.js" -Pattern "normalizeUrgency" -Quiet)
$inline = (Select-String -Path "src/task.js" -Pattern "u1|u3|RangeError" -Quiet)
[pscustomobject]@{
  tests_pass = $pass; normalizer_added = [bool]$norm; inline_check_leaked = [bool]$inline;
  tap = ($tests -split [char]10 | Select-String "^# (pass|fail)" | ForEach-Object { $_.Line }) -join "; "
} | ConvertTo-Json | Set-Content "$RepoDir/../verdict.json"
Write-Output $tests
