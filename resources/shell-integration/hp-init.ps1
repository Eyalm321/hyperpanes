# Hyperpanes shell integration (PowerShell / pwsh).
# Dot-sourced into an interactive pane AFTER the user's $PROFILE has loaded.
# Strictly ADDITIVE: every step is guarded so any failure leaves a normal shell.

# 1) OSC 7 cwd reporting -----------------------------------------------------------
# Wrap `prompt` exactly once (idempotent across re-sourcing) so every prompt emits
# the live working directory as ESC ] 7 ; <file-uri> BEL. We keep the existing
# prompt (the user's, or the default) and call it after emitting.
if (-not $global:__HyperpanesPromptWrapped) {
  $global:__HyperpanesPromptWrapped = $true
  $global:__HyperpanesInnerPrompt = $function:prompt

  function global:prompt {
    try {
      $p = (Get-Location).ProviderPath
      if ($p) {
        $uri = ([System.Uri]$p).AbsoluteUri
        $esc = [char]27
        $bel = [char]7
        Write-Host -NoNewline "$esc]7;$uri$bel"
      }
    } catch {}

    if ($global:__HyperpanesInnerPrompt) {
      & $global:__HyperpanesInnerPrompt
    } else {
      # Reproduce PowerShell's built-in default prompt.
      "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) "
    }
  }
}

# 2) History autocomplete (feature 3) ----------------------------------------------
# Inline grey "ghost text" from command history. Requires PSReadLine >= 2.2; older
# versions lack -PredictionViewStyle, so version-guard and swallow any error.
try {
  $psrl = Get-Module -Name PSReadLine
  if (-not $psrl) {
    $psrl = Get-Module -ListAvailable -Name PSReadLine | Sort-Object Version -Descending | Select-Object -First 1
  }
  if ($psrl -and $psrl.Version -ge [version]'2.2.0') {
    Set-PSReadLineOption -PredictionSource History -ErrorAction SilentlyContinue
    Set-PSReadLineOption -PredictionViewStyle InlineView -ErrorAction SilentlyContinue
  }
} catch {}
