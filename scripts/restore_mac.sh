#!/usr/bin/env bash
# typepaste restore (mac) — macOS 缺 base32/md5sum，用 python3/md5 回退。
# Usage:
#   单次模式: bash restore_mac.sh <encoded_file> <local_md5>
#   分片模式: bash restore_mac.sh <uid_full> <local_md5> <part_md5s>
#     part_md5s: 所有分片 md5 按 p1..pN 逗号拼接；total = md5 个数。
set -e
cur="$1"
local_md5="$2"
part_md5s="$3"

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
    if command -v md5sum >/dev/null 2>&1; then
      actual=$(tr -d '\n' < "$part_file" | md5sum | cut -d' ' -f1)
    else
      actual=$(tr -d '\n' < "$part_file" | md5 -q)
    fi
    if [ "$actual" = "$expected" ]; then
      echo "[OK] part $i md5 match"
    else
      errors="$errors\n[FAIL] part $i md5 mismatch (got=$actual want=$expected)"
      mv "$part_file" "$part_file.x"
    fi
  done
  if [ -n "$errors" ]; then
    echo -e "$errors"
    echo "[FAIL] 共有分片校验失败，未合并"
    exit 1
  fi
  cat $(seq 1 "$total" | sed "s|^|$base.p|") > "$base"
  echo "[OK] 已合并 $total 片 → $base"
  cur="$base"
  md5="$local_md5"
fi

case "$cur" in
  *.b32)
    out="${cur%.b32}"
    python3 -c "import sys,base64;sys.stdout.buffer.write(base64.b32decode(sys.stdin.read().upper().encode()))" < "$cur" > "$out"
    cur="$out" ;;
  *.b64)
    out="${cur%.b64}"; base64 -d "$cur" > "$out"; cur="$out" ;;
  *.b16)
    out="${cur%.b16}"
    python3 -c "import sys;sys.stdout.buffer.write(bytes.fromhex(sys.stdin.read().strip().upper()))" < "$cur" > "$out"
    cur="$out" ;;
esac

case "$cur" in
  *.gz) gunzip "$cur"; cur="${cur%.gz}" ;;
esac

if [ -n "$md5" ]; then
  if command -v md5sum >/dev/null 2>&1; then
    actual=$(md5sum "$cur" | cut -d' ' -f1)
  else
    actual=$(md5 -q "$cur")
  fi
  if [ "$actual" = "$md5" ]; then
    echo "[OK] md5 match ($cur)"
  else
    echo "[FAIL] md5 mismatch (got=$actual want=$md5)"
  fi
fi

case "$cur" in
  *.zip) unzip -o "$cur" ;;
esac
