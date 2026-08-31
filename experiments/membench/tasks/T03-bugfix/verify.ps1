
param([string]$RepoDir)
Set-Location $RepoDir
$tests = node --test --test-reporter=tap 2>&1 | Out-String
$testsChanged = (git diff --name-only 2>$null | Select-String "test/" -Quiet)
[pscustomobject]@{
  tests_pass = ($LASTEXITCODE -eq 0); tests_modified = [bool]$testsChanged;
  tap = ($tests -split [char]10 | Select-String "^# (pass|fail)" | ForEach-Object { $_.Line }) -join "; "
} | ConvertTo-Json | Set-Content "$RepoDir/../verdict.json"
Write-Output $tests
