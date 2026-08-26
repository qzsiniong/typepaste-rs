#!/usr/bin/env bash
# typepaste restore (linux)
# Usage:
#   单次模式: bash restore_linux.sh <encoded_file> <local_md5>
#   分片模式: bash restore_linux.sh <uid_full> <local_md5> <part_md5s>
#     part_md5s: 所有分片 md5 按 p1..pN 逗号拼接；total = md5 个数。
# 据 uid 后缀反向还原：decode(.b32/.b64/.b16) -> gunzip(.gz) -> md5 -> unzip(.zip)
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
    echo "[FAIL] 共有分片校验失败，未合并"
    exit 1
  fi
  cat $(seq 1 "$total" | sed "s|^|$base.p|") > "$base"
  echo "[OK] 已合并 $total 片 → $base"
  cur="$base"
  md5="$local_md5"
fi

case "$cur" in
  *.b32) out="${cur%.b32}"; cat "$cur" | tr 'a-z' 'A-Z' | base32 -d > "$out"; cur="$out" ;;
  *.b64) out="${cur%.b64}"; base64 -d "$cur" > "$out"; cur="$out" ;;
  *.b16) out="${cur%.b16}"; cat "$cur" | tr 'a-z' 'A-Z' | xxd -r -p > "$out"; cur="$out" ;;
esac

case "$cur" in
  *.gz) gunzip "$cur"; cur="${cur%.gz}" ;;
esac

if [ -n "$md5" ]; then
  actual=$(md5sum "$cur" | cut -d' ' -f1)
  if [ "$actual" = "$md5" ]; then
    echo "[OK] md5 match ($cur)"
  else
    echo "[FAIL] md5 mismatch (got=$actual want=$md5)"
  fi
fi

case "$cur" in
  *.zip) unzip -o "$cur" ;;
esac
