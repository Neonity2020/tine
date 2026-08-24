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
$observedButtons = @()

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
      $name = $button.Current.Name
      $automationId = $button.Current.AutomationId
      $observedButtons += "name='$name' automation_id='$automationId'"
      # Native Windows message boxes expose stable command IDs even when their
      # visible labels are localized or include an accelerator marker:
      # IDOK=1 and IDYES=6. Keep the label match for non-Win32 dialog hosts.
      if ($automationId -in @('1', '6') -or $name -match '^(?i:&?Yes|OK)$') {
        $affirmative = $button
        break
      }
    }
    if ($null -eq $affirmative) {
      # UI Automation can expose the dialog window one tick before its child
      # buttons. Keep polling instead of turning that race into a test failure.
      continue
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

$observed = ($observedButtons | Select-Object -Unique) -join '; '
throw "Tine confirmation dialog did not expose an affirmative button for process $ProcessId within $TimeoutSeconds seconds: $ExpectedText; observed buttons: $observed"
