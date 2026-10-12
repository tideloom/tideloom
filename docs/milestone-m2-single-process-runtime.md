# M2：单进程运行时

日期：2026-10-12
前置：PR #8（显式续体）合并后开始。

## 目标与退出条件

M1 钉死了解释器核心的形状：`Definition`、`Continuation`、`start` / `resume`、四种 `Resume` 事件。M2 把它接成一个能跑的系统，但只在一个进程里：内存存储、进程内唤醒、进程内执行器。

退出条件只有一条，用一个 example 验收：一个包含 `set`、`call: http`、`try` 重试、`wait`、`fork`（cooperative 与 compete 各一个分支集合）、`run: workflow` 子工作流和 `emit` → `listen` 配对的工作流，在 `examples/single_process.rs` 里由 `Engine` 端到端跑完，期间进程不 sleep，时钟由测试注入。

不在 M2 范围内：SQL 存储、Kafka / NATS 总线、多进程、CLI、metrics、`listen.foreach`、`run: shell` / `script` / `container`、`call: grpc` / `openapi` / `asyncapi`、HTTPS 与认证、schema 校验、`schedule`。

## 全局约束（每个 MR 都适用）

1. `tideloom-core` 保持纯函数：不依赖 tokio、不做 I/O、不读时钟、不持有 trait object。核心需要新能力时，只能通过扩展 `Pause` / `Block` / `Resume` / `Outcome` 这四个类型，并在 MR 描述里单独列出"核心 API 变更"一节。
2. 所有 I/O 进新 crate `tideloom-runtime`。三个 trait（`InstanceStore`、`Wakeup`、`ActivityExecutor`）按 ADR-0001 §9 的签名实现，用 `async_trait` + `Arc<dyn ...>`，不用泛型参数。
3. `Continuation` 一旦落盘就是持久化格式。MR-1 给它加 `schema: u16` 字段，之后任何改动帧结构的 MR 必须同时写从上一版本读取的兼容路径，或在 MR 里说明为什么可以拒绝旧版本。
4. `store.apply` 与 `wakeup.commit_offset` 只能在 `Engine` 的一个函数里按这个顺序调用，其它地方禁止单独调用任一个。
5. 每个 MR 自带测试，测试文件名在下表固定；改了核心语义的 MR 同时改 README 对应小节；改了 ADR 决策的 MR 在 ADR 末尾加修订记录。
6. 一个 MR 只做一件事。表里标了"可并行"的才能同时开分支；其余按依赖顺序合入。
7. 分支名 `m2/<编号>-<短名>`，commit 首行 Conventional Commits，正文说明核心 API 变更与持久化格式变更。

## MR 列表

| 编号 | 名称 | 依赖 | 可并行 | 规模 |
|---|---|---|---|---|
| MR-0 | 本文档 + ADR 修订（crate 拆分、进行中 activity 策略） | PR #8 | 否 | 小 |
| MR-1 | `tideloom-runtime`：`Instance` / `Commit` / `InstanceStore` + 内存实现 | MR-0 | 否 | 中 |
| MR-2 | `Wakeup` trait + 进程内可重放实现 | MR-1 | 与 MR-9 并行 | 小 |
| MR-3 | `Engine` 循环：claim → resume → apply → notify；Timer / Retry / Activity | MR-2 | 否 | 大 |
| MR-4 | `task_execution` Journal 与进行中 activity 的崩溃策略 | MR-3 | 与 MR-5 并行 | 中 |
| MR-5 | `fork`：分支即子实例、join、compete 取消 | MR-3 | 与 MR-4 并行 | 大 |
| MR-6 | `run: workflow`：子工作流实例与定义注册表 | MR-5 | 与 MR-7 并行 | 中 |
| MR-7 | `emit` 与 `listen` 第一阶段：单事件、静态匹配、inbox | MR-3 | 与 MR-6 并行 | 大 |
| MR-8 | 表达式求值器扩容：jq 函数子集 | 无 | 随时 | 中 |
| MR-9 | 验收 example 与 M2 收尾 | MR-4 ~ MR-8 | 否 | 小 |

规模：小 < 300 行，中 300 ~ 800 行，大 > 800 行，含测试。

---

## MR-0：文档与 ADR 修订

**做什么**

- 提交本文档。
- ADR-0001 增加修订记录：§9 的三个 trait 落在 `tideloom-runtime`，核心 crate 不依赖它们；Open Question 1 定为"进行中的 activity 在 worker 崩溃后标记为 `unknown`，不自动重做，由 `task_execution.kind` 对应的执行器声明是否幂等后才允许自动重做"。
- ADR-0001 §3 补一句：`Continuation` 带 `schema` 版本号。

**边界**：不改任何代码。

---

## MR-1：`tideloom-runtime` 与 `InstanceStore`

**做什么**

- 新 crate `tideloom-runtime`，依赖 `tideloom-core`、`tokio`、`async-trait`、`thiserror`、`serde`。
- 类型：`InstanceId`（UUIDv7 字符串）、`Status { Runnable, Running, Waiting(WaitReason), Completed, Faulted, Cancelled }`、`WaitReason { Timer, Activity, Join, Child, Events }`、`Instance`、`Commit`、`StepResult`、`StoreError`。
- `Instance` 字段：`id`、`definition: Identity + ContentHash`、`parent: Option<ParentRef { id, branch: Option<usize> }>`、`status`、`continuation: Option<Vec<u8>>`（JSON 序列化的 `Continuation`，完成后为 `None`）、`version: u64`、`lease: Option<Lease { worker, until }>`、`wake_at: Option<Timestamp>`、`input`、`output: Option<Value>`、`fault: Option<Fault>`。
- `trait InstanceStore`：`create`、`get`、`claim(worker, now, limit)`、`apply(commit)`、`sweep(before)`、`children(parent)`，签名照 ADR §9，补 `children` 一项供 MR-5 用。
- `MemoryStore`：`Mutex<HashMap<InstanceId, Instance>>`。
- `tideloom-core`：`Continuation` 增加 `schema: u16` 字段，当前值 1，反序列化时版本不匹配返回错误。

**约束**

- `apply` 的 CAS 语义：`commit.expected_version != instance.version` 返回 `VersionConflict`，不改任何字段。
- `claim` 只返回 `status ∈ {Runnable, Waiting(Timer)}` 且 `wake_at <= now` 且无有效租约的实例，并在返回前写入租约和 `Running`。
- `sweep` 只返回租约到期的 `Running` 实例的 id，不改状态；改状态是 `Engine` 的事。
- 不实现引擎循环，不实现唤醒，不写 SQL。

**测试**：`tideloom-runtime/tests/memory_store.rs`。CAS 冲突、两个 worker 同时 claim 只有一个拿到、sweep 只返回过期租约、`children` 顺序按 branch。

---

## MR-2：`Wakeup`

**做什么**

- `Signal { instance_id, hint: Option<Hint>, offset: Offset }`，`Hint { Timer, ChildDone(InstanceId), Event(EventId) }`。
- `trait Wakeup { notify, watch(from: Offset) -> SignalStream, commit_offset }`。
- `LocalWakeup`：进程内环形缓冲 + `tokio::sync::Notify`，`watch(from)` 从缓冲里重放 `offset > from` 的信号再接实时流；缓冲满时丢最旧的。

**约束**

- 信号是 hint。丢信号只能导致延迟，不能导致错误；测试里必须有一个"信号全部丢失，靠 sweeper 兜底"的用例，但该用例在 MR-3 才能写，MR-2 只保证接口语义。
- `commit_offset` 幂等，重复提交同一 offset 不报错。

**测试**：`tideloom-runtime/tests/local_wakeup.rs`。重放、offset 推进、缓冲溢出。

---

## MR-3：`Engine` 循环

**做什么**

- `Engine { store, wakeup, executors: HashMap<TaskKind, Arc<dyn ActivityExecutor>>, clock, worker_id, definitions: Arc<dyn DefinitionSource> }`。`DefinitionSource` 是只读 trait：按 `Identity + ContentHash` 取 `Definition`，本 MR 给内存实现。
- `trait ActivityExecutor { fn kind(&self) -> TaskKind; async fn execute(&self, task: TaskExecution) -> Result<Value, Fault> }`。本 MR 把现有 `http_call` 包成 `HttpExecutor`；`emit` 暂时给一个 `LogExecutor`（MR-7 替换）。
- 一步 = 一次 `step(instance)`：取定义，`start` 或 `resume`，把 `Outcome` 翻译成 `Commit`：
  - `Completed` / `Faulted` → 终态，`continuation = None`。
  - `Blocked(Timer)` → `Waiting(Timer)`，`wake_at = task_started + wait duration`。
  - `Blocked(Retry)` → `Waiting(Timer)`，`wake_at = pause.retry_at(task_started)`。
  - `Blocked(Activity)` → 同步调用执行器（M2 内执行器与解释器同进程），结果作为 `Resume::Output` / `Fault` 立即再 `resume`，循环到非 Activity 的停点再 `apply`。
  - `Blocked(Join | Child | Events)` → `Waiting(对应原因)`，本 MR 不处理唤醒条件，留给 MR-5 / 6 / 7。
- `Engine::run_once(now)`：sweep 过期租约 → 状态改回 `Runnable`；claim；逐个 `step`；`apply` + `commit_offset` 在 `Engine::commit` 一个函数里完成；成功后 `notify`。
- `Engine::drive_until_idle(clock)` 测试辅助：循环 `run_once` 直到没有可领取实例，时钟由测试推进。

**约束**

- `Engine` 不 sleep，不读系统时钟，所有时间来自 `Clock` trait。
- `step` 内部可以多次 `resume`（Activity 同步执行），但只 `apply` 一次；中途崩溃的语义是整段重做，这是 MR-4 要处理的事，本 MR 先接受。
- 工作流超时与任务超时用 `Resume::Tick` 在 claim 之后、`resume` 之前检查一次。

**核心 API 变更**：`Block` 增加 `wait_after() -> Option<Duration>`，把 `wait` 任务的时长从 body 解析出来，解释器仍不 sleep。

**测试**：`tideloom-runtime/tests/engine.rs`。`wait` 工作流在时钟推进后完成；`try` 重试经 `wake_at` 再领取；工作流超时在 `Tick` 落为 `Faulted`；丢掉全部信号后 sweeper 仍让实例完成；两个 worker 并发 `run_once` 不重复执行同一步。

---

## MR-4：`task_execution` Journal 与崩溃策略

**做什么**

- `StepResult { instance_id, key: TaskKey, kind, attempt, input, output: Result<Value, Fault>, started_at, finished_at }` 进入 `Commit.step_results`，`MemoryStore` 按 `(instance_id, key)` 保存。
- Activity 执行前先 `apply` 一条 `pending` 的 `StepResult`（attempt 递增），执行后再 `apply` 结果。这是 DBOS 的"副作用前写 checkpoint、副作用后写结果"纪律。
- 崩溃后再次 claim 到同一实例时，若最近的 `StepResult` 是 `pending`：执行器声明 `idempotent()` 为真则重做，否则实例置 `Faulted`，fault 类型为 runtime，detail 写明 `activity outcome unknown`。

**约束**

- Journal 只写不读：解释器不从 `task_execution` 恢复任何状态，这是和 replay 模式的分界线。
- `ActivityExecutor` 新增 `fn idempotent(&self) -> bool`，`HttpExecutor` 返回 false。

**测试**：`tideloom-runtime/tests/journal.rs`。一次 http 调用产生 pending 与 done 两条记录；模拟执行后崩溃（在两次 apply 之间中断）再 claim，非幂等执行器导致 `Faulted`。

---

## MR-5：`fork`

**做什么**

- 核心：`start_at(definition, position, input)`，从任意 `do` 列表节点开始一个续体，用于分支；`Pause::Join` 的 `Block` 暴露 `branches() -> &[Branch { position, name }]`。
- 引擎：遇到 `Join` 时在同一个 `Commit.spawn_children` 里创建全部分支实例（`parent = { id, branch: i }`，输入为 fork 的输入），父实例 `Waiting(Join)`。
- 分支实例的终态 `Commit` 触发父实例重算：`children(parent)` 全部终态 → cooperative 下全部成功则 `Resume::Release(按 branch 顺序的输出数组)`，任一失败则父实例 `Faulted` 并把其余 `Running` / `Waiting` 分支置 `Cancelled`；compete 下首个成功即 `Release(该输出)` 并取消其余，全部失败才 `Faulted`。
- 取消：`Cancelled` 是终态，被取消的分支下次被 claim 时直接丢弃，不再 `resume`。

**约束**

- fork 是错误边界：分支内未捕获的 fault 只到父实例的 join 判定，不穿透到 fork 外层的 `try`（ADR §6）。
- 重算父实例的操作必须幂等：两个分支同时完成、各自触发一次重算，结果一致。靠 `apply` 的 CAS 保证，重算失败就丢弃，由后到的那次再做。
- 不处理分支内的 `listen`、`run: workflow`，那是 MR-6 / 7 的事。

**测试**：`tideloom-runtime/tests/fork.rs`。cooperative 三分支输出顺序；compete 首个完成取消其余；分支 fault 不被外层 try 捕获；两个分支同时完成只产生一次 Release。

---

## MR-6：`run: workflow`

**做什么**

- `DefinitionSource` 增加按 `(namespace, name, version)` 查找；`Pause::Child` 的 `Block` 暴露 `child() -> ChildRef { identity, input, await }`。
- 引擎：创建子实例（`parent = { id, branch: None }`），父实例 `Waiting(Child)`；子实例终态触发父实例 `Resume::Output(子输出)` 或 `Resume::Fault(子 fault)`。`await: false` 时父实例立即 `Resume::Output(子实例 id)`。
- 子实例的 `workflow_started` 取创建时的时钟。

**约束**

- 复用 MR-5 的子实例机制和重算路径，不得另起一套。
- 子工作流的定义版本在创建时固定，父实例续体里不存子定义。

**测试**：`tideloom-runtime/tests/child_workflow.rs`。同步等待、`await: false`、子工作流 fault 被父 `try` 捕获、递归调用自身三层。

---

## MR-7：`emit` 与 `listen` 第一阶段

**做什么**

- `CloudEvent` 类型与 `trait EventSink { async fn publish(&self, event: CloudEvent) }`；`EmitExecutor` 从 `emit.event.with` 构造事件并 `publish`。
- `trait EventSource`：按静态字段（`type`、`source`、`subject`）索引的订阅表与 per-instance inbox。`LocalEventBus` 同时实现 `EventSink` 与 `EventSource`，进程内闭环。
- 核心：`Pause::Events` 的 `Block` 暴露 `listen() -> ListenSpec { strategy: One | Any | All, filters: Vec<Filter { r#type, source, subject, correlate: Vec<...> }> }`，只解析静态字段，`until` 与表达式型 correlation 本 MR 拒绝（build 期报 `BuildError`）。
- 引擎：`Waiting(Events)` 时登记订阅；事件到达写 inbox 并 `notify`；claim 后若 inbox 满足 strategy 则 `Resume::Release(事件数组)`，否则继续等待。消费位点在 `apply` 成功后才推进。

**约束**

- 不做 `foreach`，不做 `until`，不做 correlation 表达式，不做超时（listen 的超时走任务 `timeout`，已由核心支持）。
- 事件匹配先走索引再做精确比较；本 MR 的精确比较只有字符串相等。

**测试**：`tideloom-runtime/tests/listen.rs`。`emit` 后 `listen one` 被唤醒；`all` 需要两类事件；事件先于 listen 到达时不丢失；不匹配的事件不唤醒。

---

## MR-8：表达式求值器扩容

**做什么**

- 增加 jq 子集：`map`、`select`、`length`、`has`、`keys`、`add`、`tostring`、`tonumber`、`ascii_downcase`、`split`、`join`、`startswith`、`endswith`、`contains`、`if then else end`、`//` 默认值、`[.[] | ...]` 迭代。
- 每个函数一条文档行写进 README 的表达式小节。

**约束**

- 求值器保持纯函数，无副作用，不读时钟，不访问网络；不支持的语法仍是 expression fault。
- 不引入外部 jq 实现；如果后续决定换成 `jaq`，是另一个 ADR。
- 不改 `walk.rs`。

**测试**：`tideloom-core/src/expr.rs` 内单元测试，每个函数至少一个正例一个错误用例。

---

## MR-9：验收 example 与收尾

**做什么**

- `examples/single_process.rs`：退出条件里描述的那个工作流，YAML 放 `examples/single_process.yaml`。
- `README` 增加"运行一个工作流"小节；ADR-0001 增加 M2 完成的修订记录，列出 M3（SQL 存储与多进程）的入口条件。
- 清理：删除 `drive` / `drive_from`，它们的职责已被 `Engine` 覆盖；测试改用 `Engine`。

**约束**

- 不加新功能。发现缺口回到对应 MR 补，而不是在这里顺手做。

---

## 开 MR 时的检查清单

- [ ] 标题写清编号，如 `M2 MR-5: fork branches as child instances`
- [ ] 描述里有"核心 API 变更"与"持久化格式变更"两节，没有就写"无"
- [ ] 新增测试文件名与本文档一致
- [ ] 触碰 `tideloom-core` 的 MR，`cargo test -p tideloom-core` 不依赖 `tideloom-runtime`
- [ ] `cargo clippy --all-targets`、`cargo +nightly fmt --check`、`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` 干净
- [ ] 改了 ADR 决策的，ADR 末尾有修订记录
