# [ADR-0001] Serverless Workflow 执行模型：不可变 AST、运行时 Block 与可重算等待

## Status

Proposed

日期：2026-09-28

## Context

我们要实现一个 Serverless Workflow DSL v1.0 的运行时。最初的设想是把 YAML 编译成若干 Block，构建依赖树，把 Block 分发给 worker 执行并收集耗时。在对比 Airflow、Prefect、Dagster、DBOS、Temporal、Restate 以及 Lemline 之后，需要回答三个问题：

1. 依赖树或 DAG 是否适合作为这个 DSL 的执行模型？
2. 执行状态的真相源放在哪里：数据库、消息总线，还是 worker 内存？
3. 多路等待（fork join、`listen` 多事件、子工作流完成）如何在无状态 worker 下保证不丢信号？

关键约束来自 DSL 语义本身：`do` 是严格顺序，`switch.then` 可以跳转到同级任意任务并形成循环，`for` 和 `try.retry` 是循环，`listen`、`wait`、`run` 可能挂起数天，`fork` 是唯一显式并行原语并带 `compete` 语义，数据依赖隐藏在 jq 表达式和 `$context` 中。控制流图是运行时才确定的一般有向图，不是 DAG。

## Decision

### 1. 定义层：编译为不可变 AST，按内容 hash 版本化

YAML 解析为不可变的节点树，节点位置用 JSON pointer 表示，例如 `/do/0/validateOrder`。定义按 `(namespace, name, version)` 加内容 hash 存储，一旦写入不可修改，同名发新版即新记录。实例记录启动时的 hash，生命周期内始终加载同一份 AST。

不从 AST 抽取依赖树参与调度。图只作为派生的只读视图存在：从 AST 导出静态控制流图用于校验和展示，从执行记录导出实际轨迹图用于耗时统计和关键路径分析。

### 1.1 节点属性：三个独立谓词，不做"控制 / 执行"二分

规范本身没有对 12 种 task 做分类，只有"是否嵌套子任务"这一语法区分。实现中常见的"控制单元 vs 执行单元"二分会把 `wait`、`set`、`emit`、`run workflow` 这些边界案例分错。编译期在每个 AST 节点上计算三个互相独立的只读属性，运行时只查表：

| 属性 | 含义 | 驱动什么 | 为真的任务 |
|---|---|---|---|
| `composite` | 有子节点 | 解释器导航，`enterFromChild` 钩子，position 方案 | `do`、`for`、`fork`、`try`、`switch`、`call function` |
| `blocking` | 可能暂停 | Block 边界，是否需要 `wake_at` 或等待条件 | `wait`、`listen`、`run workflow`、`fork`（join）、`try`（retry backoff） |
| `effectful` | 有外部副作用 | `task_execution` 行、幂等键、副作用前 checkpoint、runner 的 `kind` 路由 | `call http/grpc/openapi/asyncapi`、`run shell/script/container`、`run workflow`、`emit` |

三个属性不重合：`wait` 是 blocking 但不 effectful；`emit` 是 effectful 但不 blocking；`set`、`raise`、`switch` 三者都不是；`run workflow` 既 blocking 又 effectful；`fork` 既 composite 又 blocking。

两条派生规则：

- **Block 边界 = blocking 或 effectful**。
- **`task_execution` 行与 runner 路由 = 仅 effectful**。

`call function` 归为 composite 而非 effectful：调用 `use.functions` 中的用户定义函数是内联一段子树，解释器导航进入，不是外部调用。Lemline 曾把它当 activity 执行，后改为控制流导航，这里直接采用后者。

处理器实现只有一个模板方法基类：composite 节点覆盖子节点导航钩子，叶子节点覆盖 `execute`。不为这三个属性建立类层次。

### 2. 执行单元：Block 是运行时单位，不是编译时切分

Block 定义为一次运行中，从一个恢复点开始、到下一个不可避免的暂停点为止的连续执行段。编译期按 1.1 节的规则标出树上所有 Block 边界，即 `blocking` 或 `effectful` 为真的节点。运行时 Block 由 `(instance_id, position, execution_key)` 唯一确定，`execution_key` 是各层祖先的循环与重试计数的拼接。

一个 Block 的生命周期严格等于一次 `Claim` 到一次 `Apply`。worker 领到实例后从续体位置开始解释，控制流节点在进程内连续执行，到达暂停点时构造一个 `Commit` 交回存储并释放实例。

只实现一个解释器。测试与生产走同一条代码路径，差异只在注入的存储和总线实现。

### 3. 状态层：显式续体 + 每个 Block 一次 checkpoint

执行状态是一个显式的、可序列化的续体：当前位置、各层祖先节点的状态、`$context`。不采用 replay 式恢复，因为 replay 是宿主语言调用栈不可序列化时才需要付的代价，DSL 解释器的栈天生是数据。

存储接口按引擎需要的保证定义，而不是按仓库模式定义。引擎只依赖四个原语：租约领取、带版本 CAS 的原子提交、按主键读、按条件扫描。原子提交建模为值对象 `Commit`，各后端自行决定如何实现原子性，事务句柄不出现在接口中。trait 定义见第 9 节。

两张核心表：`workflow_instance` 保存续体、状态、版本、租约、`wake_at`；`task_execution` 保存每次 activity 的位置、`execution_key`、attempt、输入输出、起止时间。后者是耗时统计的一等来源，不依赖额外的分析管道。

checkpoint 粒度是 Block 而非 task：写次数等于 activity 数，不等于任务数。

### 4. 消息总线：可重放日志，只作唤醒与信号，不承载状态

总线的正确性定位是通知，不是真相。消息体只有 `InstanceID` 和可选的 hint。丢消息、重复、乱序都只影响延迟，不影响正确性：收到消息后仍走 `Claim`，领不到即丢弃；sweeper 定期扫描 `wake_at <= now` 兜底。trait 定义见第 9 节。

因为等待状态保存在 worker 内存（见第 5 节），总线抽象的对象是**带消费位点的可重放日志**，不是队列。Kafka、NATS JetStream、Redis Streams 符合，RabbitMQ 经典队列不符合。第一版用数据库中的 `event_inbox` 表加位点列实现同一接口，第一版不引入 Kafka。

唤醒是 hint，不需要 outbox。`Apply` 成功后尽力 `Notify` 即可。

### 5. 多路等待：状态留在 worker 内存，丢失即重算

等待条件不物化到存储。worker 持有实例时在内存中收集信号；worker 崩溃或主动释放后，接手的 worker 从持久来源重算等待状态。

重算成立的唯一不变量：**等待所依赖的每一个事实，必须能从某个 worker 没有"消费即丢弃"的持久来源重新读出。**

| 等待类型 | 重算来源 | 代价 |
|---|---|---|
| Timer / Retry | 续体中的 `startedAt` 与 duration、attempt | 无 |
| 子实例 / fork 分支完成 | 子实例行的终态与输出 | 无，前提是子实例是独立行 |
| 外部事件 | 事件日志，且消费位点在实例 `Apply` 之后才推进 | 总线必须可重放 |
| 进行中的 activity | 无，副作用在外部 | 不可重算，按任务声明幂等重做或标记未知 |

暂停原因是封闭枚举：`Timer`、`Retry`、`Child`、`Join`、`Events`、`Activity`。Block 之间的差异全部收敛到暂停原因及其恢复条件，Block 本身是同构的。

内存等待的两条纪律：

1. 接手实例时先订阅信号流，再读取持久事实，两边合并去重，消除检查与订阅之间的窗口。
2. 等待超过阈值（初值 30 秒）必须释放实例，写 `wake_at` 或标记 waiting，由 sweeper 或事件到达触发重新 `Claim`。释放后的重算与崩溃后的重算是同一段代码。

信号路由：小规模所有 worker 订阅同一条流，按 `instance_id` 过滤；规模上升后以 `instance_id` 为分区键，实例信号与持有它的 worker 落在同一分区。两种方式都不需要注册表。

### 6. 并行与错误边界

`fork` 的每个分支是独立实例行，带 `parent_id` 与 `branch_position`。join 由分支完成的 commit 触发父实例重算。cooperative 模式任一分支失败即失败父实例并标记兄弟分支 cancelled；compete 模式首个成功即完成，全部失败才失败。fork 是错误边界，分支内未捕获的错误不穿透到 fork 之外的 `try`。

### 7. Worker 能力差异是独立的轴

HTTP、gRPC、shell、container 等执行能力的差异不体现在 Block 上，而是 `task_execution.kind`。解释器 worker 遇到 `effectful` 节点时插入 pending 行并暂停，各类型 runner 按 `kind` 领取执行，完成后的 commit 满足 `Activity` 条件。解释器 worker 与执行 runner 可独立扩缩容；早期可在同一进程内完成，模型不变。

### 8. `listen` 的特殊处理

`listen.foreach` 允许在持续接收事件的同时对每个事件执行子块，等待不是一次性的。入站事件先写入 per-instance 的 inbox，恢复条件为"inbox 非空"，Block 消费 inbox 后决定继续等待或结束。

事件匹配走索引：编译期从 `listen` 定义提取可静态确定的字段（`type`、`source` 等）作为 subscription 索引列，只对命中的候选再执行 jq 精确匹配与 correlation。

### 9. 外部抽象：三个 trait 与组合方式

实现语言为 Rust。引擎与外部世界的边界恰好是三个 trait：`InstanceStore`、`Wakeup`、`ActivityExecutor`，另加 `Clock` 用于测试。以下定义为规范性定义，后续实现以此为准。

```rust
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

/// Atomic unit of work produced by one Block. Backends decide how to make it atomic.
pub struct Commit {
    pub instance_id: InstanceId,
    pub expected_version: u64,
    pub new_stack: Vec<u8>,
    pub new_status: Status,
    pub wake_at: Option<Timestamp>,
    pub step_results: Vec<StepResult>,   // task_execution rows to upsert
    pub spawn_children: Vec<Instance>,   // fork branches, sub-workflows
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("version conflict")]
    VersionConflict,
    #[error("not found")]
    NotFound,
    #[error(transparent)]
    Backend(#[from] anyhow::Error),
}

#[async_trait]
pub trait InstanceStore: Send + Sync {
    async fn create(&self, inst: Instance) -> Result<(), StoreError>;
    async fn get(&self, id: InstanceId) -> Result<Instance, StoreError>;
    /// Lease up to `limit` runnable instances whose `wake_at <= now`.
    async fn claim(&self, worker: WorkerId, now: Timestamp, limit: usize)
        -> Result<Vec<Instance>, StoreError>;
    /// CAS on `expected_version`; all fields land atomically or none do.
    async fn apply(&self, commit: Commit) -> Result<(), StoreError>;
    /// Instances whose lease expired before `before`.
    async fn sweep(&self, before: Timestamp) -> Result<Vec<InstanceId>, StoreError>;
}

/// A signal is a hint, never a source of truth.
pub struct Signal {
    pub instance_id: InstanceId,
    pub hint: Option<Hint>,
    pub offset: Offset,
}

pub type SignalStream = BoxStream<'static, Result<Signal, BusError>>;

/// Replayable log with consumer offsets, not a queue.
#[async_trait]
pub trait Wakeup: Send + Sync {
    async fn notify(&self, signal: Signal) -> Result<(), BusError>;
    async fn watch(&self, from: Offset) -> Result<SignalStream, BusError>;
    /// Must be called only after the corresponding `InstanceStore::apply` succeeded.
    async fn commit_offset(&self, offset: Offset) -> Result<(), BusError>;
}

#[async_trait]
pub trait ActivityExecutor: Send + Sync {
    fn kind(&self) -> TaskKind;
    async fn execute(&self, task: TaskExecution) -> Result<Value, ActivityError>;
}

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

pub struct Engine {
    store: Arc<dyn InstanceStore>,
    wakeup: Arc<dyn Wakeup>,
    executors: HashMap<TaskKind, Arc<dyn ActivityExecutor>>,
    clock: Arc<dyn Clock>,
}
```

组合方式的三条约定：

1. **用 `dyn` 而不是泛型参数。** 三个 trait 全是 I/O 边界，每个 Block 调用一次，动态分发开销可忽略；后端由配置在运行时选择，泛型会让 `Engine<S, W, X>` 的类型参数扩散到所有调用方与测试。trait 中的 `async fn` 目前不能直接做 trait object，统一使用 `async_trait`。
2. **解释器核心不依赖任何一个 trait。** 核心是纯函数：AST、续体、输入进，`Outcome` 出，`Outcome` 携带 `Commit` 与暂停原因。三个 trait 只由外层 `Engine` 持有，核心可在无 mock 的情况下做属性测试。
3. **`apply` 与 `commit_offset` 的顺序是不可拆分的。** 第 5 节的可重算性完全依赖"先 `store.apply`，后 `wakeup.commit_offset`"。在 `Engine` 中把两者封装为单一函数，禁止在其他位置分别调用。

三个节点属性 `composite`、`blocking`、`effectful` 不是 trait，是编译期算好挂在 `Node` 上的字段，运行时只查表。

## Consequences

### 正面

- 定义是数据，版本问题在数据层解决，不需要 Temporal 式的 patch 机制或 Restate 式的不可变部署。
- 单一解释器，测试与生产语义不会漂移。
- 实例可查询、可取消、可重跑，排障是一条 SQL。
- 总线只是 hint，去掉 outbox 与消费者侧去重表，总线故障退化为延迟而非错误。
- 等待状态不落库，存储模型只有 `workflow_instance`、`task_execution`、子实例关系和 inbox。
- 演进路径渐进：内存 store 与进程内唤醒 → SQL store 与轮询 → 接入可重放日志 → 按状态冷热分区或按 `instance_id` 分片。每一级不改解释器与 `Commit` 定义。

### 负面

- 每个 Block 一次写，吞吐受存储约束。缓解手段是 Block 级 checkpoint 与 group commit，不是把状态搬到总线。
- 内存等待占用 worker 租约与内存，长等待必须释放，需要阈值与 sweeper 协作。
- 总线必须是可重放日志，排除了经典队列型中间件。
- 进行中的 activity 崩溃后无法重算，副作用正确性依赖任务声明的幂等键或人工介入策略。
- `$context` 体积无上限保护时会拖慢每次 checkpoint，需要上限策略。

### 中性

- 静态控制流图与运行轨迹图作为派生视图仍值得实现，但不参与调度。
- 事件匹配的索引设计依赖对 `listen` 定义的静态分析能力，影响可支持的过滤表达式范围。

## Alternatives Considered

### A. 编译为依赖树 / 静态 DAG 调度

拒绝。`switch.then`、`for`、`try.retry` 使控制流成为运行时才确定的一般有向图；数据依赖藏在 jq 表达式中，静态推断不可判定；`do` 语义是顺序，推断并行违反规范意图；长挂起与调度器的短生命周期节点假设冲突。DAG 仍适合结构静态的批处理数据管道，不适合本 DSL。

### B. Replay 式持久化执行（DBOS / Temporal / Restate 模式）

拒绝。这些系统 replay 的原因是宿主语言调用栈不可序列化，需要重放重建；同时要求工作流代码确定性，并把版本兼容问题推给 patch API 或不可变部署。DSL 解释器的状态天然可序列化，直接快照即可，无需承担确定性约束。保留其"副作用前写 checkpoint、副作用后同事务写结果"的核心纪律。

### C. 消息承载续体，DB 只在暂停点介入（Lemline 模式）

拒绝。emit 先于 ack 使交付语义为 at-least-once，确定性消息 ID 只在 broker 支持按 ID 去重时有效，Kafka 不支持，热路径无去重表，崩溃窗口内同一续体会被并行执行两次；无实例表导致列表、定位、取消都需要额外管道；续体随每条消息流转，`$context` 增大直接撞消息体上限；为适配不同状态载体维护两套 orchestrator。其换取的热路径零写与高吞吐，不是当前场景的主要痛点。

### D. 等待条件物化到存储，用原子翻转做 fan-in

未采用，作为后备。做法是 `wait_condition` 表加"满足一个条件并判定是否全部满足"的原子提交。正确性等价于内存重算方案，但引入一张表和一套翻转逻辑。仅当内存中等待的实例数量超出 worker 容量时再启用，接口不变。

### E. 泛型 Repository 抽象存储

拒绝。会诱使把 `task_execution` 与 `workflow_instance` 拆成独立写入，原子性丢失。存储接口按引擎需要的保证定义。

### F. 节点按"控制单元 / 执行单元"二分

拒绝。Step Functions、Zeebe、Conductor 等 DSL 引擎都做显式二分，Lemline 用单一谓词 `isActivity` 表达"能否在解释器内同步完成"。单一维度会把 `wait`（暂停但无副作用）、`emit`（有副作用但不暂停）、`set` 与 `raise`（两者皆无）分到不合适的一侧。本设计中 Block 边界与 `task_execution` 行由不同条件触发，需要 `blocking` 与 `effectful` 两个独立谓词，再加语法层面的 `composite`。三个属性的代价只是编译期多算两位，换来的是每条派生规则都能精确表述。

## Open Questions

1. 进行中的 activity 在 worker 崩溃后的默认策略：自动幂等重做，还是标记未知等待人工介入。这决定 `task_execution` 的状态机。
2. `$context` 的体积上限，以及超限后拒绝还是外置到对象存储。
3. 内存等待阈值的初值与是否按暂停类型区分。

## References

- Serverless Workflow Specification v1.0: https://github.com/serverlessworkflow/specification
- Lemline ADR-0002 Workflow Execution Model、ADR-0003 Messaging Architecture、ADR-0008 Fork Error Management、ADR-0009 Idempotent IDs、ADR-0011/0012 Listen Correlation & CloudEvent Processing
- DBOS Transact: durable execution via step checkpoints in Postgres
- Temporal: event history replay and deterministic constraints
- Restate: journal-based durable execution with immutable deployments
- Prefect 2/3: runtime-inferred task graph, server as state authority
- Dagster: software-defined asset graph, event-log-based run state, IO managers
