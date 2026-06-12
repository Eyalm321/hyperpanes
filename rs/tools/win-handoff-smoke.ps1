# Windows second-instance handoff smoke: isolated primary + .hyperpanes secondary.
$iso = "$env:TEMP\hp-ho-win"
Remove-Item -Recurse -Force $iso -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$iso\hyperpanes" | Out-Null
Set-Content "$iso\hyperpanes\control-settings.json" '{ "enabled": true, "allowInput": false }'
Set-Content "$iso\ho.hyperpanes" '{"format":"hyperpanes","version":1,"workspace":{"groups":[{"title":"win-ho","panes":[{"label":"ho"}]}]}}'
$env:APPDATA = $iso
Remove-Item Env:HYPERPANES_CONTROL_FILE -ErrorAction SilentlyContinue
Remove-Item Env:HYPERPANES_PANE_ID -ErrorAction SilentlyContinue
$bin = "$PSScriptRoot\..\crates\app\target\debug\hyperpanes.exe"
$p = Start-Process -FilePath $bin -PassThru
Start-Sleep 8
$p2 = Start-Process -FilePath $bin -ArgumentList "$iso\ho.hyperpanes" -PassThru
$exited = $p2.WaitForExit(15000)
Start-Sleep 3
$c = Get-Content "$iso\hyperpanes\control.json" | ConvertFrom-Json
$h = @{ Authorization = "Bearer $($c.token)" }
$s = Invoke-RestMethod -Uri ("http://127.0.0.1:" + $c.port + "/state") -Headers $h
$titles = ($s.windows | ForEach-Object { $_.tabs } | ForEach-Object { $_.title }) -join ', '
Write-Output "secondary exited cleanly: $exited (rc=$($p2.ExitCode))"
Write-Output "tabs after handoff: $titles"
Stop-Process -Id $p.Id -ErrorAction SilentlyContinue
if ($exited -and $titles -match 'win-ho') { Write-Output 'WIN HANDOFF: PASS' } else { Write-Output 'WIN HANDOFF: FAIL' }
