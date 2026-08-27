# typepaste restore (powershell)
# Usage:
#   单次模式: powershell -File restore_powershell.ps1 <encoded_file> <local_md5>
#   分片模式: powershell -File restore_powershell.ps1 <uid_full> <local_md5> <part_md5s>
#     part_md5s: 所有分片 md5 按 p1..pN 逗号拼接；total = md5 个数。
# 据 uid 后缀反向还原：decode -> gunzip -> md5 -> unzip
param([Parameter(Mandatory=$true)][string]$File, [string]$LocalMd5, [string]$PartMd5s)
$ErrorActionPreference = "Stop"

function Get-Md5([string]$path) {
  return (Get-FileHash -Path $path -Algorithm MD5).Hash.ToLower()
}

$cur = $File
$ExpectedMd5 = $LocalMd5

# 分片模式：$PartMd5s 非空时，批量校验所有分片后合并
if ($PartMd5s) {
    $md5Arr = $PartMd5s -split ','
    $total = $md5Arr.Count
    $base = $cur
    $errors = @()
    for ($i = 1; $i -le $total; $i++) {
        $partFile = "$base.p$i"
        $expected = $md5Arr[$i - 1]
        if (-not (Test-Path -LiteralPath $partFile)) {
            $errors += "[FAIL] part $i 缺失文件: $partFile"
            continue
        }
        $content = (Get-Content -Raw $partFile) -replace "`r|`n", ""
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($content)
        $hash = [System.Security.Cryptography.MD5]::Create().ComputeHash($bytes)
        $actual = ([System.BitConverter]::ToString($hash) -replace '-', '').ToLower()
        if ($actual -eq $expected) {
            Write-Host "[OK] part $i md5 match"
        } else {
            $errors += "[FAIL] part $i md5 mismatch (got=$actual want=$expected)"
            Rename-Item -LiteralPath $partFile -NewName "$partFile.x"
        }
    }
    if ($errors.Count -gt 0) {
        $errors | ForEach-Object { Write-Host $_ }
        Write-Host "[FAIL] 共有分片校验失败，未合并"
        exit 1
    }
    $parts = 1..$total | ForEach-Object { "$base.p$_" }
    $stream = [System.IO.File]::Create($base)
    foreach ($p in $parts) {
        $bytes = [System.IO.File]::ReadAllBytes($p)
        $stream.Write($bytes, 0, $bytes.Length)
    }
    $stream.Close()
    Write-Host "[OK] 已合并 $total 片 → $base"
    $cur = $base
}

# decode
if ($cur -like '*.b32') {
  $out = $cur -replace '\.b32$',''
  $py = Get-Command python3 -ErrorAction SilentlyContinue
  if (-not $py) { throw "base32 解码需要 python3（目标机未安装）" }
  $code = "import sys,base64;sys.stdout.buffer.write(base64.b32decode(sys.stdin.read().upper().encode()))"
  $bytes = ((Get-Content -Raw $cur) | python3 -c $code)
  [System.IO.File]::WriteAllBytes((Resolve-Path -LiteralPath .).Path + "\" + (Split-Path $out -Leaf), $bytes)
  $cur = $out
} elseif ($cur -like '*.b64') {
  $out = $cur -replace '\.b64$',''
  $text = (Get-Content -Raw $cur) -replace '\s',''
  $bytes = [System.Convert]::FromBase64String($text)
  [System.IO.File]::WriteAllBytes((Resolve-Path -LiteralPath .).Path + "\" + (Split-Path $out -Leaf), $bytes)
  $cur = $out
} elseif ($cur -like '*.b16') {
  $out = $cur -replace '\.b16$',''
  $hex = ((Get-Content -Raw $cur) -replace '\s','').ToUpper()
  $bytes = New-Object byte[] ($hex.Length / 2)
  for ($i = 0; $i -lt $bytes.Length; $i++) { $bytes[$i] = [Convert]::ToByte($hex.Substring($i*2, 2), 16) }
  [System.IO.File]::WriteAllBytes((Resolve-Path -LiteralPath .).Path + "\" + (Split-Path $out -Leaf), $bytes)
  $cur = $out
}

# gunzip
if ($cur -like '*.gz') {
  $out = $cur -replace '\.gz$',''
  $in = [System.IO.File]::OpenRead($cur)
  $outf = [System.IO.File]::Create($out)
  $gz = New-Object System.IO.Compression.GzipStream($in, [System.IO.Compression.CompressionMode]::Decompress)
  $gz.CopyTo($outf)
  $gz.Close(); $outf.Close(); $in.Close()
  $cur = $out
}

# md5 verify
if ($ExpectedMd5) {
  $actual = Get-Md5 $cur
  if ($actual -eq $ExpectedMd5) {
    Write-Host "[OK] md5 match ($cur)"
  } else {
    Write-Host "[FAIL] md5 mismatch (got=$actual want=$ExpectedMd5)"
  }
}

# unzip
if ($cur -like '*.zip') {
  Expand-Archive -Path $cur -DestinationPath . -Force
}
