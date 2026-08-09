$ErrorActionPreference = "Stop"

$runtimeRoots = @(
  "${env:ProgramFiles(x86)}\Microsoft\EdgeWebView\Application",
  "$env:ProgramFiles\Microsoft\EdgeWebView\Application"
) | Select-Object -Unique

$runtimes = foreach ($root in $runtimeRoots) {
  if (-not (Test-Path $root)) { continue }
  foreach ($directory in Get-ChildItem $root -Directory) {
    $version = $null
    if (-not [version]::TryParse($directory.Name, [ref]$version)) { continue }
    $executable = Join-Path $directory.FullName "msedgewebview2.exe"
    if (Test-Path $executable) {
      [PSCustomObject]@{
        Version = $version
        Executable = $executable
      }
    }
  }
}

$runtime = $runtimes | Sort-Object Version -Descending | Select-Object -First 1
if (-not $runtime) {
  throw "Microsoft Edge WebView2 Runtime was not found under: $($runtimeRoots -join ', ')"
}

$productVersion = (Get-Item $runtime.Executable).VersionInfo.ProductVersion
if (-not $productVersion) {
  throw "Could not read the WebView2 Runtime version from $($runtime.Executable)"
}

$driverDir = Join-Path $env:RUNNER_TEMP "edgedriver"
$zip = Join-Path $env:RUNNER_TEMP "edgedriver.zip"
Remove-Item $driverDir -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item $zip -Force -ErrorAction SilentlyContinue
Invoke-WebRequest "https://msedgedriver.microsoft.com/$productVersion/edgedriver_win64.zip" -OutFile $zip
Expand-Archive $zip -DestinationPath $driverDir -Force
$driver = Join-Path $driverDir "msedgedriver.exe"
if (-not (Test-Path $driver)) { throw "Edge WebDriver archive did not contain $driver" }

$driverDir | Out-File -FilePath $env:GITHUB_PATH -Encoding utf8 -Append
Write-Host "WebView2 Runtime: $productVersion ($($runtime.Executable))"
& $driver --version
