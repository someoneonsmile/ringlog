# 架构评审：问题清单与修复方案

- 状态：**已实施**（Phase 1-4 完成，见文末「实施记录」；最终结构见 [architecture.md](architecture.md)）
- 基线（重构前）：`cargo test` 67 通过（单测 20 + 黑盒 27 + 对抗 20）、`cargo clippy --all-targets` 无告警
- 范围：`src/` 7 个文件（约 1 500 行）+ `tests/` 2 个文件（约 2 120 行）
- **不含**：功能 bug 排查、文件格式变更、CLI 行为变更
- 目的：先就"问题与方案"达成一致，再分阶段重构

## 0. 一句话结论

依赖图无环、单模块内聚尚可，**不需要重写**。真正的债在"边界"：lib 里装了半个 CLI（参数解析、stdout/stderr、中文告警），bin 里装了半个引擎（攒批策略、线程、格式化）。于是 lib 不可复用、告警不可测、`main.rs` 变成上帝模块。

修复方向：把 lib 收敛为「文件格式 + 读写引擎」，把终端 I/O 与用户可见文案全部收回 bin，再用类型把几处"魔法约定"变成显式契约。分 4 个阶段，每阶段行为不变，e2e 测试作为安全网。

## 1. 现状

### 1.1 依赖关系

```
src/main.rs (bin) ──┬─→ lib::cli ──────→ lib::format（HEADER_PAGE / MIN_DATA_CAPACITY）
                    ├─→ lib::follow ───→ lib::ring::Reader, lib::format::Header
                    ├─→ lib::input::LineSplitter
                    └─→ lib::ring::{Ring, Reader} ──→ lib::format

lib 内部：follow 直接 io::stdout()、直接 crate::warn → stderr（库内含终端 I/O）
         lib::warn 与 main::write_stderr 是两套互不相知的 stderr 出口

tests/cli.rs、tests/adversarial.rs：不链接 lib，只通过 CARGO_BIN_EXE_ringlog 跑二进制，
并各自独立实现文件格式解析（作为独立 oracle）
```

| 文件 | 行数 | 当前职责 |
| --- | --- | --- |
| `src/lib.rs` | 20 | 声明 5 个模块 + `pub(crate) fn warn` |
| `src/format.rs` | 159 | 布局常量、`Header` 编解码、`pub use crc32c::crc32c` |
| `src/input.rs` | 167 | 字节流 → 记录（分帧、截断、计数） |
| `src/ring.rs` | 384 | 写者 `Ring`、读者 `Reader`、头读写、锁文件 |
| `src/follow.rs` | 270 | 首屏定位 + 跟随循环 + stdout 输出 + stderr 告警 |
| `src/cli.rs` | 296 | clap 定义、跨参校验、`parse_size`、帮助文案（含 90 行单测） |
| `src/main.rs` | 206 | 退出码、stdin 线程、攒批策略、截断汇总、`--info` 文本 |

### 1.2 三个关键事实

1. **lib 没有外部消费者**：全仓库只有 `src/main.rs` 用 `ringlog::`；两个集成测试文件完全不 `use ringlog`。也就是说，当前 lib/bin 拆分只换来"单测能放在 lib 里"，没有换来任何可复用性，却付出了边界混乱的代价。
2. **终端 I/O 在 lib 内部**：`follow::run` 自己拿 `io::stdout()`（`src/follow.rs:31`），`crate::warn` 自己写 stderr（4 个调用点全在 `follow.rs`）。而 bin 因为跨 crate 访问不到 `pub(crate)`，只能复制一份 `write_stderr`（`src/main.rs:58`）。
3. **状态靠"魔法值"和注释表达**：`-n` 的三态用 `Option<u64>` + `0` 表示；记录区间用 `(usize, usize)` 裸元组；错误类别用 `io::ErrorKind` 兼职；`-s` 下限在 cli 和 ring 各实现一遍。

## 2. 问题清单

总览（严重度：高 = 影响可维护性/正确性保证；中 = 明显结构问题；低 = 优化项）：

| 编号 | 维度 | 严重度 | 摘要 |
| --- | --- | --- | --- |
| A1 | 架构 | 高 | lib/bin 边界倒置：lib 含应用层，bin 含引擎细节 |
| A2 | 架构 | 高 | stderr 两条出口（`lib::warn` / `main::write_stderr`） |
| A3 | 架构 | 中 | 模块职责与依赖方向没有成文契约 |
| S1 | 结构 | 高 | `main.rs` 上帝模块，6 类职责 |
| S2 | 结构 | 高 | `follow.rs` 三合一：定位算法 + 跟随循环 + 输出 |
| S3 | 结构 | 中 | `ring.rs` 双角色一文件，`Ring::open` 五合一 |
| S4 | 结构 | 低 | 工程化缺口（bin 无 `deny(unsafe_code)`、无 CI/fmt 检查） |
| R1 | 职责 | 中 | `-s` 下限在 cli 与 ring 重复校验 |
| R2 | 职责 | 低 | `LineSplitter` 隐式时序（finish→take）+ 分帧/计数混装 |
| B1 | 抽象 | 高 | `Cli` 扁平字段，非法状态可表示，`-n` 三态用魔法 0 |
| B2 | 抽象 | 中 | 记录区间用裸元组，契约只在注释 |
| B3 | 抽象 | 中 | 退出码语义靠 `io::ErrorKind` 隐式兼职 |
| B4 | 抽象 | 低 | `pub use crc32c::crc32c` 泄露第三方 API |
| C1 | 耦合 | 中 | cli → format 布局常量的跨层依赖 |
| C2 | 耦合 | 中 | `main` 知道攒批阈值、`capacity` 相等关系、header 排版 |
| C3 | 耦合 | 高 | lib 直接写 stdout/stderr，与调用方输出通道耦合 |
| C4 | 耦合 | 低 | 两个集成测试文件重复脚手架（格式解析重复是有意的） |

---

### 2.1 架构

**A1 lib/bin 边界倒置**（高）

- 现状：`cli.rs`（clap、帮助文案、尺寸解析）与 `follow.rs`（stdout 输出、stderr 告警）都在 lib；`main.rs` 反而承担 stdin 线程、攒批策略、`--info` 渲染、退出码。
- 问题：① 库一旦被引用，会阻塞当前进程的 stdin/stdout、直接打中文告警，副作用没有经过调用方许可；② 跟随/定位逻辑没有可注入的输出，无法单测；③ "什么算库、什么算应用"没有可检查的规则，后续只会更乱。
- 方案：lib 只保留 `format` / `input` / `ring` / `seek`（纯引擎：无 clap、无终端 I/O、无中文文案）；`cli` / `follow` / `writer` / `info` / `report` 进入 bin 的 `src/app/`。规则写进 §3。

**A2 stderr 有两条互不相知的出口**（高）

- 现状：`src/lib.rs:17` 的 `pub(crate) fn warn`（4 个调用点都在 `follow.rs`：151/160/184/240）；`src/main.rs:58` 的 `write_stderr`。前者在 lib 内可见，bin 跨 crate 访问不到，只能复制。
- 问题：前缀、措辞、失败策略靠人工保持一致；告警只能起子进程断言；库代码替调用方决定"说给谁听、怎么说"。
- 方案：lib 不再打印。把"发生了什么"作为返回值/事件上抛（`Event::Lost{bytes}` / `Event::Reset` / 数据异常 `io::Error`），由 bin 的 `app::report` 统一格式化并打印；删除 `lib::warn`。

**A3 架构契约只存在于注释和读者脑中**（中）

- 现状：README 讲行为，`format.rs`/`lib.rs` 的文档注释讲局部约定；没有一份"模块职责 + 依赖方向 + 不变量归属"的文档。
- 问题：review 时无法判断某行是否越界（例如 cli 读 `HEADER_PAGE` 算不算违规）；重构缺少验收标准。
- 方案：修复完成后把 §3 与"不变量归属表"固化为 `docs/architecture.md`，本文保留为决策记录。

### 2.2 结构

**S1 `main.rs` 是上帝模块**（高）

- 现状：206 行混了 6 类职责——退出码映射（27-50）、终端写辅助（52-60）、模式分派（62-71）、stdin 泵线程 + 攒批策略 + 截断汇总（73-174）、`--info` 文本拼接（176-206）、策略常量（18-25）。
- 问题：调整攒批策略要动"入口文件"；入口无法只读一遍看懂；策略常量与终端出口混在 bin 顶层。
- 方案：拆为 `app/writer.rs`（线程 + 攒批 + 截断汇总 + 调 report）、`app/info.rs`、`app/report.rs`；`main.rs` 只留 parse → dispatch → ExitCode（约 60 行）。

**S2 `follow.rs` 三合一**（高）

- 现状：定位算法 `locate_start`（194-218）、`scan_to_next_line`（220-245）、`scan_back`（247-270）约 100 行纯读逻辑，与跟随循环 `run`（25-55）、输出/告警 `Tailer`（57-192）挤在 270 行内；定位算法**零直接单测**。
- 问题：窗口对齐、`-n` 语义、环形接缝丢弃这些最易出边界错误的地方，改一行要靠 1.4k 行 e2e 兜底；同文件里"查数据"与"写终端"混在一起，读者难以分辨。
- 方案：抽 `src/seek.rs` 进 lib（只依赖 `Reader`，纯读）；用 `Ring` 造真实文件补 5~8 个单测；`follow.rs` 只剩"循环 + 输出 + 事件→文案"并迁入 bin。

**S3 `ring.rs` 双角色 + `Ring::open` 五合一**（中）

- 现状：`Ring`（写者）与 `Reader`（读者）同文件（384 行）；`Ring::open`（82-191）同时做加锁、打开文件、长度/格式校验、初始化/续写分支（`force || len == 0` 合并在一个 if）、`--force` 重建。
- 问题：`open` 是全项目分支最密处（5 种失败路径 + 2 种成功路径），几乎无法单测；读写混读时容易越界。
- 方案：不拆文件（见 D4），但把 `open` 拆成 `acquire_writer_lock` / `open_file` / `resume_existing` / `initialize_new` 四个小函数，主函数只做编排；错误文案保持原文。

**S4 工程化缺口**（低，不阻塞重构）

- 现状：`#![deny(unsafe_code)]` 只在 `lib.rs`，bin 没有；仓库无 CI、无 `rustfmt` 检查、无 MSRV 声明。
- 问题：约束只覆盖一半代码；格式/告警只能靠本地自觉。
- 方案：`main.rs` 补 `#![deny(unsafe_code)]`；CI/rfmt 是否加入本次另议（见 D5）。

### 2.3 职责

**R1 `-s` 下限在 cli 与 ring 重复校验**（中）

- 现状：`src/cli.rs:168-173` 用 `HEADER_PAGE + MIN_DATA_CAPACITY` 判一次并给中文错误；`src/ring.rs:118-129` 用同样算术再判一次，文案不同。
- 问题：同一条规则两处实现，格式几何一改容易只改一处；也是 C1 跨层依赖的来源。
- 方案：`format` 暴露 `MIN_TOTAL_SIZE` 与 `check_total_size(u64) -> Result<(), String>`；cli 用作 clap 校验，ring 用作防御性校验（文案统一）；cli 不再 import `HEADER_PAGE`。

**R2 `LineSplitter` 隐式时序 + 分帧/计数混装**（低）

- 现状：必须 `feed` → `finish()` → `take()`；`finish` 的语义（补尾行）只在注释里；`take()` 返回裸元组；截断计数与分帧同属一个类型。
- 问题：漏调 `finish` 会静默丢最后一行（main 目前调用正确，但类型不防错）；"输入已结束"无法从 API 表达。
- 方案：`take(&mut self, final_chunk: bool) -> Batch` 用参数替代隐式时序；截断计数保留（策略很小，单独抽类不划算）；`Batch`/`Record` 见 B2。

### 2.4 抽象

**B1 `Cli` 用扁平字段表达互斥模式**（高）

- 现状：`src/cli.rs:53-65` 一个结构体含 `size/lines/follow/sync/force/interval` 与 `mode` 并列；合法组合靠 `into_cli` 运行时判断；`src/main.rs:74-76` 还要再兜一次 `ok_or_else`（实际到不了，是**死分支**）。`-n` 的三态（缺省 / 0 / N）用 `Option<u64>` + 魔法 0 表示。
- 问题：类型允许"写入模式带 interval"这类非法状态；每个使用点都要重新判断字段此刻是否有意义；死分支让读者误以为 `-s` 可能缺失。
- 方案：`Cli { path, mode: Mode }`；`enum Mode { Write { size, sync, force }, Read { follow, lines: ReadLines, interval }, Info }`；`enum ReadLines { All, Last(u64), New }`。删除死分支；`follow::run` 改收 `ReadOptions`（`Mode::Read` 的内容）。

**B2 记录区间用裸元组表达**（中）

- 现状：`Ring::append(&[u8], &[(usize, usize)])`（199）；`LineSplitter::take() -> (Vec<u8>, Vec<(usize, usize)>)`（81-86）。"必须落在 payload 内、必须以 `\n` 结尾、长度 ≤ capacity"全靠注释和 `debug_assert`。
- 问题：调用方容易传错；新增字段时到处改签名。
- 方案：`pub struct Record { pub start: usize, pub len: usize }` + `pub struct Batch { pub payload: Vec<u8>, pub records: Vec<Record> }`；`Ring::append(&mut self, batch: Batch)`；`debug_assert` 保留。

**B3 退出码语义靠 `io::ErrorKind` 兼职**（中）

- 现状：`ring.rs:23-29` 用 `InvalidData`/`InvalidInput` 表示"数据错误/用法错误"，`main.rs:44-47` 据此映射退出码 2，其余映射 1；约定没有成文，`follow.rs` 返回的 io 错误也走同一通道。
- 问题：退出码是隐式跨模块契约；生产者新增错误时容易选错 kind；OS 自身产生的 `InvalidInput` 也可能被误判（概率低但存在）。
- 方案：**不引入自定义错误类型**（见 D3），只做两件事：`usage()`/`invalid()` 移到 `src/error.rs` 统一构造并写明约定；lib 提供 `is_user_error(kind)`，bin 的退出码映射只用这一处。

**B4 `pub use crc32c::crc32c` 泄露第三方 API**（低）

- 现状：`format.rs:33` 把第三方 crate 的函数当作 `format` 的公共 API。
- 问题：外部使用者会以为这是 ringlog 的 API；升级 crc32c 变成 API 变更。
- 方案：改为 `use crc32c::crc32c;` 内部使用，不 re-export。

### 2.5 耦合

**C1 cli → format 布局常量**（中）

- 现状：`src/cli.rs:14` import 布局常量，只为计算 `-s` 下限。
- 问题：参数层知道文件头几何；格式一改，参数层跟着改。
- 方案：R1 的 `MIN_TOTAL_SIZE` + `check_total_size`，cli 依赖"引擎公开的约束"而非布局常量。

**C2 `main` 知道太多实现细节**（中）

- 现状：`main.rs:100` 知道 `LineSplitter::new(ring.capacity())` 两个值必须相等；18-25 知道攒批阈值；176-206 知道 header 每个字段怎么排版。
- 问题：编排层同时是策略层，任何策略调整都要碰入口；这些知识也没有单测。
- 方案：S1 拆分后，`writer.rs` 承接"容量接线 + 阈值"，`info.rs` 承接渲染，`main` 只做分派。

**C3 库内直接写终端**（高）

- 现状：`follow.rs:31-37` 取 `io::stdout()` 并加锁；`crate::warn` 写 stderr；定位函数内部也走 `warn`。
- 问题：库函数会阻塞在当前进程的 stdout 上；无法把"跟随"当组件复用（输出到别的 writer、或只做解析）；也是 A2 的根因。
- 方案：`Tailer` 保持泛型 `W: Write`（已有），由 bin 创建 `StdoutLock`；事件由 bin 打印。库只做"读 + 定位 + 返回数据/事件"。

**C4 两个集成测试文件重复脚手架**（低）

- 现状：`tests/cli.rs` 与 `tests/adversarial.rs` 各自实现 `TestDir`、`bin()`、`run()`（约 60 行重复）。两者又各自实现文件格式解析——**这是故意的独立 oracle，应保留**。
- 问题：脚手架重复导致新测试要抄两遍；格式解析的"有意重复"没有注释说明，容易被后人误删。
- 方案：可选 `tests/common/mod.rs` 放 `TestDir`/`bin()`；在 adversarial 顶部注明"独立实现格式解析是有意为之"。

## 3. 目标结构

```
┌──────────────────── bin: src/main.rs + src/app/ ─────────────────────┐
│  main.rs   parse → dispatch → ExitCode（约 60 行）                     │
│  cli.rs    clap 定义、跨参校验、parse_size、Mode/ReadLines            │
│  writer.rs stdin 泵线程、攒批阈值、截断汇总                            │
│  follow.rs 跟随循环、事件 → 中文告警                                   │
│  info.rs   --info 渲染                                                 │
│  report.rs 唯一的 stdout/stderr 出口（替代 lib::warn）                 │
└───────────────────────────────┬───────────────────────────────────────┘
                                │ 只通过 pub API 使用
┌───────────────────────────────▼───────────────────────────────────────┐
│  lib: ringlog —— 纯引擎：无终端 I/O、无 clap、无进程环境、无中文文案   │
│  format.rs  布局常量 + Header + slot 编解码（crc32c 仅内部使用）      │
│  input.rs   分帧：LineSplitter → Batch{payload, records: Vec<Record>} │
│  ring.rs    Ring（写者）/ Reader（读者）/ 头读写 / lock_path           │
│  seek.rs    locate_start / scan_to_next_line / scan_back（纯读）      │
│  error.rs   usage()/invalid() 构造 + is_user_error 分类（可选）       │
└───────────────────────────────────────────────────────────────────────┘
```

依赖规则（写进 `docs/architecture.md`，review 时按此检查）：

1. lib 不依赖 bin；bin 只通过 `pub` API 使用 lib。
2. lib 内单向：`format ← ring ← seek`，`input` 独立。
3. 终端 I/O 只允许在 bin，且 stdout/stderr 写操作只出现在 `app/report.rs`（`info`/`follow` 的文案通过它输出）。
4. 用户可见文案（中文提示、退出码、帮助）只在 bin。
5. 文件格式知识只在 `format`；其他模块不得重算偏移/CRC（集成测试作为独立 oracle 除外）。

## 4. 分阶段修复方案

所有阶段都是**行为不变**的重构；每阶段结束跑完整验证（§5）并 review diff。

### Phase 1 —— 类型与契约收敛（规模 S，风险低）

| 任务 | 涉及问题 | 改动点 |
| --- | --- | --- |
| 1.1 `MIN_TOTAL_SIZE` + `check_total_size` | R1, C1 | `format.rs`、`cli.rs`、`ring.rs` |
| 1.2 `Record` / `Batch` 替换裸元组 | B2 | `input.rs`、`ring.rs`、`main.rs` |
| 1.3 `Mode` / `ReadLines` 三态 | B1 | `cli.rs`、`main.rs`、`follow.rs::run` 签名 |
| 1.4 crc32c 不再 re-export | B4 | `format.rs` |

验收：67 测试全绿；`cli` 单测改到新类型；新增 `input`/`cli` 边界单测；`--help` 与错误文案逐字节不变。

### Phase 2 —— 职责归位（规模 M，风险中低）

| 任务 | 涉及问题 | 改动点 |
| --- | --- | --- |
| 2.1 抽 `seek.rs` + 单测 | S2 | 新文件 `src/seek.rs`；`follow.rs` 变薄 |
| 2.2 拆 `main.rs` | S1, C2 | 新 `app/writer.rs`、`app/info.rs`、`app/report.rs`；`main.rs` 变薄 |
| 2.3 `take(final_chunk: bool)` | R2 | `input.rs`、`main.rs` |

验收：既有 e2e 全绿；`seek` 新增的 5~8 个单测覆盖：窗口对齐/不对齐、`-n` 为 0/1/N/超过行数/`u64::MAX`、接缝丢弃、无换行数据。

### Phase 3 —— lib/bin 边界重排（规模 M，风险中）

| 任务 | 涉及问题 | 改动点 |
| --- | --- | --- |
| 3.1 `cli.rs`、`follow.rs` 迁入 `src/app/` | A1 | 文件移动 + `mod` 声明；`lib.rs` 移除 `pub mod cli/follow` |
| 3.2 告警事件化，删 `lib::warn` | A2, C3 | `follow.rs` 返回/打印事件；`report.rs` 承接；lib 不再 `io::stdout` |
| 3.3 lib 公开面收敛 + 文档注释 | A1, A3 | `lib.rs` 只 `pub mod format/input/ring/seek` |

验收：67 测试全绿；`rg 'stdout|stderr|clap' src/` 在 lib 文件中无命中；golden 对照一致。

### Phase 4 —— 文档与收尾（规模 S，风险低）

| 任务 | 涉及问题 | 改动点 |
| --- | --- | --- |
| 4.1 `docs/architecture.md` | A3 | 目标结构图 + 依赖规则 + 不变量归属表 |
| 4.2 README 增「代码结构」指引 | A3 | README 末尾 |
| 4.3 `tests/common/mod.rs` + 注释 | C4 | 测试脚手架（可选） |
| 4.4 `error.rs` + `is_user_error` | B3 | 可选，若 Phase 1-3 顺利再做 |

## 5. 验证方案

1. **每阶段**：`cargo test --offline` 全绿、`cargo clippy --all-targets --offline -- -D warnings` 干净。
2. **Golden 对照**（Phase 1 前生成基线，存 `/tmp`，不提交）：
   - 固定输入（1 MiB 周期文本 + 1 MiB 随机字节）写入 64 KiB 环形文件；
   - 比较文件 `sha256sum`、`-n 1` / `-n 100` / 全量输出、`-f -n 0` 后追加数据、`--info`（剔除路径行）、`--help`、`--version`、10 组错误参数（stdout + stderr + 退出码）。
3. **性能冒烟**：200 MiB 过 64 MiB 环形，耗时与基线同量级（README 的 0.31 s 仅作参照，不设硬阈值）。
4. **改动纪律**：每阶段 diff 只允许结构性改动；若不得不连带改逻辑，单独拆出并在本文件记录原因。
5. **崩溃一致性**：沿用现有 `adversarial.rs`（SIGKILL、头部撕裂、并发压测），不新增机制。

## 6. 待拍板决策

| 编号 | 决策 | 推荐 | 备选与代价 |
| --- | --- | --- | --- |
| D1 | lib 边界方案 | **引擎 lib + 应用 bin**，`cli`/`follow` 迁入 bin（破坏 lib 公开 API，但仓库无外部消费者、集成测试不链接 lib，零测试影响） | 只在 lib 内部整理（省约 2~3 小时，但 A1/A2/C3 无法根治） |
| D2 | `seek` 单测方式 | **用 `Ring` 造真实临时文件**，不引入抽象 | 引入 `ReadAt` trait + 内存实现（更纯，但为测试增加一层抽象） |
| D3 | 是否引入自定义错误类型 | **不引入**，保留 `io::Error`，只集中构造与分类 | 引入 `enum Error`（干净，但侵入所有签名与 e2e 文案，收益低） |
| D4 | 是否把 `ring.rs` 拆成目录 | **不拆**（384 行、内聚可接受），只把 `open` 内部函数化 | 拆 `ring/{writer,reader,header}.rs`（文件更小，但增加跳转成本） |
| D5 | 文档与 CI | **本文保留 + 新增 `docs/architecture.md`**；CI/rfmt 作为独立后续任务 | 一并加 CI（需另定平台与缓存策略，超出本次范围） |

## 7. 明确不做

- 不改文件格式 v1（slot 布局、CRC 范围、不变量）。
- 不改 CLI 行为、帮助文案、退出码语义、错误信息文本。
- 不新增任何依赖（不加 `thiserror`/`anyhow`/`proptest`）。
- 不把 `follow` 做成 `Iterator`/async，不改"锁内只拷贝、锁外写 stdout"的设计。
- 不为单测覆盖率达到某个数字而写测试；只补关键边界（定位、分帧、模式互斥）。
- 不把 `tests/adversarial.rs` 的"独立格式解析"合并进 lib——那是黑盒测试的独立 oracle，有意重复。

## 8. 实施记录

分支 `refactor/architecture`，按阶段提交；除文档外无功能变更（golden 对照逐字节一致）。

| commit | 阶段 | 内容 |
| --- | --- | --- |
| `d1b8d62` | — | 本文档 |
| `e02982a` | Phase 1 | `Cli` → `Mode`/`StartPos`；`Batch`/`Record`；尺寸几何收敛到 `format`；crc32c 不再外置 |
| `dacfd25` | Phase 2 | 抽 `seek`（+5 单测）；`main` 拆 `app::{writer,info,report}`；`take(final_chunk)` |
| `d4a8709` | Phase 3 | `cli`/`follow` 迁入 bin；lib 收缩为引擎；删 `lib::warn`；错误分类集中到 `error` |
| `f11d723` | Phase 4 | `Ring::open` 拆成 4 个函数；`tests/common` 共享脚手架 |

与方案的差异（落地时按实际情况调整）：

- **D2**：不加 `ReadAt` trait；`seek` 单测直接用 `Ring` 造真实临时文件，数据即真实环形内容。
- **D3**：不引入自定义错误类型；保留 `io::Error`，只新增 `error.rs` 收敛构造与 `is_user_error`。
- **D4**：`ring.rs` 不拆目录；只把 `open` 拆成 `acquire_writer_lock / open_file / initialize / resume`。
- 额外：bin 也加上 `#![deny(unsafe_code)]`（原方案在 S4 里列为低优先）。

最终验证：

- `cargo test`：**75 通过**（lib 单测 16 + bin 单测 12 + 黑盒 27 + 对抗 20）
- `cargo clippy --all-targets -- -D warnings`、`cargo fmt --check`：干净
- golden 对照：74 项产物（文件 sha256、各模式 stdout/stderr/退出码、帮助与错误路径）
  与重构前**逐字节一致**

## 9. 后续优化（放开行为一致性之后）

第一轮重构以「行为完全不变」为约束。之后放开该约束，做了两项可测优化：

| 指标 | 优化前 | 优化后 | 手段 |
| --- | --- | --- | --- |
| 写 200 MiB 短行日志（44 B/行） | 0.213 s（≈985 MB/s） | **0.074 s（≈2.7 GB/s）** | `input` 快路径整块拷贝、不再逐行登记记录边界；`ring` 不跨窗口时整批一次写，跨窗口才扫描换行切分 |
| `-f` 端到端延迟 | 96–101 ms | **16–20 ms** | 写者低速率兜底 100 → 20 ms（`--sync` 仍 100 ms）；读者轮询默认 100 → 20 ms |
| 空闲跟随 CPU | < 0.4% | < 0.4%（不变） | — |
| dump 64 MiB | ≈25 ms | ≈25 ms（不变） | 读路径未改 |

附带的行为差异（日志内容不受影响）：

- `--info` 的「头部序号」变小：批次不再按 4096 条拆分，同样输入从 3 批变 2 批。
- `--help` 中轮询间隔默认值改为 20。
- 日志数据本身逐字节一致：golden 的 dump / `-n` / `-f` 输出全部一致，
  仅文件 hash（头部序号）与 help 文案变化。

评估过但不做：

- inotify 替代轮询：可到微秒级且空闲零 CPU，但要引入平台依赖与 `unsafe`，削弱「引擎无 unsafe」。
- 读路径 mmap、写路径 iovec：收益取决于内核/磁盘，复杂度与收益不匹配。
- 记录级 CRC、多写者、时间索引：属于文件格式 v2 的方向，不混进性能优化。

