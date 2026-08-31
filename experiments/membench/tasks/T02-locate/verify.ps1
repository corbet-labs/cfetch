
param([string]$RepoDir)
$a = Join-Path $RepoDir "answers.md"
if (-not (Test-Path $a)) {
  [pscustomobject]@{ score = 0; reason = "answers.md missing" } | ConvertTo-Json | Set-Content "$RepoDir/../verdict.json"; exit 0
}
$text = Get-Content $a -Raw
$golden = @("roleFor", "priceFor", "api.test", "truthiness", "falsy", "implicit", "null")
$hits = @($golden | Where-Object { $text -match [regex]::Escape($_) })
[pscustomobject]@{ score = $hits.Count; of = $golden.Count; hits = $hits } | ConvertTo-Json | Set-Content "$RepoDir/../verdict.json"
Write-Output "score: $($hits.Count)/$($golden.Count)"
