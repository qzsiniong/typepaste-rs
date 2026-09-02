#!/usr/bin/env bash
# typepaste pull (mac) — 远程端反向传输辅助脚本。
# Usage:
#   bash typepaste-pull.sh probe <path>                       # 输出: <size> <md5> <checksum>
#   bash typepaste-pull.sh prepare <path> <chunk_bytes>       # 生成 /tmp/tp_pull.b16（每行: hex md5）
#   bash typepaste-pull.sh show <index> <line_width> <space> [file]  # 清屏并显示第 index 片
#   bash typepaste-pull.sh subchunk <index> <sub_chunk_bytes> # 将第 index 片切为更小的子分片 → /tmp/tp_pull_sub.b16
# space=1 表示每字符前加空格（提升 OCR 分割），0 表示仅首字符前加空格。
set -e

probe() {
  local path="$1"
  local space="$2"
  clear || true
  sleep .1
  echo
  # 1. 文件大小（clean digits）：BSD stat -f 优先，GNU stat -c 回退
  local size
  size=$(stat -f '%z' "$path" 2>/dev/null || stat -c '%s' "$path" 2>/dev/null)
  # 2. 文件 md5（clean 32-hex）：md5 -q 优先，md5sum 回退
  local file_md5
  if command -v md5 >/dev/null 2>&1 && ! command -v md5sum >/dev/null 2>&1; then
    file_md5=$(md5 -q "$path")
  else
    file_md5=$(md5sum "$path" | cut -d' ' -f1)
  fi
  # 3. 校验和 = md5(size + file_md5)
  local checksum
  if command -v md5 >/dev/null 2>&1 && ! command -v md5sum >/dev/null 2>&1; then
    checksum=$(printf '%s' "${size}${file_md5}" | md5 -q)
  else
    checksum=$(printf '%s' "${size}${file_md5}" | md5sum | cut -d' ' -f1)
  fi
  # 4. 格式化输出三行：size 前导空格，md5 与 checksum 按 space 参数格式化
  echo " ${size}"
  local md5_sed
  if [ "$space" = "1" ]; then
    md5_sed='s/./ &/g'
  else
    md5_sed='s/^./ &/'
  fi
  printf '%s\n' "$file_md5" | sed -E "$md5_sed"
  printf '%s\n' "$checksum" | sed -E "$md5_sed"
}

prepare() {
  local path="$1"
  local chunk_bytes="$2"
  # 步骤 1：生成 base16 分片（xxd 优先，python3 回退）
  if command -v xxd >/dev/null 2>&1; then
    xxd -p -c "$chunk_bytes" "$path" > /tmp/tp_pull.b16
  else
    python3 -c "import sys;d=open(sys.argv[1],'rb').read();c=int(sys.argv[2]);[print(d[i:i+c].hex()) for i in range(0,len(d),c)]" "$path" "$chunk_bytes" > /tmp/tp_pull.b16
  fi
  # 步骤 2：逐行追加 md5（perl 单进程优先，python3 回退）
  if command -v perl >/dev/null 2>&1; then
    perl -i -MDigest::MD5 -ne 'chomp;print "$_ ",Digest::MD5::md5_hex($_),"\n"' /tmp/tp_pull.b16
  else
    python3 -c "import sys,hashlib as h;[print(l.split()[0],h.md5(l.split()[0].encode()).hexdigest()) for l in open(sys.argv[1])]" /tmp/tp_pull.b16 > /tmp/tp_pull.b16.new && mv /tmp/tp_pull.b16.new /tmp/tp_pull.b16
  fi
}

show() {
  local i="$1"
  local lw="$2"
  local space="$3"
  local file="${4:-/tmp/tp_pull.b16}"
  local space_sed
  if [ "$space" = "1" ]; then
    space_sed='s/./ &/g'
  else
    space_sed='s/^./ &/'
  fi
  clear || true
  sleep .1
  echo
  sed -n "${i}p" "$file" | awk '{print $1}' | sed -E "s/.{$lw}/&\n/g" | sed -E "$space_sed"
  sed -n "${i}p" "$file" | awk '{print $2}' | sed -E "$space_sed"
}

subchunk() {
  local i="$1"
  local sub_bytes="$2"
  local sub_chars=$((sub_bytes * 2))
  # 读取第 i 行的 hex 内容
  local hex
  hex=$(sed -n "${i}p" /tmp/tp_pull.b16 | awk '{print $1}')
  # 按 sub_chars 字符切分，每片追加 md5，写入 /tmp/tp_pull_sub.b16
  : > /tmp/tp_pull_sub.b16
  if command -v perl >/dev/null 2>&1; then
    printf '%s' "$hex" | perl -MDigest::MD5 -ne 'chomp;my $n='"$sub_chars"';while(length($_)>0){my $s=substr($_,0,$n,"");print "$s ",Digest::MD5::md5_hex($s),"\n"}' > /tmp/tp_pull_sub.b16
  else
    python3 -c "
import sys,hashlib
h=sys.argv[1];n=int(sys.argv[2])
with open('/tmp/tp_pull_sub.b16','w') as f:
    for i in range(0,len(h),n):
        s=h[i:i+n]
        f.write(f'{s} {hashlib.md5(s.encode()).hexdigest()}\n')
" "$hex" "$sub_chars"
  fi
}

case "$1" in
  probe) probe "$2" "$3" ;;
  prepare) prepare "$2" "$3" ;;
  show) show "$2" "$3" "$4" "$5" ;;
  subchunk) subchunk "$2" "$3" ;;
  *) echo "Usage: $0 {probe|prepare|show|subchunk} ..." >&2; exit 1 ;;
esac
