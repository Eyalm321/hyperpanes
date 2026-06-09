# Walk the process tree rooted at -RootPid and report per-process memory (and optionally CPU).
#
#   powershell -File proctree.ps1 -RootPid <pid> [-CpuMs <interval>]
#
# Builds a child map from Win32_Process (ParentProcessId), BFS-collects the
# subtree including the root, then reads accurate memory via Get-Process
# (WorkingSet64 + PrivateMemorySize64, both bytes). Emits {processes:[...]} JSON.
# Memory for a process that exits mid-walk is reported as 0 rather than failing.
#
# When -CpuMs > 0, it also measures CPU: it snapshots each process's TotalProcessorTime,
# sleeps -CpuMs, then snapshots again; cpuPercent = deltaCpuMs / CpuMs * 100 (percent of ONE
# core — the JS side sums the tree, so a fully-busy single thread reads ~100). Idle reads ~0.
# Memory is read at the SECOND snapshot so it reflects the settled tree.

param(
  [Parameter(Mandatory = $true)][int]$RootPid,
  [int]$CpuMs = 0
)

$ErrorActionPreference = 'Stop'

$all = Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, Name

$byParent = @{}
foreach ($p in $all) {
  $key = [int]$p.ParentProcessId
  if (-not $byParent.ContainsKey($key)) { $byParent[$key] = New-Object System.Collections.ArrayList }
  [void]$byParent[$key].Add($p)
}

$root = $all | Where-Object { $_.ProcessId -eq $RootPid } | Select-Object -First 1
$tree = New-Object System.Collections.ArrayList
$seen = @{}
if ($root) {
  $queue = New-Object System.Collections.Queue
  $queue.Enqueue($root)
  while ($queue.Count -gt 0) {
    $cur = $queue.Dequeue()
    $id = [int]$cur.ProcessId
    if ($seen.ContainsKey($id)) { continue }
    $seen[$id] = $true
    [void]$tree.Add($cur)
    if ($byParent.ContainsKey($id)) {
      foreach ($c in $byParent[$id]) { $queue.Enqueue($c) }
    }
  }
}

# Optional CPU first-snapshot: cpu-time (ms) per pid before the sample window.
$cpu0 = @{}
$measureCpu = $CpuMs -gt 0
if ($measureCpu) {
  foreach ($t in $tree) {
    try {
      $gp = Get-Process -Id ([int]$t.ProcessId) -ErrorAction Stop
      # TotalProcessorTime can be $null for a protected/access-denied process even when
      # Get-Process itself succeeds; coercing that null to [double] yields 0.0 and would
      # make the delta count the WHOLE lifetime as in-window. Keep it $null so it is skipped.
      $tpt = $gp.TotalProcessorTime
      $cpu0[[int]$t.ProcessId] = if ($null -ne $tpt) { [double]$tpt.TotalMilliseconds } else { $null }
    } catch {
      $cpu0[[int]$t.ProcessId] = $null
    }
  }
  Start-Sleep -Milliseconds $CpuMs
}

$result = foreach ($t in $tree) {
  $ws = [int64]0
  $pb = [int64]0
  $cpuPct = [double]0
  try {
    $gp = Get-Process -Id ([int]$t.ProcessId) -ErrorAction Stop
    $ws = [int64]$gp.WorkingSet64
    $pb = [int64]$gp.PrivateMemorySize64
    if ($measureCpu) {
      $prev = $cpu0[[int]$t.ProcessId]
      $tpt = $gp.TotalProcessorTime
      if (($null -ne $prev) -and ($null -ne $tpt)) {
        $now = [double]$tpt.TotalMilliseconds
        $cpuPct = [Math]::Round((($now - $prev) / $CpuMs) * 100, 2)
        if ($cpuPct -lt 0) { $cpuPct = 0 }
      }
    }
  } catch {
    $ws = [int64]0
    $pb = [int64]0
    $cpuPct = [double]0
  }
  [pscustomobject]@{
    pid          = [int]$t.ProcessId
    name         = [string]$t.Name
    workingSet   = $ws
    privateBytes = $pb
    cpuPercent   = $cpuPct
  }
}

# Wrap so a single-element result still serializes as an array on the JS side.
[pscustomobject]@{ rootPid = $RootPid; cpuMs = $CpuMs; processes = @($result) } | ConvertTo-Json -Depth 4 -Compress
