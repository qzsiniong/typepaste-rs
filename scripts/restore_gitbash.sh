#!/usr/bin/env bash
# typepaste restore (gitbash) — Windows Git Bash，GNU 工具，xxd 可能缺失用 python3 回退。
# Usage:
#   单次模式: bash restore_gitbash.sh <encoded_file> <local_md5>
#   分片模式: bash restore_gitbash.sh <uid_full> <local_md5> <part_md5s>
#     part_md5s: 所有分片 md5 按 p1..pN 逗号拼接；total = md5 个数。
# 据 uid 后缀反向还原：decode(.b32/.b64/.b16) -> gunzip(.gz) -> md5 -> unzip(.zip)
set -e
cur="$1"
local_md5="$2"
part_md5s="$3"

md5="$local_md5"

# 分片模式：part_md5s 非空时，批量校验所有分片后合并
if [ -n "$part_md5s" ]; then
  IFS=',' read -ra md5_arr <<< "$part_md5s"
  total=${#md5_arr[@]}
  base="$cur"
  errors=""
  for i in $(seq 1 "$total"); do
    part_file="$base.p$i"
    expected="${md5_arr[$((i-1))]}"
    if [ ! -f "$part_file" ]; then
      errors="$errors\n[FAIL] part $i 缺失文件: $part_file"
      continue
    fi
    actual=$(tr -d '\n' < "$part_file" | md5sum | cut -d' ' -f1)
    if [ "$actual" = "$expected" ]; then
      echo "[OK] part $i md5 match"
    else
      errors="$errors\n[FAIL] part $i md5 mismatch (got=$actual want=$expected)"
      mv "$part_file" "$part_file.x"
    fi
  done
  if [ -n "$errors" ]; then
    echo -e "$errors"
    exit 1
  fi
  cat $(seq 1 "$total" | sed "s|^|$base.p|") > "$base"
  echo "[OK] 已合并 $total 片 → $base"
  cur="$base"
fi

case "$cur" in
  *.b32)
    out="${cur%.b32}"
    if command -v base32 >/dev/null 2>&1; then
      cat "$cur" | tr 'a-z' 'A-Z' | base32 -d > "$out"
    else
      python3 -c "import sys,base64;sys.stdout.buffer.write(base64.b32decode(sys.stdin.read().upper().encode()))" < "$cur" > "$out"
    fi
    echo "[OK] 已解码 $cur → $out"
    cur="$out" ;;
  *.b64)
    out="${cur%.b64}";
    base64 -d "$cur" > "$out";
    echo "[OK] 已解码 $cur → $out"
    cur="$out" ;;
  *.b16)
    out="${cur%.b16}"
    if command -v xxd >/dev/null 2>&1; then
      cat "$cur" | tr 'a-z' 'A-Z' | xxd -r -p > "$out"
    else
      python3 -c "import sys;sys.stdout.buffer.write(bytes.fromhex(sys.stdin.read().strip().upper()))" < "$cur" > "$out"
    fi
    echo "[OK] 已解码 $cur → $out"
    cur="$out" ;;
esac

case "$cur" in
  *.gz) 
    gunzip "$cur";
    echo "[OK] 已解压 $cur → ${cur%.gz}"
    cur="${cur%.gz}" ;;
esac

if [ -n "$md5" ]; then
  actual=$(md5sum "$cur" | cut -d' ' -f1)
  if [ "$actual" = "$md5" ]; then
    echo "[OK] md5 match ($cur)"
  else
    echo "[FAIL] md5 mismatch (got=$actual want=$md5)"
    exit 1
  fi
fi

case "$cur" in
  *.zip) 
    unzip -o "$cur";
    echo "[OK] 已解压 $cur → ${cur%.zip}"
    cur="${cur%.zip}" ;;
esac
