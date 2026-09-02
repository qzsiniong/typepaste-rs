# typepaste pull (powershell) — 远程端反向传输辅助脚本。
# Usage:
#   powershell -File typepaste-pull.ps1 probe <path> <char_space>
#   powershell -File typepaste-pull.ps1 prepare <path> <chunk_bytes>
#   powershell -File typepaste-pull.ps1 show <index> <line_width> <char_space>
# char_space=1 表示每字符前加空格（提升 OCR 分割），0 表示关闭。
param([Parameter(Mandatory=$true)][string]$Action, [string]$Arg1, [string]$Arg2, [string]$Arg3)

function Probe-File([string]$path, [bool]$charSpace) {
  Clear-Host
  Write-Output ''
  $size = (Get-Item -LiteralPath $path).Length
  $md5 = (Get-FileHash -LiteralPath $path -Algorithm MD5).Hash.ToLower()
  $combined = "$size$md5"
  $checksum = ([BitConverter]::ToString([System.Security.Cryptography.MD5]::Create().ComputeHash([System.Text.Encoding]::UTF8.GetBytes($combined))).Replace('-', '').ToLower())
  if ($charSpace) {
    Write-Output (' ' + $size)
    Write-Output ($md5 -replace '(.)', ' $1')
    Write-Output ($checksum -replace '(.)', ' $1')
  } else {
    Write-Output $size
    Write-Output $md5
    Write-Output $checksum
  }
}

function Prepare-Chunks([string]$path, [int]$chunkBytes) {
  $b = [System.IO.File]::ReadAllBytes($path)
  $sb = New-Object System.Text.StringBuilder
  $md5 = [System.Security.Cryptography.MD5]::Create()
  for ($i = 0; $i -lt $b.Length; $i += $chunkBytes) {
    $end = [Math]::Min($i + $chunkBytes - 1, $b.Length - 1)
    $hex = [BitConverter]::ToString($b[$i..$end]).Replace('-', '').ToLower()
    $h = [BitConverter]::ToString($md5.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($hex))).Replace('-', '').ToLower()
    $null = $sb.AppendLine("$hex $h")
  }
  [System.IO.File]::WriteAllText('/tmp/tp_pull.b16', $sb.ToString())
}

function Show-Chunk([int]$index, [int]$lineWidth, [bool]$charSpace) {
  Clear-Host
  Write-Output ''
  Start-Sleep -Milliseconds 500
  $line = (Get-Content /tmp/tp_pull.b16)[$index - 1]
  $parts = $line -split ' '
  $hex = $parts[0]
  $md5 = $parts[1]
  $wrapped = [regex]::Replace($hex, "(.{$lineWidth})", '$1' + "`n")
  if ($charSpace) {
    $wrapped = [regex]::Replace($wrapped, '(.)', ' $1')
    $md5 = [regex]::Replace($md5, '(.)', ' $1')
  }
  Write-Output $wrapped
  Write-Output $md5
}

switch ($Action) {
  'probe' { Probe-File $Arg1 ($Arg2 -eq '1') }
  'prepare' { Prepare-Chunks $Arg1 ([int]$Arg2) }
  'show' { Show-Chunk ([int]$Arg1) ([int]$Arg2) ($Arg3 -eq '1') }
  default { Write-Error "Usage: $($MyInvocation.MyCommand.Name) {probe|prepare|show} ..."; exit 1 }
}
