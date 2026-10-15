# M3：PostgreSQL 存储与双进程 worker

日期：2026-10-15
状态：草案，待评审
前置：M2 已在 `main` 合入（PR #10 ~ #20），本文档第一部分的加固 MR 合入后才开始 SQL 部分。

## 为什么先加固再上 SQL

M2 的审查（三路并行：引擎语义、后端实现、核心与文档）找到一个可复现的 blocker 和七个会被 SQL schema 固化的结构问题。它们都在内存实现里"碰巧能跑"，一旦写成表结构和事务就改不动了。所以 M3 分两段：H 段（hardening）只改内存实现和核心接口，S 段才写 SQL。H 段每个 MR 都小，但顺序不能颠倒。

M2 审查结论摘要，详细证据在各 MR 的"动机"里：

| 级别 | 问题 | 位置 |
|---|---|---|
| blocker | 父实例在同一轮里被重新 park，晚完成的分支的 release 因 CAS 快照过期被丢弃，父实例永远 `Waiting(Join)` | `engine/step.rs` `run_once` 的 `parent_versions` 快照 |
| major | `Engine` 持有 `known` id 列表并扫描它来找 due timer 和满足条件的 listen，跨进程不成立 | `engine.rs` `known`、`step.rs` `release_satisfied_listens` / `next_wake_at` |
| major | fork 分支的起点位置用 `"\u{0}tideloom.branch"` 键藏在 `Instance.input` 里 | `step.rs` `BRANCH_POSITION` |
| major | `EngineParts.clock` 从未被读取，`run_once(now)` 由调用方传时间，`drive_until_idle` 绑定 `ManualClock` 具体类型 | `engine.rs`、`step.rs` |
| major | activity 进行中状态 `Waiting(Activity)` 不被 `claim` 也不被 `sweep` 看到（`sweep` 只列 `Running`），崩溃恢复只靠测试辅助 `lapse_lease` 把状态改回 `Running` 才成立 | `step.rs`、`store/memory.rs` |
| major | MR-4 把 `open_attempt` 塞进 `Lease`，store 在 `apply` 时从本 commit 的 `step_results` 推导它；`open_attempt` 只是"本租约已写过 Pending"的进程内标记，引擎真正的恢复判定仍是 `steps()` 全量扫描。持久化格式变了但 MR 没声明 | `instance.rs`、`store/memory.rs`、`engine/step.rs` |
| major | `HttpExecutor::execute` 永远返回 fault，真正的 HTTP 路径是引擎对 `TaskKind::Http` 的特判；`ureq` 仍在 `tideloom-core` | `executor/http.rs`、`step.rs`、`core/http.rs` |
| major | `EmitExecutor` 自带一套表达式求值器，和核心的语义不一致 | `executor/emit.rs` |
| major | 引擎直接调用 `LocalEventBus::satisfied` / `release`，listen 语义绑定在一个实现上 | `step.rs`、`engine/listen.rs` |
| major | 表达式解析器与求值器无嵌套深度上限，恶意定义可以栈溢出杀死持有租约的 worker | `core/expr.rs` |
| minor | 新分支第一轮只 park 不执行；`rejoin` / `relisten` 用 `translate` 停住，`rechild` / `release_timer` 走 `finish_inline`；`wait` 与 retry 的 `wake_at` 都用 claim 时刻的 `now` 而不是 `task_started`；每次 activity 读整条 journal；公开 API 泄漏了 `notify_due`、`step_until_pending`、`UNKNOWN_DETAIL`、`ListenRelease`、`pub mod engine`、`LogExecutor` | 散布于 `step.rs`、`translate.rs`、`lib.rs` |
| minor | 验收 example 里父实例的 `listen` 在子工作流 `tell` 的 `emit` 之后，靠 `LocalEventBus` 把订阅之前的事件重放进 inbox 才能配对；AGENTS.md 三处过期（`http.rs` 仍写 test-only `drive`、测试清单指向不存在的 `tideloom-core/tests/http_call.rs`、deliberate 清单仍写 MR-9 将删除 `drive`）；README 未写 PR #17 的三条语义 | `examples/`、`AGENTS.md`、`README.md` |

做得好的部分同样记录在案，M3 不要破坏它们：`Engine::commit` 是唯一的 `apply` + `commit_offset` 调用点且顺序正确；`MemoryStore::apply` 先校验整条 commit 再改任何字段，父状态、子实例、取消、journal 在一把锁内落地；`translate.rs` 是纯的 Outcome 到 Commit 映射；fork 与 child 共用一条重算路径；listen 的 `ack` 在 `apply` 之后；库代码没有 `unwrap` / `sleep` / `SystemTime`；173 个测试全部注入时钟。

## 目标与退出条件

两个独立的 worker **进程**共享一个 PostgreSQL，跑完 M2 的验收工作流（`tideloom-runtime/examples/single_process.yaml`，H6 改过 listen/emit 顺序之后的那份），其中一个进程在 activity 执行中被 `SIGKILL`，另一个进程按 journal 纪律恢复并跑完。

退出条件用两个测试验收：

1. `tideloom-postgres/tests/two_workers.rs`：启动两个 worker 进程（同一二进制，不同 `WorkerId`），提交 M2 example 的工作流，外部注入 `listen` 等待的事件，断言 `Completed` 且输出与 M2 example 一致。跨进程的证据是 `workflow_instance.lease_worker`：分支或子工作流至少有一个实例的租约曾经属于另一个 worker。journal 行不带 worker 列，M3 不加。
2. `tideloom-postgres/tests/crash_recovery.rs`：worker A 在 `Pending` journal 行提交之后、执行器返回之前停住；worker B 的 sweeper 回收过期租约；非幂等执行器下实例 `Faulted` 且 journal 有 `Unknown`，幂等执行器下 `attempt + 1` 重做并完成。进程内两个 `Engine` 共享一个 `PgStore` 即可证明这条纪律；`kill -9` 不是退出条件，见 MR-S6。

两个测试都需要真实 PostgreSQL，通过环境变量 `TIDELOOM_TEST_DATABASE_URL` 提供，缺省时跳过；CI 的 ubuntu `test` job 用 service container。`two_workers.rs` 跨进程，无法注入时钟，是全仓库唯一允许读墙钟的测试。`crash_recovery.rs` 仍注入 `ManualClock`。等待一律用有上限的轮询，不用 `sleep` 定长。

不在 M3 范围内：Kafka / NATS、`listen.foreach` / `until` / correlation、`run: shell` / `script` / `container`、`call: grpc` / `openapi` / `asyncapi`、HTTPS 与认证、schema 校验、CLI、metrics、分区与冷热分离。

## 全局约束（在 M2 的基础上新增）

1. **SQL 里没有 `now()`。** 所有时间比较用参数传入的 `now`，store 不读数据库时钟。这让 SQL store 的测试仍能用 `ManualClock`。
2. **一个 `apply` 一个事务。** 父实例变更、子实例插入、取消、journal upsert 在同一个事务内；事务句柄不出 trait。
3. **`claim` 用 `FOR UPDATE SKIP LOCKED`，`apply` 用 `WHERE version = $expected`。** 这两句是互斥性和原子性的全部来源，其它地方不加锁。
4. **journal 仍然只写不读**，唯一的读取是崩溃恢复判定：某 key 最新一行是否 `Pending`。现行 `steps(instance)` 会把整条 journal 读进引擎，H2 把它收成 `latest_step(instance, key)`。
5. **事件不保留。** 事件只投递给已存在的订阅；订阅之前发出的事件丢失。这是 ADR §5 "等待状态可重算"的边界：可重算的是"已到 inbox 的事件"，不是"曾经发生过的事件"。
6. **持久化格式变更必须声明**。H 段改的是 `Instance` / `Commit` / `SpawnChild`，不改 `Continuation` 的帧，`SCHEMA` 保持 1。每个变更在 MR 描述里写明；SQL schema 以 H 段结束时的类型为准，不为旧形态留列。内存实现没有旧行要迁移。
7. 新 crate `tideloom-postgres` 放所有 SQL 实现，`tideloom-runtime` 不依赖 `sqlx`。
8. DDL 用 `sqlx::migrate!` 管理，放 `tideloom-postgres/migrations/`，文件名 `NNNN_<name>.sql`，只追加不修改。

## MR 列表

| 编号 | 名称 | 依赖 | 可并行 | 规模 |
|---|---|---|---|---|
| MR-0 | 本文档 + ADR 修订 | M2 | 否 | 小 |
| MR-H1 | 父实例 release 用最新版本；`rejoin` / `relisten` 改走 `finish_inline`；分支首轮直接执行 | MR-0 | 与 H2、H4、H5 并行 | 小 |
| MR-H2 | 持久化形态：`root_task` 列、`in_flight` 取代 `Lease.open_attempt`、删除 `WaitReason::Activity`、sweep 覆盖进行中 activity、`latest_step`、删除 `lapse_lease` | MR-0 | 与 H1、H4、H5 并行 | 中 |
| MR-H3 | 引擎读自己的时钟；`InstanceStore::wake` / `next_due`；删除 `known`；`drive_until_idle` 进 test-util | MR-H1、MR-H2 | 否 | 中 |
| MR-H4 | 传输层出核心：`http.rs` 与 `ureq` 搬进 runtime，`HttpExecutor` 成为真正的执行器，删除引擎特判；`BlockDetail::Emit` 由核心求值，删除 emit 私有求值器 | MR-0 | 与 H1、H2、H5 并行 | 中 |
| MR-H5 | 核心：表达式深度上限；`wait` 与 retry 的 `wake_at` 用 `task_started`；listen `satisfied` / `release` 变成引擎内的纯函数 | MR-0 | 与 H1、H2、H4 并行 | 小 |
| MR-H6 | 公开 API 收缩；AGENTS.md / README 刷新；删掉订阅前的事件重放；example 改为 listen 先于 emit | MR-H1 ~ H5 | 否 | 小 |
| MR-S1 | `tideloom-postgres` crate、migration 0001、`PgStore` 实现 `InstanceStore` | MR-H6 | 否（S2 / S3 依赖它的 crate） | 大 |
| MR-S2 | `PgWakeup`：`signal_log` + `signal_cursor`，`LISTEN` / `NOTIFY` 做 hint | MR-S1 | 与 S3、S4 并行 | 中 |
| MR-S3 | `PgEventBus`：`event_subscription` + `event_inbox`，publish 返回 landed ids，可选地在同一事务里 `wake` | MR-S1 | 与 S2、S4 并行 | 中 |
| MR-S4 | `PgDefinitions` 与 `definition` 表 | MR-S1 | 与 S5 并行 | 小 |
| MR-S5 | `Engine::run(shutdown)` 生产循环：signal 驱动 + sweep 定时 + `next_due` 轮询，优雅退出释放租约 | MR-H3、MR-H6 | 与 S2、S3、S4 并行 | 中 |
| MR-S6 | 双进程验收：`two_workers.rs`、`crash_recovery.rs`、CI service container | MR-S1 ~ S5 | 否 | 中 |

规模：小 < 300 行，中 300 ~ 800 行，大 > 800 行，含测试。

## 文件归属

| MR | 新建 | 修改 | 禁止触碰 |
|---|---|---|---|
| MR-H1 | 无（回归用例加进已有的 `tideloom-runtime/tests/fork.rs`） | `tideloom-runtime/src/engine/step.rs`（`run_once`、`release_parent`、`rejoin`、`relisten`）、`engine/translate.rs`（去掉分支首轮 park） | `instance.rs`、`commit.rs`、trait |
| MR-H2 | 无 | `tideloom-runtime/src/instance.rs`、`commit.rs`、`store.rs`（`latest_step`）、`store/memory.rs`、`engine/step.rs`（分支启动、journal 提交、恢复判定）、`tests/journal.rs`、`tests/memory_store.rs`、`tests/fork.rs`、`tests/contract.rs` | `tideloom-core` |
| MR-H3 | `tideloom-runtime/src/testing.rs`（feature `test-util`） | `engine.rs`、`engine/step.rs`、`store.rs`（加 `wake`、`next_due`）、`store/memory.rs`、`events.rs`（`publish` 返回 landed ids）、`events/local.rs`、`Cargo.toml`（`test-util`）、所有 runtime 测试与 `examples/single_process.rs` 的 harness | `tideloom-core` |
| MR-H4 | `tideloom-runtime/src/executor/http.rs`（原 `core/http.rs` 迁入，按需拆文件） | `tideloom-core/src/spec.rs`（`BlockDetail::Emit`）、`walk.rs`（emit 的 detail 求值）、`lib.rs`、`Cargo.toml`（去 `ureq`）；`tideloom-runtime/src/executor/http.rs`、`executor/emit.rs`、`engine/step.rs`（删除 Http 特判）、`tests/http_call.rs`、`Cargo.toml`（加 `ureq`） | `engine/join.rs`、`child.rs` |
| MR-H5 | 无 | `tideloom-core/src/expr.rs`；`tideloom-runtime/src/engine/translate.rs`（`wake_at` 来源）、`engine/listen.rs`、`events/local.rs`（删除 `satisfied` / `release`）、`engine/step.rs`（调用点） | `walk.rs` |
| MR-H6 | 无 | `tideloom-runtime/src/lib.rs`、`engine.rs`（`pub(crate)`）、删除 `executor/log.rs`、`events/local.rs`（删 retained 重放）、`AGENTS.md`、`README.md`、`examples/single_process.*` | 行为，除 example 的 listen/emit 顺序 |
| MR-S1 | `tideloom-postgres/Cargo.toml`、`src/lib.rs`、`src/store.rs`、`migrations/0001_instances.sql`、`tests/pg_store.rs`、`tests/store_contract.rs` | 根 `Cargo.toml`（workspace member） | `tideloom-runtime` 的 trait 签名 |
| MR-S2 | `tideloom-postgres/src/wakeup.rs`、`migrations/0002_signals.sql`、`tests/pg_wakeup.rs` | `src/lib.rs` | `tideloom-runtime` |
| MR-S3 | `tideloom-postgres/src/events.rs`、`migrations/0003_events.sql`、`tests/pg_events.rs` | `src/lib.rs` | `tideloom-runtime` |
| MR-S4 | `tideloom-postgres/src/definitions.rs`、`migrations/0004_definitions.sql`、`tests/pg_definitions.rs` | `src/lib.rs` | 其它 |
| MR-S5 | `tideloom-runtime/src/engine/run.rs`、`tests/run_loop.rs` | `engine.rs`（`EngineConfig`、`Engine::run`）、`store.rs`（`release_leases`）、`store/memory.rs`、`Cargo.toml`（`tokio` 正式依赖、`tokio-util`） | bus trait |
| MR-S6 | `tideloom-postgres/tests/two_workers.rs`、`tests/crash_recovery.rs`、`tideloom-postgres/src/bin/worker.rs`、`.github/workflows/ci.yml`（postgres service，仅 `test` job） | README | 其它 |

---

## MR-H1：父实例 release 与收尾路径

**动机**：`run_once` 在 claim 之后把每个父实例的 `version` 快照下来，子实例完成时按快照做 CAS，目的是"两个分支同一轮完成只 release 一次"。但当父实例已经 `Runnable`（被早完成的分支 release 过）并和晚完成的分支在同一轮被 claim 时，父实例先 step，`rejoin` 发现还有分支未完成，`still_waiting` 把版本推高；随后分支完成，release 带着旧快照，CAS 冲突被丢弃。此时父实例是 `Waiting(Join)`，没有 `wake_at`，`claim` 永远不会再返回它。复现：fork 两个分支，一个 `set`，一个 `wait: 5s` 后 `set`，三轮 `run_once` 后父实例卡死。

**做什么**

- 删除 `parent_versions` 快照；`release_parent` 对父实例做 `get` 后用当前版本 CAS。"只 release 一次"由 `rejoin` 保证：它每次都重新读 `children`，第二次 release 把 `Runnable` 的父实例再次置为 `Runnable` 是幂等的。`release_parent` 现有的 `!waiting && status != Runnable` 早退要改掉：本轮里父实例可能已经从 `Runnable` 被重新 step 成 `Waiting(Join)`，晚完成的分支必须能 release 这个新版本。
- `rejoin` 与 `relisten` 改为走 `finish_inline`，和 `rechild` / `release_timer` 一致。现行 `rejoin` 在 `JoinDecision::Release` 之后直接 `translate`，join 之后紧接的 activity 被 `translate` 当成"新分支"park 成 due timer，要下一轮才执行。
- 删除 `step.rs` 里"新分支首轮只 park"的特判（`continuation.is_none() && parent.is_some()` 时跳出 activity 循环），以及 `translate.rs` 里把未执行的 `Pause::Activity` park 成 due timer 的分支。compete 的取消本来就是尽力而为（ADR §6），靠重读和执行器幂等约定兜底：首轮直接执行意味着一个随后被取消的分支可能已经发出了副作用。

**测试**：`fork.rs` 增加 `a_late_branch_releases_a_parent_that_was_reparked_in_the_same_pass`（即上面的复现）、`a_branch_runs_its_first_activity_in_the_pass_that_starts_it`。

---

## MR-H2：持久化形态

**动机**：三件事会直接变成错误的列。分支起点藏在 `input` 里会成为一列满是 NUL 前缀键的 JSON；`Lease.open_attempt` 让"租约"承载 journal 状态，store 在 `apply` 里从本 commit 的 `step_results` 推导它，而引擎的恢复判定仍是 `steps()` 全量扫描；`Waiting(Activity)` 是 `claim` 和 `sweep` 都看不到的状态（`sweep` 只列 `Running`），崩溃后只有测试辅助 `lapse_lease` 把它改回 `Running` 才能恢复，真实崩溃会永远卡住。

**做什么**

- `Instance` 与 `SpawnChild` 增加 `root_task: Option<String>`；删除 `BRANCH_POSITION`、`branch_input`、`visible_input`。`begin` 在 `root_task` 为 `Some` 时调用 `start_at`，输入就是 `Instance.input`，不再剥键。
- `Instance` 增加 `in_flight: Option<InFlight>`，`InFlight` 是 `{ key: TaskKey, attempt: u32 }`。`Commit` 增加同名字段，由引擎在写 `Pending` 时设为 `Some`、在写终态 outcome 时设为 `None`。`Lease` 回到 `{ worker, until }`。store 不再从 `step_results` 推导 attempt。
- 删除 `WaitReason::Activity`。activity 进行中的实例保持 `Running` 且持有租约，`in_flight` 为 `Some`。`sweep` 的语义不变（租约过期的 `Running`），自然覆盖它。恢复判定改为：被 sweep 回收后再次 claim 的实例若 `in_flight` 为 `Some`，用 `latest_step(instance, key)` 看该 key 最新一行是否 `Pending`，再按执行器幂等性决定重做或 `Faulted`。`in_flight` 只指出查哪一行，不代替那一行。
- `InstanceStore` 增加 `latest_step(&self, instance: &InstanceId, key: &TaskKey) -> Result<Option<StepResult>, StoreError>`。`open_attempt` 与 `steps()` 全量扫描一起退出恢复路径。`steps(instance)` 留下，只给测试和运维。这是 trait 签名变更，写在本 MR 的 "Trait signature changes" 里，不另开 MR：它和 `in_flight` 是同一件事。
- 删除 `MemoryStore::lapse_lease`。测试把 `ManualClock` 推进到租约 `until` 之后再 `run_once`；`run_once` 的 sweep 用的就是这个 `now`。

**持久化格式变更**：`Instance` 增 `root_task`、`in_flight`，`Lease` 删 `open_attempt`；`Commit` 与 `SpawnChild` 增对应字段。`Continuation` 不改，`SCHEMA` 保持 1。内存实现无需迁移路径；SQL schema 以此为准。

**Trait signature changes**：`InstanceStore::latest_step`。

**测试**：`journal.rs` 的崩溃用例改为不用 `lapse_lease`；`memory_store.rs` 增加"sweep 返回进行中 activity 的过期租约"；`fork.rs` 增加"分支实例的 `input` 与父输入逐字节相等，`root_task` 是分支位置"。

---

## MR-H3：时钟、唤醒与 `known` 列表

**动机**：`EngineParts.clock` 没有被读过，所有循环由调用方传 `now`，`drive_until_idle` 的参数是 `&ManualClock`；验收 example 因此造了两个时钟。`known` 列表是进程内状态，第二个 worker 或重启后为空，满足条件的 listen 只有订阅它的那个进程能释放。跨进程这两条都不成立。

**做什么**

- `Engine::run_once(&self)` 不带参数，读一次 `self.clock.now()`，整个 pass 共用。生产循环用 `SystemClock`；测试把 `ManualClock` 放进 `EngineParts.clock`。
- `InstanceStore` 增加两个方法，写在本 MR 的 "Trait signature changes" 里：
  - `wake(&self, ids: &[InstanceId], now: Timestamp) -> Result<usize, StoreError>`：把 `Waiting(Events)` 且 `wake_at` 为空的实例置为 `Runnable`，`wake_at = now`，版本加一；其它状态忽略。这是条件翻转，不是 CAS，所以不走 `Engine::commit`。
  - `next_due(&self, now: Timestamp) -> Result<Option<Timestamp>, StoreError>`：`Runnable` 或 `Waiting(Timer)` 且无有效租约的实例中，严格晚于 `now` 的最早 `wake_at`。只给测试驱动和生产循环的轮询间隔用，不参与正确性。
- `EventSink::publish` 改为返回 `Result<Vec<InstanceId>, EventError>`，即本次投递写入 inbox 的实例。`LocalEventBus::publish` 已经算出了这个集合，只是没有返回。引擎在 emit 的 activity 结果 `commit` 之后调用 `store.wake(landed, now)`，再 `notify`。满足条件的 listen 由此变成可 claim；被 claim 后 inbox 不满足就重新 park，这是 M2 已有的行为。`wake` 不在 `publish` 的事务里：事件总线和 store 是两个 trait，S3 的 `PgEventBus` 可以在自己的事务里顺手 `wake`，那是实现细节，不是 trait 契约。
- 删除 `known`、`remember`、`known_ids`、`release_satisfied_listens`、`next_wake_at`、`notify_due`。`notify_due` 是公开函数，调用点在 `tests/engine.rs`。
- `drive_until_idle` 移到 `tideloom-runtime/src/testing.rs`，feature `test-util`。签名改为 `drive_until_idle(engine: &Engine)`：时钟就是 `engine.clock()`，调用方把它 downcast 成 `ManualClock` 不可行（`Clock` 不是 `Any`），所以 helper 要求 `EngineParts.clock` 是 `Arc<ManualClock>`，内部用 `next_due` 调 `ManualClock::set`。现有测试与 `examples/single_process.rs` 已经持有这个 `Arc`，改的是传参方式，不是再造一个时钟。验收 example 里那第二个 `ManualClock::at(clock.now())` 删掉。

**测试**：`engine.rs` 增加"引擎只用自己的时钟，调用方不传 `now`"；`listen.rs` 增加"两个 `Engine` 共享 store 与 event bus，订阅在 A 上 park，事件由 B 上的 emit 发出，A 或 B 的下一轮 `run_once` 都能释放它"。

**Trait signature changes**：`InstanceStore::wake`、`InstanceStore::next_due`、`EventSink::publish` 的返回值。

---

## MR-H4：传输层出核心

**动机**：AGENTS 规则一说核心不做 I/O，但 `ureq` 和 `http_call` 仍在 `tideloom-core`；`HttpExecutor::execute` 永远返回 fault，引擎对 `TaskKind::Http` 特判直接调 `http_call`，注册一个 `HttpExecutor` 反而会让 HTTP 失效。`EmitExecutor` 因为拿不到核心求值器自己写了一个，`$task`、比较、MR-8 的函数在 `emit.event.with` 里都不可用。

**做什么**

- 核心：`BlockDetail` 增加 `Emit(Value)`。`walk.rs` 的 `detail()` 对 `NodeKind::Emit` 在 block 创建时用核心求值器求 `emit.event.with`，作用域与 `set` 相同（`.`、`$input`、`$context`、`$workflow`、`$task`）。其它 activity 的 detail 仍是 `None`。`http.rs` 整体搬出，`ureq` 从 `tideloom-core/Cargo.toml` 移除，`tideloom_core::http_call` 删除。
- runtime：`executor/http.rs` 接收原 `http.rs` 的请求构造、发送、响应解码，`ureq` 加进 `tideloom-runtime`。`HttpExecutor::execute` 用 `TaskExecution` 的 `input` / `context` / `workflow_input` 和 `definitions.get` 取出的节点完成工作，不再需要 `Block`。`idempotent()` 保持 `false`：一次崩溃后的 GET 也是第二次请求，M2 的 journal 纪律就是"未声明幂等则 `Unknown`"。按方法返回真值会让崩溃恢复悄悄重发，留到有显式幂等声明的里程碑。删除 `step.rs` 里"未注册 HTTP executor 时直接调 `HttpExecutor::call`"的特判，以及 `activity_idempotent` 对 `TaskKind::Http` 的硬编码 `false`；HTTP 与其它 kind 一样问已注册 executor 的 `idempotent()`。
- `EmitExecutor::execute` 只做 `BlockDetail::Emit` 的 `Value` 到 `CloudEvent` 的转换。detail 不在 `TaskExecution` 上，所以执行器仍从 `definitions.get` 读节点，但读到的是已经求值过的 detail，不再自己求值。删除 `executor/emit.rs` 的私有求值器。

**Core API changes**：`BlockDetail::Emit`；删除 `http_call`。`Pause` / `Resume` / `Outcome` 不变。

**测试**：`tideloom-runtime/tests/http_call.rs` 改走已注册的 `HttpExecutor`（现行测试本来就不注册它，走的是引擎特判）。`tideloom-core/src/http.rs` 里的模板单元测试随文件迁到 runtime。`listen.rs` 增加 `emit` 使用 `$task.name` 与 `${ .items | length }`，这两条是现行私有求值器明确拒绝的。

---

## MR-H5：核心小修

- `expr.rs`：`Parser` 与 `eval` 各加深度计数，上限 64，超过返回 expression fault `expression nests too deeply`。上限对齐引擎已有的 `MAX_ACTIVITIES = 64`，不新造一个数。测试用超过上限的括号嵌套，断言返回 fault 而不是栈溢出；不要用五万层，那一层在计数器生效前就会先把解析栈打爆，测不到这条路径。
- `wake_at` 不进核心。`BlockDetail::Wait.after` 已经是从任务起点算的时长，`Continuation::task_started()` 已经记下首次观察时刻。改的是 `translate.rs`：`Pause::Timer` 的 `wake_at` 用 `continuation.task_started() + detail.after`，`Pause::Retry` 用 `continuation.task_started() + delay`（`Pause::retry_at` 已是这个算法，只是调用方传了 `now`）。`task_started` 缺失时退回 `now`，与现行 "first observation sticks" 一致。零时长仍 park 到 `task_started + 1ms`，否则 `claim` 的 `wake_at <= now` 会在同一时刻把实例领回来，`drive_until_idle` 无法 idle。测试：对同一个 `wait` 再 `Tick` 一次并重新 park，`wake_at` 不后移。
- `engine/listen.rs`：把 `LocalEventBus::satisfied` 与 `LocalEventBus::release` 收成引擎内的纯函数。`release` 的返回值保持 `Option<ListenRelease>`（事件数组加要 ack 的 id），不是 `Value`；`Resume::Release` 的 `Value` 由 `relisten` 现有的转换负责。`LocalEventBus` 不再提供这两个方法。`ListenRelease` 改为 `pub(crate)`，H6 再把它从 crate 根的 re-export 拿掉。

---

## MR-H6：API 收缩与文档

- `tideloom-runtime` 公开面收缩到：`Engine`、`EngineParts`、`EngineError`、`StepCount`、七个 trait 及其类型、`MemoryStore`、`LocalWakeup`、`LocalEventBus`、`MemoryDefinitions`、`ManualClock`、`SystemClock`、`HttpExecutor`、`EmitExecutor`。`pub mod engine` 改回私有；删除 `LogExecutor`、`UNKNOWN_DETAIL` 的公开 re-export、`step_until_pending`、`notify_due`。`UNKNOWN_DETAIL` 这句 fault detail 留下，只是不再从 crate 根导出。
- AGENTS.md：`http.rs` 改写到 runtime；测试清单改为 `tideloom-runtime/tests/http_call.rs`；删掉 "MR-9 将删除 `drive`"；工作队列指向本文档。"deliberate" 清单加两条：事件不保留给尚未订阅的实例；`InstanceId` 用 `Uuid::now_v7()` 读墙钟，事件 id 由 emit 的文档或执行器生成，两者都不经过 `Clock`。
- README：补 PR #17 的三条语义（`set` 的输出是赋值对象本身，不是合并后的输入；无匹配的 `switch` 以输入完成并走自身 `then`；`export.as` 替换 `$context`，不是合并）。
- `examples/single_process.yaml` 不改成把 `listen` 塞进 fork。现行配对是父实例的 `hear`（`listen`）消费子工作流 `tell` 的 `emit`，而 `LocalEventBus::subscribe` 会把订阅之前保留的事件重放进 inbox。H6 删掉这段重放。example 改为 `hear` 在 `tell` 之前，中间留一个 `wait`，让 emit 发生在订阅之后。输出断言保持 M2 的终态；顺序变化若改变输出，测试改断言并在 MR 里写明，不改工作流语义去迁就旧输出。

---

## MR-S1：`tideloom-postgres` 与 `PgStore`

**做什么**

- 新 crate，依赖 `sqlx = { version = "0.8", default-features = false, features = ["runtime-tokio", "postgres", "migrate", "json", "uuid"] }`、`tideloom-runtime`、`tideloom-core`。时间列用 `TIMESTAMPTZ`，读写在边界上把 `Timestamp` 的毫秒整数转成 `time::OffsetDateTime`（`time` 已是 `sqlx` 的传递依赖），不额外打开 `sqlx` 的 `time` feature，除非编译证明需要。`InstanceId` 在运行时是 UUID v7 的文本，不是 `uuid::Uuid`；SQL 列用 `UUID`，边界上解析，解析失败是 `StoreError::Backend`。
- migration `0001_instances.sql`，见下。
- `PgStore::connect(url)`、`PgStore::migrate()`，实现 `InstanceStore` 全部方法，包括 H3 新增的 `wake` / `next_due`。

**schema**

```sql
CREATE TABLE workflow_instance (
    id                 UUID        PRIMARY KEY,
    created_at         TIMESTAMPTZ NOT NULL,
    updated_at         TIMESTAMPTZ NOT NULL,
    definition_ns      TEXT        NOT NULL,
    definition_name    TEXT        NOT NULL,
    definition_version TEXT        NOT NULL,
    definition_hash    TEXT        NOT NULL,
    parent_id          UUID        NULL,
    parent_branch      INTEGER     NULL,
    root_task          TEXT        NULL,
    status             SMALLINT    NOT NULL,
    wait_reason        SMALLINT    NULL,
    row_version        BIGINT      NOT NULL DEFAULT 0,
    lease_worker       TEXT        NULL,
    lease_until        TIMESTAMPTZ NULL,
    in_flight_key      TEXT        NULL,
    in_flight_attempt  INTEGER     NULL,
    wake_at            TIMESTAMPTZ NULL,
    continuation_json  JSONB       NULL,
    input_json         JSONB       NOT NULL,
    output_json        JSONB       NULL,
    fault_json         JSONB       NULL
);

COMMENT ON COLUMN workflow_instance.created_at         IS '创建时间，由引擎传入，不用数据库时钟';
COMMENT ON COLUMN workflow_instance.updated_at         IS '最后一次 apply 的时间，由引擎传入';
COMMENT ON COLUMN workflow_instance.definition_ns      IS '定义 namespace';
COMMENT ON COLUMN workflow_instance.definition_name    IS '定义 name';
COMMENT ON COLUMN workflow_instance.definition_version IS '定义 version';
COMMENT ON COLUMN workflow_instance.definition_hash    IS '定义内容 SHA-256 hex，创建时固定';
COMMENT ON COLUMN workflow_instance.parent_id          IS '父实例 id，fork 分支与子工作流有值';
COMMENT ON COLUMN workflow_instance.parent_branch      IS 'fork 分支下标，子工作流为 NULL';
COMMENT ON COLUMN workflow_instance.root_task          IS '以 start_at 启动时的任务 JSON Pointer，根启动为 NULL';
COMMENT ON COLUMN workflow_instance.status             IS '状态：0 runnable, 1 running, 2 waiting, 3 completed, 4 faulted, 5 cancelled';
COMMENT ON COLUMN workflow_instance.wait_reason        IS 'status=2 时的原因：0 timer, 1 join, 2 child, 3 events';
COMMENT ON COLUMN workflow_instance.row_version        IS 'CAS 版本号，每次 apply 加一';
COMMENT ON COLUMN workflow_instance.lease_worker       IS '持有租约的 worker';
COMMENT ON COLUMN workflow_instance.lease_until        IS '租约到期时间';
COMMENT ON COLUMN workflow_instance.in_flight_key      IS '正在执行的 activity 的 TaskKey 文本，position 与 execution_key';
COMMENT ON COLUMN workflow_instance.in_flight_attempt  IS '正在执行的 activity 的 attempt';
COMMENT ON COLUMN workflow_instance.wake_at            IS '最早可再次 claim 的时间';
COMMENT ON COLUMN workflow_instance.continuation_json  IS '序列化的 Continuation，终态为 NULL';
COMMENT ON COLUMN workflow_instance.input_json         IS '工作流输入';
COMMENT ON COLUMN workflow_instance.output_json        IS '完成时的输出';
COMMENT ON COLUMN workflow_instance.fault_json         IS '失败时的 RFC 7807 错误';

-- claim: status IN (0, 2 with wait_reason 0) AND wake_at <= $now AND lease free
CREATE INDEX IF NOT EXISTS idx_workflow_instance_wake_at_claimable
    ON workflow_instance (wake_at)
    WHERE status IN (0, 2);

-- sweep: lease lapsed
CREATE INDEX IF NOT EXISTS idx_workflow_instance_lease_until
    ON workflow_instance (lease_until)
    WHERE lease_until IS NOT NULL;

-- children
CREATE INDEX IF NOT EXISTS idx_workflow_instance_parent_id_parent_branch
    ON workflow_instance (parent_id, parent_branch)
    WHERE parent_id IS NOT NULL;

CREATE TABLE task_execution (
    id            BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    created_at    TIMESTAMPTZ NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL,
    instance_id   UUID        NOT NULL,
    position      TEXT        NOT NULL,
    execution_key TEXT        NOT NULL,
    attempt       INTEGER     NOT NULL,
    kind          SMALLINT    NOT NULL,
    outcome       SMALLINT    NOT NULL,
    input_json    JSONB       NOT NULL,
    result_json   JSONB       NULL,
    started_at    TIMESTAMPTZ NOT NULL,
    finished_at   TIMESTAMPTZ NOT NULL
);

COMMENT ON COLUMN task_execution.created_at    IS '创建时间';
COMMENT ON COLUMN task_execution.updated_at    IS '更新时间';
COMMENT ON COLUMN task_execution.instance_id   IS '所属实例';
COMMENT ON COLUMN task_execution.position      IS '任务 JSON Pointer';
COMMENT ON COLUMN task_execution.execution_key IS '祖先计数，如 loop:0/attempt:1';
COMMENT ON COLUMN task_execution.attempt       IS '同一 key 的第几次执行，崩溃重做时加一';
COMMENT ON COLUMN task_execution.kind          IS '执行器类型：0 http, 1 grpc, 2 openapi, 3 asyncapi, 4 a2a, 5 mcp, 6 emit, 7 shell, 8 script, 9 container';
COMMENT ON COLUMN task_execution.outcome       IS '结果：0 pending, 1 output, 2 fault, 3 unknown';
COMMENT ON COLUMN task_execution.input_json    IS '任务输入';
COMMENT ON COLUMN task_execution.result_json   IS 'outcome=1 时的输出，outcome=2 时的错误';
COMMENT ON COLUMN task_execution.started_at    IS '执行器被调用的时间';
COMMENT ON COLUMN task_execution.finished_at   IS '结果写入的时间，pending 时等于 started_at';

CREATE UNIQUE INDEX IF NOT EXISTS uk_task_execution_instance_id_position_execution_key_attempt
    ON task_execution (instance_id, position, execution_key, attempt);
```

**关键语句**

```sql
-- claim
WITH due AS (
    SELECT i.id
    FROM   workflow_instance AS i
    WHERE  (i.status = 0 OR (i.status = 2 AND i.wait_reason = 0))
      AND  (i.wake_at IS NULL OR i.wake_at <= $1)
      AND  (i.lease_until IS NULL OR i.lease_until <= $1)
    ORDER  BY i.wake_at NULLS FIRST, i.id
    LIMIT  $2
    FOR UPDATE SKIP LOCKED
)
UPDATE workflow_instance AS i
SET    status = 1, lease_worker = $3, lease_until = $4, updated_at = $1
FROM   due
WHERE  i.id = due.id
RETURNING i.id, i.definition_ns, /* ... every column ... */ i.fault_json;

-- apply, inside one transaction
UPDATE workflow_instance AS i
SET    status = $3, wait_reason = $4, row_version = i.row_version + 1,
       continuation_json = $5, wake_at = $6, output_json = $7, fault_json = $8,
       in_flight_key = $9, in_flight_attempt = $10,
       lease_worker = CASE WHEN $11 THEN NULL ELSE i.lease_worker END,
       lease_until  = CASE WHEN $11 THEN NULL ELSE i.lease_until  END,
       updated_at = $12
WHERE  i.id = $1 AND i.row_version = $2;
-- 0 rows affected: ROLLBACK, return VersionConflict

INSERT INTO workflow_instance (id, /* ... */) VALUES /* spawn_children */;

UPDATE workflow_instance AS i
SET    status = 5, row_version = i.row_version + 1, updated_at = $2
WHERE  i.id = ANY($1) AND i.status NOT IN (3, 4, 5);

INSERT INTO task_execution (/* ... */) VALUES (/* ... */)
ON CONFLICT (instance_id, position, execution_key, attempt)
DO UPDATE SET outcome = EXCLUDED.outcome, result_json = EXCLUDED.result_json,
              finished_at = EXCLUDED.finished_at, updated_at = EXCLUDED.updated_at;
```

**约束**

- `row_version` 而不是 `version`，避开 `VERSION` 在部分方言里的保留。Rust 字段仍叫 `version`。
- `apply` 的所有语句在一个事务里，任何一句失败整体回滚；`VersionConflict` 由第一条 `UPDATE` 的 rowcount 判定。
- `claim` 与 `apply` 都不读 `now()`，也不写 `DEFAULT CURRENT_TIMESTAMP`。时间参数与 `created_at` / `updated_at` / `started_at` / `finished_at` 都由引擎传入。`Pending` 行的 `finished_at` 等于 `started_at`。
- `steps(instance)` 只给测试和运维；引擎用 H2 的 `latest_step(instance, key)`。状态码与 `TaskKind` / `StepOutcome` 的对应关系写在 `PgStore` 旁边的常量里，不散落在 SQL 字符串中。

**测试**：`tests/pg_store.rs` 复用 `memory_store.rs` 的断言。共享步骤放在 `tideloom-postgres/tests/store_contract.rs`，只被 pg 测试调用。`MemoryStore` 不反向依赖 postgres crate，它的用例留在 runtime，两边各跑一份等价断言，不为此新开 crate。外加：两个连接并发 `claim` 同一批实例不重叠；`apply` 中途失败（spawn 一个重复 id）不留下部分写入。

---

## MR-S2：`PgWakeup`

```sql
CREATE TABLE signal_log (
    id          BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    instance_id UUID        NOT NULL,
    hint        SMALLINT    NOT NULL,
    hint_ref    TEXT        NULL
);

COMMENT ON COLUMN signal_log.created_at  IS '创建时间';
COMMENT ON COLUMN signal_log.updated_at  IS '更新时间';
COMMENT ON COLUMN signal_log.instance_id IS '可能变得可运行的实例';
COMMENT ON COLUMN signal_log.hint        IS '原因：0 timer, 1 child done, 2 event, 3 other';
COMMENT ON COLUMN signal_log.hint_ref    IS '子实例 id 或事件 id';

CREATE INDEX IF NOT EXISTS idx_signal_log_instance_id ON signal_log (instance_id);

CREATE TABLE signal_cursor (
    id               BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    created_at       TIMESTAMPTZ NOT NULL,
    updated_at       TIMESTAMPTZ NOT NULL,
    consumer_group   TEXT        NOT NULL,
    committed_offset BIGINT      NOT NULL DEFAULT 0
);

COMMENT ON COLUMN signal_cursor.created_at       IS '创建时间';
COMMENT ON COLUMN signal_cursor.updated_at       IS '更新时间';
COMMENT ON COLUMN signal_cursor.consumer_group   IS '消费组，一组 worker 共用一个位点';
COMMENT ON COLUMN signal_cursor.committed_offset IS '已提交的 signal_log.id';

CREATE UNIQUE INDEX IF NOT EXISTS uk_signal_cursor_consumer_group ON signal_cursor (consumer_group);
```

- `Offset` 就是 `signal_log.id`。`notify` 插入一行并 `pg_notify('tideloom_signal', '')`；`watch(from)` 先 `SELECT ... WHERE id > $from ORDER BY id`，再 `LISTEN` 后按通知拉增量。通知丢失时按 `poll_interval` 兜底查表，不靠通知本身正确。
- `Wakeup::commit_offset` 现行签名没有 consumer group。M3 不改 trait：`PgWakeup::new(pool, consumer_group)` 把组名存在实现里，`commit_offset` / `committed` 读写这一组的行。多组并存是表的能力，不是 trait 的参数。
- 保留策略不放进 S2。S6 的 worker 每小时删除 7 天前的 `signal_log` 行；`watch` 的 `from` 早于最小 id 时返回 `OffsetExpired`。测试不依赖清理任务。

---

## MR-S3：`PgEventBus`

```sql
CREATE TABLE event_subscription (
    id          BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    instance_id UUID        NOT NULL,
    spec_json   JSONB       NOT NULL,
    is_active   SMALLINT    NOT NULL DEFAULT 1
);

COMMENT ON COLUMN event_subscription.created_at  IS '创建时间';
COMMENT ON COLUMN event_subscription.updated_at  IS '更新时间';
COMMENT ON COLUMN event_subscription.instance_id IS '订阅的实例';
COMMENT ON COLUMN event_subscription.spec_json   IS 'ListenSpec';
COMMENT ON COLUMN event_subscription.is_active   IS '是否生效：1 是，0 否';

CREATE UNIQUE INDEX IF NOT EXISTS uk_event_subscription_instance_id ON event_subscription (instance_id);

CREATE TABLE event_subscription_attr (
    id              BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL,
    subscription_id BIGINT      NOT NULL,
    filter_index    INTEGER     NOT NULL,
    attr_name       TEXT        NOT NULL,
    attr_value      TEXT        NOT NULL
);

COMMENT ON COLUMN event_subscription_attr.created_at      IS '创建时间';
COMMENT ON COLUMN event_subscription_attr.updated_at      IS '更新时间';
COMMENT ON COLUMN event_subscription_attr.subscription_id IS '所属订阅';
COMMENT ON COLUMN event_subscription_attr.filter_index    IS 'ListenSpec.filters 的下标';
COMMENT ON COLUMN event_subscription_attr.attr_name       IS 'CloudEvent 属性名';
COMMENT ON COLUMN event_subscription_attr.attr_value      IS '要求相等的值';

CREATE INDEX IF NOT EXISTS idx_event_subscription_attr_attr_name_attr_value
    ON event_subscription_attr (attr_name, attr_value);

CREATE TABLE event_inbox (
    id          BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    instance_id UUID        NOT NULL,
    event_id    TEXT        NOT NULL,
    event_json  JSONB       NOT NULL,
    is_acked    SMALLINT    NOT NULL DEFAULT 0
);

COMMENT ON COLUMN event_inbox.created_at  IS '创建时间';
COMMENT ON COLUMN event_inbox.updated_at  IS '更新时间';
COMMENT ON COLUMN event_inbox.instance_id IS '收件实例';
COMMENT ON COLUMN event_inbox.event_id    IS 'CloudEvent id';
COMMENT ON COLUMN event_inbox.event_json  IS '完整事件';
COMMENT ON COLUMN event_inbox.is_acked    IS '是否已被消费：1 是，0 否';

CREATE UNIQUE INDEX IF NOT EXISTS uk_event_inbox_instance_id_event_id ON event_inbox (instance_id, event_id);
```

- `publish`：按事件的属性在 `event_subscription_attr` 上查候选订阅，应用层做 filter 级别的精确比较（一个 filter 的全部属性都相等才算命中，与 `LocalEventBus` 的 `matches` 一致），命中的实例插 `event_inbox`。`ON CONFLICT (instance_id, event_id) DO NOTHING` 保证重复投递幂等。返回值是本次新写入 inbox 的实例 id，与 H3 的 `EventSink::publish` 一致。
- `wake` 由引擎在 emit 的 `commit` 之后调用（H3）。`PgEventBus` 可以在 `publish` 自己的事务里对命中实例再执行一次与 `PgStore::wake` 相同的 `UPDATE`，少一次往返；两次 `wake` 必须幂等，只翻转 `Waiting(Events)` 且 `wake_at` 为空的行。trait 不要求 `publish` 调用 store。
- `subscribe` 幂等：spec 相同则不动；spec 不同则替换属性行并保留 inbox。不扫描历史事件。`unsubscribe` 置 `is_active = 0` 并删 inbox；`ack` 置 `is_acked = 1`。`is_active` / `is_acked` 用 `SMALLINT`，与本仓库其它布尔列一致，不改成 `BOOLEAN`。
- 不保留订阅之前的事件（全局约束 5）。H6 已经删掉 `LocalEventBus` 的 retained 重放，两个实现在这一点上一致。

---

## MR-S4：`PgDefinitions`

```sql
CREATE TABLE definition (
    id            BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    created_at    TIMESTAMPTZ NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL,
    namespace     TEXT        NOT NULL,
    name          TEXT        NOT NULL,
    version       TEXT        NOT NULL,
    content_hash  TEXT        NOT NULL,
    document_json JSONB       NOT NULL
);

COMMENT ON COLUMN definition.created_at    IS '创建时间';
COMMENT ON COLUMN definition.updated_at    IS '更新时间';
COMMENT ON COLUMN definition.namespace     IS 'document.namespace';
COMMENT ON COLUMN definition.name          IS 'document.name';
COMMENT ON COLUMN definition.version       IS 'document.version';
COMMENT ON COLUMN definition.content_hash  IS '规范 JSON 的 SHA-256 hex';
COMMENT ON COLUMN definition.document_json IS '规范化后的文档';

CREATE UNIQUE INDEX IF NOT EXISTS uk_definition_namespace_name_version_content_hash
    ON definition (namespace, name, version, content_hash);
```

- `get(key)` 按 namespace、name、version、content_hash 四列精确查。`latest(ns, name, version)` 与 `MemoryDefinitions::latest` 一样按三列匹配；多行时取 `created_at` 最新的一行，不发明"按版本号排序"。定义不可修改：同一 hash 重复插入是 no-op，不同 hash 是新行。`DefinitionSource` 没有 `publish`；插入是 `PgDefinitions` 自己的方法，与 `MemoryDefinitions::publish` 同形。

---

## MR-S5：`Engine::run`

- `EngineConfig { lease: Duration, claim_limit: usize, sweep_every: Duration, poll_every: Duration }`。consumer group 属于 `PgWakeup` 的构造参数，不进 `EngineConfig`：内存总线没有组。
- `Engine::run(&self, shutdown: CancellationToken)`。`CancellationToken` 来自新增的 `tokio-util` 依赖，加在 `tideloom-runtime`，不手写一套 shutdown 标志。一个任务 `watch` 信号流，每个信号触发一次 `run_once`；一个定时任务每 `sweep_every` 调 `run_once`；一个任务按 `next_due` 与 `poll_every` 的较小值调度下一次 `run_once`。三者共用一个 `tokio::sync::Notify`，`run_once` 不重入。`tokio` 的 `rt` / `time` / `macros` 从 dev-dependencies 提升到 dependencies，`sync` 已在。
- 退出：停止 claim，等正在执行的 step 结束并 `apply` 它们的 commit，然后调用 `InstanceStore::release_leases(&self, worker: &WorkerId, now: Timestamp)`，把本 worker 仍持有的租约 `until` 改成 `now`，让下一轮 sweep 回收。这是 trait 新增，写在本 MR 的 "Trait signature changes"，不另开 MR。不在退出路径上把实例直接改回 `Runnable`：那会跳过 sweep 的恢复判定，把一个 `in_flight` 的 activity 当成普通重入。
- `run_once` 的 `now` 来自 `self.clock`。生产用 `SystemClock`，测试用 `ManualClock`。`run` 等待 `Notify` 或 shutdown，不读墙钟，也不 `sleep` 定长。

**测试**：`tests/run_loop.rs` 用 `MemoryStore` + `LocalWakeup` + `ManualClock`：信号触发 step；shutdown 后本 worker 的租约 `until` 不晚于 shutdown 时刻，下一轮 `run_once` 能 sweep 到它；`next_due` 推进时钟而不是睡到那个时刻。

**Trait signature changes**：`InstanceStore::release_leases`。

---

## MR-S6：双进程验收

- `tideloom-postgres/src/bin/worker.rs`：读 `DATABASE_URL`、`WORKER_ID`，组装 `PgStore` + `PgWakeup` + `PgEventBus` + `PgDefinitions` + `SystemClock` + `HttpExecutor` + `EmitExecutor`，调用 `Engine::run`，收到 SIGTERM 优雅退出。幂等性是 executor 的属性，不由环境变量翻转。崩溃用例在测试里注册自己的 executor，不经过这个二进制。
- `tests/two_workers.rs` 用 `std::process::Command` 拉起两个 `worker` 进程。`tests/crash_recovery.rs` 不拉两个完整二进制再赌时序：它在进程内跑两个 `Engine` 共享一个 `PgStore`，worker A 提交 `Pending` 之后、调用 executor 之前停住，然后让 A 的租约过期，由 worker B 的 `run_once` 回收。`kill -9` 留作可选冒烟，不作为退出条件，因为"Pending 已提交、executor 未返回"在进程外不可观测。等待用有上限的轮询（每 50 ms 查一次状态，总上限 30 s），不用定长 `sleep` 当作同步。
- 跨进程证据读 `workflow_instance.lease_worker`，不读 journal。见退出条件。
- CI：只给 `test` job 加 `postgres:16` service 与 `TIDELOOM_TEST_DATABASE_URL`。`check` job 只跑 clippy，不加数据库。矩阵含 `ubuntu-22.04`、`macos-14`、`windows-2022`；service container 只在 ubuntu 上可用，其它 OS 的 job 不设置该变量，测试按退出条件跳过。`sqlx` 用 offline 模式，`.sqlx/` 提交进仓库，这样 clippy 与无数据库的测试不必连 Postgres 才能编译。
- `signal_log` 与已 ack 的 `event_inbox` 的清理放在 `worker` 的一个每小时任务里，保留 7 天。清理失败只记日志，不影响 `Engine::run`。

---

## 完成后的完成度

M3 结束时，按 M2 后那张表的口径：运行时引擎 60% → 80%，SQL 存储 0% → 80%，总线 10% → 40%（有可重放的 DB 实现，无 Kafka），运维 5% → 15%（有 worker 二进制与优雅退出，无取消 / 查询接口）。整体约 **58%**。剩下的大块是任务类型补全与 `listen` 第二阶段（M4）、运维面与加固（M5）。

## 开 MR 时的检查清单

- [ ] 标题 `M3 MR-<编号>: <短名>`，分支 `m3/<编号>-<短名>`
- [ ] "Core API changes"、"Persistence format changes"、"Trait signature changes" 三节都在，没有就写 "none"
- [ ] 只碰"文件归属"表分配的文件
- [ ] SQL 里没有 `now()`；每个 `apply` 一个事务；DDL 只追加
- [ ] 新 migration 的每个字段有 COMMENT，索引名 `idx_` / `uk_` 前缀含表名
- [ ] `cargo test --workspace` 在没有 `TIDELOOM_TEST_DATABASE_URL` 时仍然全绿（PG 测试跳过）
- [ ] `cargo clippy --workspace --all-targets`、`cargo +nightly fmt --check`、`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` 干净
