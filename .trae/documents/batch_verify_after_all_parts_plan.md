# 分片传输：所有分片完成后批量校验 + 合并

## 需求

当前分片模式下，每传完一片就调用一次还原脚本校验该片 md5，最后一片额外触发合并。
新需求：**所有分片传完后，只调用一次还原脚本**，传入所有分片的 md5，对所有分片做一次批量 md5 校验，校验通过后再执行合并及后续 decode/gunzip/unzip。

## 设计方案（按用户反馈调整）

### 调用契约

**单次模式（非分片）**：
```
<script> <uid_full> <local_md5>
```

**分片模式**：`to_send` 传完后调用一次：
```
<script> <uid_full> <local_md5> <part_md5s>
```
- `uid_full`：基础 uid（**不带 `.pN` 后缀**），如 `typepaste_file.b32`。与单次模式一致。
- `local_md5`：原始数据整体 md5。与单次模式一致。
- `part_md5s`：所有分片 md5 按 p1..pN 顺序用逗号拼接，如 `md5_1,md5_2,...,md5_N`。
- **不传 total**：`total = part_md5s` 中 md5 的个数。

### 触发条件
- `run_chunked_transfer` 循环只负责逐片写 heredoc，**不调用脚本**。
- 循环结束后（`to_send` 非空时），**总是调用一次脚本**（不要求包含最后一片）。
- 脚本校验 `part_md5s` 列出的所有分片；若本次只传了部分片，缺失的片会校验失败并报错，用户补传后重新运行即可。

### uid 解析变更
- 脚本不再从 uid 末尾 `.p{n}` 解析分片号（因为传入的是 base uid）。
- 分片模式由 `part_md5s` 是否非空判定。
- 分片文件名 = `uid_full.p{i}`，i 从 1 到 total。

### 脚本批量校验逻辑（4 平台统一）

```
cur = uid_full
local_md5 = $2
part_md5s = $3

if part_md5s 非空:
    md5_arr = part_md5s.split(',')
    total = len(md5_arr)
    base = cur
    errors = []
    for i in 1..total:
        part_file = base.p{i}
        expected = md5_arr[i-1]
        if not exists(part_file):
            errors.append("part $i 缺失")
            continue
        actual = md5(去换行内容 of part_file)
        if actual == expected:
            print OK
        else:
            errors.append("part $i md5 mismatch (got=$actual want=$expected)")
            mv part_file part_file.x
    if errors 非空:
        print 所有错误
        exit 1
    cat p1..pN > base
    cur = base
    md5 = local_md5
继续 decode → gunzip → md5(local_md5) → unzip
```

校验循环**不提前退出**：缺失文件或 md5 不匹配时记录错误并继续下一片；
循环结束后若有错误则统一输出并 exit 1（不合并），无错误才合并。
失败片改名 `.x`，便于用户用 `--only-parts` 重传。

## 修改文件清单

### 1. `src/cli.rs`

**`auto_invoke_command`**（cli.rs:534）：
- 签名改为：`auto_invoke_command(args, uid_full, md5, part_md5s: Option<&str>) -> String`
  - `md5`：总是 `local_md5`（原始数据整体 md5）
  - `part_md5s`：`None`=单次模式，`Some(逗号串)`=分片模式
- 命令格式：
  - 单次：`{script} {uid_full} {md5}`
  - 分片：`{script} {uid_full} {md5} {part_md5s}`

**`run_chunked_transfer`**（cli.rs:564）：
- 循环体内**移除** `auto_invoke_command` 调用及 `type_command(invoke)`（保留 heredoc 写入与 sleep）
- 循环结束后（`to_send` 非空），调用一次：
  ```rust
  let all_md5s = parts.part_md5s.join(",");
  let invoke = auto_invoke_command(
      args, &payload.uid_full, &payload.local_md5, Some(&all_md5s),
  );
  type_command(&format!("{invoke}\n"), interval, &mut send_char, stop);
  ```
- dry-run 分支同步调整：逐片只打印 uid+md5；最后打印一次批量调用命令

**`run_auto_mode`**（cli.rs:475）：
- 调用改为 `auto_invoke_command(args, &payload.uid_full, &payload.local_md5, None)`

**测试**（cli.rs 860-1025）：
- 更新 `auto_invoke_command` 测试：单次模式断言不变；分片模式断言 `<uid_full> <local_md5> <all_md5s>`
- 移除原 `total`/`local_md5` 双 Option 参数的测试

### 2. `scripts/restore_linux.sh` / `restore_gitbash.sh`
- 参数：`cur="$1"`, `local_md5="$2"`, `part_md5s="$3"`
- 分片判定：`if [ -n "$part_md5s" ]`
- `IFS=',' read -ra md5_arr <<< "$part_md5s"`；`total=${#md5_arr[@]}`
- 循环校验 p1..pN，失败改名 `.x` 并 exit 1
- `cat p1..pN > base`；`cur="$base"`；`md5="$local_md5"`
- 移除原 `.p{n}` 正则解析与单片 `$md5` 校验逻辑
- 注意 `set -e`：用 `if` 而非 `&&` 短路

### 3. `scripts/restore_mac.sh`
- 同 linux，md5 计算保留 `md5sum`/`md5 -q` 回退

### 4. `scripts/restore_powershell.ps1`
- `param`：`$File`, `$LocalMd5`, `$PartMd5s`
- 分片判定：`if ($PartMd5s)`
- `$md5Arr = $PartMd5s -split ','`；`$total = $md5Arr.Count`
- 循环校验，失败 `Rename-Item ... .x; exit 1`
- 合并后 `$cur = $base`；`$ExpectedMd5 = $LocalMd5`

### 5. `README.md`
- 更新调用契约：分片模式 `<script> <uid_full> <local_md5> <part_md5s>`
- 更新分片流程：去掉逐片校验，改为传完后批量校验；触发条件为本次传输完成即调用

## 风险与注意

1. **命令行长度**：每片 md5 32 字符 + 逗号，100 片约 3.3KB，远低于 shell 限制，安全。
2. **`set -e` 陷阱**：bash 中 `[ -n "$x" ] && cmd` 条件为假时返回 1 会触发退出，必须用 `if` 语句。
3. **断点续传**：若本次只传部分片，脚本校验所有片时缺失片会报错（exit 1），用户补传后重跑即可。已传片的 `.x` 改名机制保持不变。
4. **uid 不带 `.pN`**：脚本不再依赖 `.p{n}` 后缀解析分片，分片模式完全由 `part_md5s` 非空判定。
5. **向后兼容**：单次模式契约不变；分片模式契约变更（参数语义 + 数量），旧脚本需重新部署。

## 验证

- `cargo build` 0 warning
- `cargo test` 全通过
- dry-run 验证：
  - 正常分片：最后打印一次 `<uid_full> <local_md5> <part_md5s>` 命令
  - `--only-parts 1`（只传一片）：仍打印批量校验命令（part_md5s 含所有片 md5）
  - `--skip-parts 1-2`：传完剩余片后打印批量校验命令
- （可选）真机验证脚本批量校验+合并流程
