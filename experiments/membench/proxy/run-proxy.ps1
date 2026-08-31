# Native (no-Docker) proxy for local benchmarking: pip-installed LiteLLM.
# One-time:  pip install "litellm[proxy]"
# Run:       .\run-proxy.ps1        (from membench\proxy; needs ANTHROPIC_API_KEY)
# Then point the runner at it (it already is): http://127.0.0.1:4000
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $env:ANTHROPIC_API_KEY) {
    Write-Error "ANTHROPIC_API_KEY is not set - the proxy needs it to reach the real API."
}
# The proxy banner is full of Unicode; a CP1252 console kills startup with
# UnicodeEncodeError unless Python is forced to UTF-8.
$env:PYTHONUTF8 = "1"
$env:PYTHONIOENCODING = "utf-8"
Push-Location $here
# custom_callbacks.py must be importable from the working directory for the
# callbacks setting in litellm-config.yaml to resolve.
litellm --config litellm-config.yaml --port 4000 --host 127.0.0.1
Pop-Location
