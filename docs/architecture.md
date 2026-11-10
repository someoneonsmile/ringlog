# 架构

ringlog 是「一个库 + 一个命令行应用」：**库是引擎，二进制是应用**。目标是让文件格式与
读写逻辑可独立阅读、测试和复用，让所有终端 I/O 与用户可见文案集中在一处。

## 1. 分层

```
┌──────────────── bin：src/main.rs + src/app/ ─────────────────┐
│  main.rs   parse → dispatch → ExitCode（约 55 行）           │
│  cli.rs    clap 定义、跨参校验、parse_size、Mode/StartPos    │
│  writer.rs stdin 泵线程、攒批阈值、截断汇总                   │
│  follow.rs Tailer 循环、落后/重置检测与告警                   │
│  info.rs   --info 渲染                                        │
│  report.rs stdout / stderr 的唯一出口                         │
└───────────────────────────┬──────────────────────────────────┘
                            │ 只通过 pub API 使用
┌───────────────────────────▼──────────────────────────────────┐
│ lib：ringlog（引擎，无终端 I/O、无 clap、无中文文案）          │
│  format.rs 文件布局、Header 编解码、尺寸约束                  │
│  input.rs  字节流 → Batch（payload + Record）                 │
│  ring.rs   Ring（写者）/ Reader（读者）/ 头读写 / 锁文件      │
│  seek.rs   窗口定位：StartPos / locate_start / scan_*         │
│  error.rs  错误分类：usage / invalid / is_user_error          │
└──────────────────────────────────────────────────────────────┘
```

## 2. 目录

```
src/
├── lib.rs          引擎入口：声明模块、写明边界
├── format.rs       文件格式 v1
├── ring.rs         文件读写层
├── input.rs        行分帧
├── seek.rs         首屏定位
├── error.rs        错误分类
├── main.rs         bin 入口（声明 mod app）
└── app/            应用层，只被 bin 编译
    ├── cli.rs
    ├── writer.rs
    ├── follow.rs
    ├── info.rs
    └── report.rs
tests/
├── common/mod.rs   共享 TestDir / bin()
├── cli.rs          黑盒行为测试（27）
└── adversarial.rs  对抗性与崩溃一致性测试（20）
```

`app/` 只由 `main.rs` 的 `mod app;` 声明，`lib.rs` 不引用它——库和应用的边界由
Cargo 目标本身保证：`use ringlog::...` 只能看见引擎，应用内部的模块对库不可见。

## 3. 依赖规则

1. **lib 不依赖 bin**；bin 只通过 `pub` API 使用 lib。
2. lib 内部单向：`format ← ring ← seek`；`input` 独立；`error` 被 `ring` 使用。
3. **终端 I/O 只允许出现在 bin 的 `app/report.rs`**：`info` 用 `report::stdout`，
   `follow` 用 `report::buffered_stdout` 与 `report::warn`，`writer` 用 `report::warn`。
4. 用户可见文案（中文提示、帮助、退出码语义）只在 bin。
5. 文件格式知识只在 `format`：其他模块不得重算槽位偏移、CRC 范围或尺寸算术。
   集成测试**有意**独立实现格式解析，作为黑盒测试的独立 oracle。

检查方式：

```bash
# lib 内不应出现终端输出或参数解析代码
rg -n 'io::stdout|io::stderr|use clap' src/lib.rs src/format.rs src/input.rs src/ring.rs src/seek.rs src/error.rs
# 终端输出应只有一处
rg -n 'io::stdout|io::stderr' src/
```

## 4. 模块职责与接口

| 模块 | 职责 | 关键接口 |
| --- | --- | --- |
| `format` | 布局常量、`Header` 双 slot 编解码、不变量校验、尺寸推导 | `Header`、`MIN_TOTAL_SIZE`、`data_capacity`、`file_size` |
| `ring` | 写者互斥与环形写（含换行填充）、读者共享锁快照、跨环读 | `Ring::open/append`、`Reader::open/with_snapshot/read_exact_abs`、`lock_path` |
| `input` | 按 `\n` 分帧，超长行截断并计数 | `LineSplitter::feed/take(final_chunk)`、`Batch`、`Record` |
| `seek` | 在环形窗口里找记录起点，报告丢弃量 | `StartPos`、`Located`（`pos/dropped/no_newline`）、`locate_start`、`scan_to_next_line` |
| `error` | 区分用法/数据错误与普通 I/O 错误 | `usage`、`invalid`（crate 内）、`is_user_error`（公开） |
| `app::cli` | clap 参数、模式互斥、尺寸解析 | `parse` → `Cli { path, mode: Mode }` |
| `app::writer` | stdin 泵线程 + 攒批 + 截断汇总 | `run(path, size, force, sync)` |
| `app::follow` | 首次定位 + 跟随循环 + 告警 | `run(reader, follow, start, interval)` |
| `app::info` | 元信息渲染 | `run(path)` |
| `app::report` | 终端输出唯一出口 | `stdout`、`buffered_stdout`、`warn` |

## 5. 数据流

**写入**：

```
stdin ──读线程（256 KiB 块，sync_channel(4)）──▶ LineSplitter.feed
      ▶ 超长行截断计数
      ▶ 攒满 256 KiB / 4096 条，或 100 ms 兜底 ──▶ Ring::append(Batch)
          [flock 排他] 规划 Op（Pad / Data）→ write_all_at →（--sync 时 fsync）
                     → 写新 header slot → 更新内存状态 → 解锁
```

**读取**：

```
Reader::open（校验 文件大小 == file_size(capacity)）
      ▶ follow::run：init 用 seek 计算起点（全部 / 只看新 / 最近 N 行）
      ▶ 每 step：[共享锁] 读头 → 判重置 / 判落后 → read_exact_abs ≤64 KiB → 解锁
                 → 只输出完整行（半行留 pending）→ 事件在锁外打印
```

关键设计：**锁内只碰文件和内存，锁外才写 stdout**，因此下游慢管道不会反向阻塞写者。

## 6. 跨模块契约

| 契约 | 定义方 | 维护方 | 依赖方 |
| --- | --- | --- | --- |
| 布局与不变量 `total_written % capacity == write_pos` | `format` | `ring` 写批次时保证 | `seek`、`follow` 计算逻辑偏移 |
| 每条记录以 `\n` 结尾、长度 ≤ `capacity` | `input` 生产 | `ring::append` 的 `debug_assert` 前提 | `seek` 只按换行符定位 |
| 数据区任意时刻都是完整记录的串（不跨环拆行） | `ring` 的换行填充 | `ring` | `seek` 的扫描、`follow` 的输出 |
| 「头部 + 数据」来自同一一致状态 | `Reader::with_snapshot` | `ring` 的 flock 协议 | `follow`、`info`、`seek` 调用方 |
| 同一文件同一时刻只有一个写者 | `Ring::open` 的 `<FILE>.lock` flock | `ring` | `cli` 的帮助文案 |
| `capacity = 文件大小 - 4 KiB` | `format::{MIN_TOTAL_SIZE, data_capacity, file_size}` | `ring`、`cli` 共用 | `input`（记录上限） |

## 7. 错误与退出码

| 码 | 含义 | 来源 |
| --- | --- | --- |
| 0 | 正常结束（含下游关闭管道 EPIPE） | `main` |
| 1 | 普通 I/O 错误 | `ringlog::error::is_user_error` 为假 |
| 2 | 用法错误、数据错误（参数/尺寸/头部无效等） | clap 参数错误；`error::is_user_error` 为真 |

库内统一用 `io::Error`，但按语义选择 `ErrorKind`：`InvalidInput` = 用法，
`InvalidData` = 数据，其余为 I/O。退出码判定只在 `main` 一处使用
`is_user_error`，避免"每个模块自己决定错误类别"。

## 8. 测试

| 层次 | 位置 | 说明 |
| --- | --- | --- |
| 单元 | `format` / `input` / `seek` / `app::cli` | 纯逻辑与真实临时文件，无子进程 |
| 黑盒行为 | `tests/cli.rs` | 通过 `CARGO_BIN_EXE_ringlog` 跑二进制，覆盖 CLI 契约与读写行为 |
| 对抗 | `tests/adversarial.rs` | 极小容量、头部撕裂/损坏、SIGKILL、并发、EPIPE、`--force` 重置 |

`tests/common/mod.rs` 提供 `TestDir` / `bin()`；两个测试文件**各自**实现文件格式解析，
这是有意为之——用独立 oracle 验证格式，而不是复用 `format.rs`。

```bash
cargo test                 # 单元 + 黑盒 + 对抗
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
