# M2：单进程运行时

日期：2026-10-12
前置：PR #8（显式续体）、PR #9（本文档与 ADR 修订）、MR-S（接口脚手架）合并后开始。

## 目标与退出条件

M1 钉死了解释器核心的形状：`Definition`、`Continuation`、`start` / `resume`、四种 `Resume` 事件。M2 把它接成一个能跑的系统，但只在一个进程里：内存存储、进程内唤醒、进程内执行器。

退出条件只有一条，用一个 example 验收：一个包含 `set`、`call: http`、`try` 重试、`wait`、`fork`（cooperative 与 compete 各一个分支集合）、`run: workflow` 子工作流和 `emit` → `listen` 配对的工作流，在 `examples/single_process.rs` 里由 `Engine` 端到端跑完，期间进程不 sleep，时钟由测试注入。

不在 M2 范围内：SQL 存储、Kafka / NATS 总线、多进程、CLI、metrics、`listen.foreach`、`run: shell` / `script` / `container`、`call: grpc` / `openapi` / `asyncapi`、HTTPS 与认证、schema 校验、`schedule`。

## 全局约束（每个 MR 都适用）

完整版在 `AGENTS.md` 的 "Non-negotiable rules"，这里只列与 MR 切分直接相关的：

1. `tideloom-core` 保持纯函数：不依赖 tokio、不做 I/O、不读时钟、不持有 trait object。核心需要新能力时，只能通过扩展 `Pause` / `Block`（经 `BlockDetail`）/ `Resume` / `Outcome` 这四个类型，并在 MR 描述里单独列出"Core API changes"一节。
2. 所有 I/O 在 `tideloom-runtime`。trait 已由 MR-S 定义，实现 MR 不改 trait 签名；需要改签名时单独开一个小 MR 先行合入，并说明哪个 MR 需要它。
3. `Continuation` 带 `schema` 版本号（`tideloom_core::SCHEMA`）。改帧结构的 MR 必须同时 bump 版本并提供从上一版本读取的路径，或说明为什么可以拒绝旧版本，写在"Persistence format changes"一节。
4. `store.apply` 与 `wakeup.commit_offset` 只能通过 `Engine::commit` 调用，其它地方禁止单独调用任一个。MR-S 已把这个函数放好。
5. 每个 MR 自带测试，测试文件名在下表固定；改了核心语义的 MR 同时改 README 对应小节；改了 ADR 决策的 MR 在 ADR 末尾加修订记录。
6. 一个 MR 只做一件事，只碰"文件归属"表分配给它的文件。标了"可并行"的才能同时开分支。
7. 分支名 `m2/<编号>-<短名>`，MR 标题 `M2 MR-<编号>: <短名>`，commit 首行 Conventional Commits。

## MR 列表

| 编号 | 名称 | 依赖 | 可并行 | 规模 |
|---|---|---|---|---|
| MR-0 | 本文档 + ADR 修订 | PR #8 | 否 | 小（已合入） |
| MR-S | 接口脚手架：`tideloom-runtime` 的 trait 与类型、核心 `BlockDetail` / `start_at` / `schema`、`AGENTS.md` | MR-0 | 否 | 中 |
| MR-1 | `MemoryStore`：`InstanceStore` 的内存实现 | MR-S | 与 MR-2、MR-8 并行 | 中 |
| MR-2 | `LocalWakeup`：`Wakeup` 的进程内可重放实现 | MR-S | 与 MR-1、MR-8 并行 | 小 |
| MR-3 | `Engine` 循环：claim → resume → apply → notify；Timer / Retry / Activity | MR-1、MR-2 | 否 | 大 |
| MR-4 | `task_execution` Journal 与进行中 activity 的崩溃策略 | MR-3 | 与 MR-5、MR-7 并行 | 中 |
| MR-5 | `fork`：分支即子实例、join、compete 取消 | MR-3 | 与 MR-4、MR-7 并行 | 大 |
| MR-6 | `run: workflow`：子工作流实例 | MR-5 | 与 MR-7 并行 | 中 |
| MR-7 | `emit` 与 `listen` 第一阶段：`LocalEventBus`、inbox、唤醒 | MR-3 | 与 MR-4、MR-5、MR-6 并行 | 大 |
| MR-8 | 表达式求值器扩容：jq 函数子集 | 无 | 随时 | 中 |
| MR-9 | 验收 example 与 M2 收尾 | MR-4 ~ MR-8 | 否 | 小 |

规模：小 < 300 行，中 300 ~ 800 行，大 > 800 行，含测试。

## 文件归属

每个 MR 只在自己的行里创建或修改文件。同一行以外的文件需要改动时，先开独立的小 MR。

| MR | 新建 | 修改 | 禁止触碰 |
|---|---|---|---|
| MR-1 | `tideloom-runtime/src/store/memory.rs`、`tideloom-runtime/tests/memory_store.rs` | `tideloom-runtime/src/store.rs`（仅 `mod memory;` 与 re-export）、`src/lib.rs`（re-export） | trait 签名、`commit.rs`、`instance.rs` |
| MR-2 | `tideloom-runtime/src/wakeup/local.rs`、`tests/local_wakeup.rs` | `src/wakeup.rs`（仅 mod 与 re-export）、`src/lib.rs`、`Cargo.toml`（加 tokio） | trait 签名 |
| MR-3 | `tideloom-runtime/src/engine/step.rs`、`src/engine/translate.rs`、`src/executor/http.rs`、`src/executor/log.rs`、`tests/engine.rs` | `src/engine.rs`（加 `run_once` / `drive_until_idle`，不改 `commit`）、`src/lib.rs` | `tideloom-core/src/walk.rs`、trait 签名 |
| MR-4 | `tideloom-runtime/src/engine/step.rs`、`tideloom-runtime/tests/journal.rs` | `src/engine.rs`（`run_once` / `step`，不改 `commit`）、`src/store/memory.rs`（journal 存取；租约记下未完成的 attempt）、`src/instance.rs`（`Lease::open_attempt`）、`src/lib.rs`（re-export） | `commit.rs` 中 `StepResult` 的字段 |
| MR-5 | `tideloom-runtime/src/engine/join.rs`、`tests/fork.rs` | `src/engine/translate.rs`（Join 分支）、`src/engine/step.rs`（子实例终态触发父实例重算） | `tideloom-core`（`start_at` 与 `Branches` 已就绪） |
| MR-6 | `tideloom-runtime/src/engine/child.rs`、`tests/child_workflow.rs` | `src/engine/translate.rs`（Child 分支）、`src/engine/join.rs`（复用重算路径） | `tideloom-core`（`ChildRef` 已就绪） |
| MR-7 | `tideloom-runtime/src/events/local.rs`、`src/executor/emit.rs`、`src/engine/listen.rs`、`tests/listen.rs` | `src/events.rs`（仅 mod 与 re-export）、`src/engine/translate.rs`（Events 分支）、`src/engine/step.rs`（claim 后检查 inbox） | `tideloom-core`（`ListenSpec` 已就绪；需要 `until` / `correlate` 时另开核心 MR） |
| MR-8 | 无 | `tideloom-core/src/expr.rs`、README 表达式小节 | `tideloom-core/src/walk.rs` |
| MR-9 | `examples/single_process.rs`、`examples/single_process.yaml` | README、ADR 修订记录；删除 `tideloom-core/src/http.rs` 中的 `drive*` 与对应测试 | 其它 |

---

## MR-S：接口脚手架（已随本修订提交）

**做了什么**

- 新 crate `tideloom-runtime`：`Instance` / `InstanceId` / `Status` / `WaitReason` / `ParentRef` / `Lease` / `DefinitionKey`、`Commit` / `StepResult` / `StepOutcome` / `SpawnChild`、trait `InstanceStore` / `Wakeup` / `ActivityExecutor` / `DefinitionSource` / `EventSink` / `EventSource` / `Clock`、`Engine` 结构与 `Engine::commit`。只带三个足够小的实现：`ManualClock`、`SystemClock`、`MemoryDefinitions`。
- 核心：`Continuation` 加 `schema` 与 `root`；`start_at` 从任一任务启动（fork 分支）；`Block::detail()` 返回 `BlockDetail`（`Wait` / `Branches` / `Child` / `Listen`）。
- `AGENTS.md`、`docs/runtime-architecture.md`、本文档的文件归属表。

**边界**：不实现任何 I/O 后端，不实现 `Engine` 循环。

---

## MR-1：`MemoryStore`

**做什么**

- `MemoryStore`：`Mutex<HashMap<InstanceId, Instance>>` 加 journal 的 `BTreeMap<(InstanceId, TaskKey, u32), StepResult>`，实现 `InstanceStore` 全部方法。

**约束**

- `apply` 的 CAS 语义：`commit.expected_version != instance.version` 返回 `VersionConflict`，不改任何字段。成功时 `version + 1`，`spawn_children` 插入、`cancel` 中非终态的置 `Cancelled`，与父实例变更在同一把锁内完成。
- `claim` 只返回 `status ∈ {Runnable, Waiting(Timer)}` 且 `wake_at <= now` 且无有效租约的实例，按 `wake_at` 升序，返回前写入租约并置 `Running`。
- `sweep` 只返回租约在 `before` 之前到期的 `Running` 实例 id，不改状态。
- `children` 按 `parent.branch` 升序，再按创建时间。

**测试**：`tideloom-runtime/tests/memory_store.rs`。CAS 冲突不改字段；两个 worker 同时 claim 只有一个拿到；sweep 只返回过期租约；`spawn_children` 与父状态原子落地；`cancel` 跳过终态实例；`children` 顺序。

---

## MR-2：`LocalWakeup`

**做什么**

- `LocalWakeup`：进程内环形缓冲 + `tokio::sync::Notify`。`watch(from)` 先重放缓冲里 `offset > from` 的信号再接实时流；缓冲满时丢最旧的，`from` 早于保留范围时返回 `BusError::OffsetExpired`。
- `tideloom-runtime/Cargo.toml` 加 `tokio`（`sync`、`rt`、`macros`、`time` 仅测试用）。

**约束**

- `commit_offset` 幂等且单调；`committed()` 返回最后提交的 offset。
- 不实现任何路由或分区，所有 worker 看同一条流。

**测试**：`tideloom-runtime/tests/local_wakeup.rs`。重放、offset 推进、缓冲溢出报错、并发 notify 的 offset 严格递增。

---

## MR-3：`Engine` 循环

**做什么**

- `Engine::run_once(now) -> Result<usize, EngineError>`：sweep 过期租约 → 状态改回 `Runnable`；claim；逐个 `step`；返回处理的实例数。
- `Engine::drive_until_idle(clock: &ManualClock)` 测试辅助：循环 `run_once` 直到没有可领取实例且没有未来的 `wake_at`；若有，推进时钟到最早的 `wake_at` 再继续。
- `step(instance)` 按 `docs/runtime-architecture.md` 的 "One engine step" 实现：解码续体（`None` → `start`；`root_task` 为 `Some` 的情况留给 MR-5）；Activity 同步调用执行器后立即再 `resume`，循环到非 Activity 停点再 `apply` 一次；`Blocked(Join | Child | Events)` 置对应 `Waiting`，唤醒条件留给 MR-5 / 6 / 7。
- `HttpExecutor` 包现有 `http_call`；`LogExecutor` 处理 `emit`（MR-7 替换）。
- 工作流超时与任务超时：claim 之后、`resume` 之前先 `Resume::Tick` 一次。

**约束**

- 不 sleep，不读系统时钟，所有时间来自 `Clock`，一次 `step` 只读一次。
- `step` 内部可以多次 `resume`，但只 `apply` 一次；中途崩溃的语义是整段重做，MR-4 处理。
- 不改 `Engine::commit`，不改任何 trait。

**测试**：`tideloom-runtime/tests/engine.rs`。`wait` 工作流在时钟推进后完成；`try` 重试经 `wake_at` 再领取；工作流超时在 `Tick` 落为 `Faulted`；丢掉全部信号后 sweeper 仍让实例完成；两个 `Engine` 共享一个 store 并发 `run_once` 不重复执行同一步。

---

## MR-4：`task_execution` Journal 与崩溃策略

**做什么**

- Activity 执行前先 `Engine::commit` 一条 `StepOutcome::Pending`（`release_lease = false`），执行后再提交结果。
- 崩溃后再次 claim 到同一实例时，若最近的 `StepResult` 是 `Pending`：执行器 `idempotent()` 为真则 `attempt + 1` 重做；否则提交 `StepOutcome::Unknown` 并把实例置 `Faulted`，fault 为 runtime，detail 为 `activity outcome unknown`。

**约束**

- Journal 只写不读：解释器不从 `task_execution` 恢复任何状态，这是和 replay 模式的分界线。唯一的读取是上面这条恢复判定，且它只决定"重做还是 fault"，不决定"在哪里"。
- 不改 `StepResult` 的字段。

**测试**：`tideloom-runtime/tests/journal.rs`。一次 http 调用先提交 `Pending` 再把同一 `(instance, key, attempt)` 行 upsert 为 `Output`（`Commit::step_results` 的现行契约是 upsert，不是追加两条记录；MR-1 已按此实现并测试）。模拟执行后崩溃（在两次提交之间中断）再 claim，非幂等执行器导致 `Faulted` 且 journal 里有 `Unknown`；幂等执行器重做且 `attempt` 为 1。

---

## MR-5：`fork`

**做什么**

- `translate`：`Blocked(Join)` 时用 `BlockDetail::Branches` 在同一个 `Commit.spawn_children` 里创建全部分支实例（`parent = { id, branch: i }`，输入为 fork 的输入），父实例 `Waiting(Join)`。
- `step`：实例 `root_task` 为 `Some` 时用 `start_at` 启动；分支实例的终态 `Commit` 之后 `notify(parent, ChildDone)`。
- `join.rs`：父实例被 claim 后从 `children(parent)` 重算：cooperative 下全部 `Completed` → `Resume::Release(按 branch 顺序的输出数组)`，任一 `Faulted` → 父实例 `Faulted` 并 `cancel` 其余非终态分支；compete 下首个 `Completed` → `Release(该输出)` 并 `cancel` 其余，全部 `Faulted` 才 `Faulted`。
- 被取消的分支下次被 claim 时直接丢弃，不 `resume`。

**约束**

- fork 是错误边界：分支内未捕获的 fault 只到父实例的 join 判定，不穿透到 fork 外层的 `try`（ADR §6）。
- 重算必须幂等：两个分支同时完成、各自触发一次重算，结果一致。靠 `apply` 的 CAS 保证，冲突方丢弃。
- 不处理分支内的 `listen`、`run: workflow`。

**测试**：`tideloom-runtime/tests/fork.rs`。cooperative 三分支输出顺序；compete 首个完成取消其余；分支 fault 不被外层 try 捕获；两个分支同时完成只产生一次 Release；嵌套 fork。

---

## MR-6：`run: workflow`

**做什么**

- `translate`：`Blocked(Child)` 时用 `BlockDetail::Child` 经 `DefinitionSource::latest` 解析定义，创建子实例（`parent = { id, branch: None }`），父实例 `Waiting(Child)`；`await_result` 为 false 时父实例立即 `Resume::Output(子实例 id)`。
- 子实例终态触发父实例 `Resume::Output(子输出)` 或 `Resume::Fault(子 fault)`，复用 `join.rs` 的重算路径。

**约束**

- 子工作流的定义 key 在创建时固定，父实例续体里不存子定义。
- 不得另起一套子实例机制。

**测试**：`tideloom-runtime/tests/child_workflow.rs`。同步等待；`await: false`；子工作流 fault 被父 `try` 捕获；递归调用自身三层；子工作流定义未发布时父实例 `Faulted`。

---

## MR-7：`emit` 与 `listen` 第一阶段

**做什么**

- `LocalEventBus`：同时实现 `EventSink` 与 `EventSource`，进程内闭环；按 `ListenSpec` 的静态属性做订阅索引，per-instance inbox。
- `EmitExecutor`：从 `emit.event.with` 构造 `CloudEvent` 并 `publish`，替换 MR-3 的 `LogExecutor`。
- `translate`：`Blocked(Events)` 时 `events_in.subscribe(instance, spec)`，实例 `Waiting(Events)`；事件到达时 `notify(instance, Event(id))`。
- `step`：claim 后若 inbox 满足 strategy（`One` / `Any` 至少一条，`All` 每个 filter 至少一条）则 `Resume::Release(事件数组)` 并 `ack`，否则继续等待。`read` 为 `data` 时数组元素是 `data`，`envelope` 时是整个事件。

**约束**

- 不做 `foreach`、`until`、correlation 表达式，这些在核心层已经是 configuration fault；需要时先开核心 MR 扩展 `ListenSpec`。
- 事件匹配只有字符串相等。
- `ack` 与 `Resume::Release` 在同一个 `step` 里，`apply` 之后才 `ack`。

**测试**：`tideloom-runtime/tests/listen.rs`。`emit` 后 `listen one` 被唤醒；`all` 需要两类事件；事件先于 listen 到达时不丢失；不匹配的事件不唤醒；`read: envelope`。

---

## MR-8：表达式求值器扩容

**做什么**

- 增加 jq 子集：`map`、`select`、`length`、`has`、`keys`、`add`、`tostring`、`tonumber`、`ascii_downcase`、`split`、`join`、`startswith`、`endswith`、`contains`、`if then else end`、`//` 默认值、`[.[] | ...]` 迭代。
- 每个函数一条文档行写进 README 的表达式小节。

**约束**

- 求值器保持纯函数，无副作用，不读时钟；不支持的语法仍是 expression fault。
- 不引入外部 jq 实现；如果后续决定换成 `jaq`，是另一个 ADR。
- 不改 `walk.rs`。

**测试**：`tideloom-core/src/expr.rs` 内单元测试，每个函数至少一个正例一个错误用例。

---

## MR-9：验收 example 与收尾

**做什么**

- `examples/single_process.rs`：退出条件里描述的那个工作流，YAML 放 `examples/single_process.yaml`。
- README 增加"运行一个工作流"小节；ADR-0001 增加 M2 完成的修订记录，列出 M3（SQL 存储与多进程）的入口条件。
- 删除 `tideloom-core` 的 `drive` / `drive_from` / `drive_with`，测试改用 `Engine`。

**约束**

- 不加新功能。发现缺口回到对应 MR 补，而不是在这里顺手做。

---

## 开 MR 时的检查清单

- [ ] 标题写清编号，如 `M2 MR-5: fork branches as child instances`
- [ ] 描述里有 "Core API changes" 与 "Persistence format changes" 两节，没有就写 "none"
- [ ] 只碰"文件归属"表分配的文件
- [ ] 新增测试文件名与本文档一致，测试注入时钟、不 sleep
- [ ] `cargo test -p tideloom-core` 不依赖 `tideloom-runtime`
- [ ] `cargo clippy --workspace --all-targets`、`cargo +nightly fmt --check`、`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` 干净
- [ ] 改了 ADR 决策的，ADR 末尾有修订记录
