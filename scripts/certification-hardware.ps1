#Requires -Version 7.0
$ErrorActionPreference = "Stop"

$os = Get-CimInstance Win32_OperatingSystem
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
[ordered]@{
    os = "Windows"
    os_version = $os.Version
    os_build = $os.BuildNumber
    architecture = $env:PROCESSOR_ARCHITECTURE
    cpu = $cpu.Name.Trim()
} | ConvertTo-Json -Compress

Get-CimInstance Win32_VideoController | ForEach-Object {
    [ordered]@{
        class = "gpu"
        name = $_.Name
        vendor = $_.AdapterCompatibility
        driver_version = $_.DriverVersion
    } | ConvertTo-Json -Compress
}

# Friendly names and status are sufficient for an NPU certificate. Do not
# emit PnP instance IDs, PCI addresses, UUIDs, serials or unrelated devices.
Get-PnpDevice -Class ComputeAccelerator -ErrorAction SilentlyContinue | ForEach-Object {
    [ordered]@{
        class = "compute_accelerator"
        name = $_.FriendlyName
        status = $_.Status
    } | ConvertTo-Json -Compress
}
