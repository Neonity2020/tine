param(
  [Parameter(Mandatory = $true)]
  [int]$ProcessId,

  [Parameter(Mandatory = $true)]
  [string]$ExpectedText,

  [int]$TimeoutSeconds = 20
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

$root = [System.Windows.Automation.AutomationElement]::RootElement
$processCondition = [System.Windows.Automation.PropertyCondition]::new(
  [System.Windows.Automation.AutomationElement]::ProcessIdProperty,
  $ProcessId
)
$windowCondition = [System.Windows.Automation.PropertyCondition]::new(
  [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
  [System.Windows.Automation.ControlType]::Window
)
$buttonCondition = [System.Windows.Automation.PropertyCondition]::new(
  [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
  [System.Windows.Automation.ControlType]::Button
)
$deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)

while ([DateTime]::UtcNow -lt $deadline) {
  $windows = $root.FindAll(
    [System.Windows.Automation.TreeScope]::Children,
    ([System.Windows.Automation.AndCondition]::new($processCondition, $windowCondition))
  )
  foreach ($window in $windows) {
    $names = $window.FindAll(
      [System.Windows.Automation.TreeScope]::Descendants,
      [System.Windows.Automation.Condition]::TrueCondition
    ) | ForEach-Object { $_.Current.Name } | Where-Object { $_ }
    if (-not (($names -join "`n").Contains($ExpectedText))) {
      continue
    }

    $buttons = $window.FindAll(
      [System.Windows.Automation.TreeScope]::Descendants,
      $buttonCondition
    )
    $affirmative = $null
    foreach ($button in $buttons) {
      $isDefault = [bool]$button.GetCurrentPropertyValue(
        [System.Windows.Automation.AutomationElement]::IsDefaultProperty
      )
      if ($isDefault) {
        $affirmative = $button
        break
      }
      if ($button.Current.Name -match '^(Yes|OK|&Yes)$') {
        $affirmative = $button
      }
    }
    if ($null -eq $affirmative) {
      throw "Tine confirmation dialog was found, but it had no affirmative button"
    }
    $invoke = $affirmative.GetCurrentPattern(
      [System.Windows.Automation.InvokePattern]::Pattern
    )
    $invoke.Invoke()
    Write-Output "accepted Tine confirmation: $ExpectedText"
    exit 0
  }
  Start-Sleep -Milliseconds 100
}

throw "Tine confirmation dialog did not appear for process $ProcessId within $TimeoutSeconds seconds: $ExpectedText"
