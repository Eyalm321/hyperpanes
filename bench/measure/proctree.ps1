# Walk the process tree rooted at -RootPid and report per-process memory.
#
#   powershell -File proctree.ps1 -RootPid <pid>
#
# Builds a child map from Win32_Process (ParentProcessId), BFS-collects the
# subtree including the root, then reads accurate memory via Get-Process
# (WorkingSet64 + PrivateMemorySize64, both bytes). Emits {processes:[...]} JSON.
# Memory for a process that exits mid-walk is reported as 0 rather than failing.

param([Parameter(Mandatory = $true)][int]$RootPid)

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

$result = foreach ($t in $tree) {
  $ws = [int64]0
  $pb = [int64]0
  try {
    $gp = Get-Process -Id ([int]$t.ProcessId) -ErrorAction Stop
    $ws = [int64]$gp.WorkingSet64
    $pb = [int64]$gp.PrivateMemorySize64
  } catch {
    $ws = [int64]0
    $pb = [int64]0
  }
  [pscustomobject]@{
    pid          = [int]$t.ProcessId
    name         = [string]$t.Name
    workingSet   = $ws
    privateBytes = $pb
  }
}

# Wrap so a single-element result still serializes as an array on the JS side.
[pscustomobject]@{ rootPid = $RootPid; processes = @($result) } | ConvertTo-Json -Depth 4 -Compress
