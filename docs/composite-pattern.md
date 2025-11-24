# Composite Pattern 实现说明

## 什么是 Composite Pattern？

Composite Pattern（组合模式）是一种设计模式，用于处理树形结构的对象。它让客户端可以统一处理单个对象和组合对象。

## 在 Serverless Workflow DSL 中的应用

Serverless Workflow DSL 定义了多种任务类型：

### 原子任务 (Atomic Tasks)
直接执行具体操作，不包含子任务：
- **Call**: 调用服务或函数
- **Set**: 设置数据
- **Emit**: 发出事件
- **Listen**: 监听事件
- **Raise**: 抛出错误
- **Wait**: 等待
- **Run**: 运行容器/脚本/workflow

### 组合任务 (Composite Tasks)
包含并管理子任务：
- **Do**: 顺序执行多个子任务
- **Fork**: 并行执行多个子任务
- **For**: 循环执行子任务
- **Switch**: 条件分支
- **Try**: 错误处理

## 实现架构

```
┌─────────────────────────────────────────┐
│         TaskExecutor                    │
│  ┌───────────────────────────────────┐  │
│  │  execute(&TaskDefinition)         │  │  ← 统一接口
│  └───────────────┬───────────────────┘  │
│                  │                       │
│        ┌─────────┴─────────┐            │
│        │                   │            │
│   ┌────▼─────┐      ┌─────▼─────┐      │
│   │  原子任务  │      │  组合任务  │      │
│   └──────────┘      └───┬───────┘      │
│   - Call                │              │
│   - Set                 │              │
│   - Emit              递归调用           │
│   - ...                 │              │
│                    ┌────▼────┐         │
│                    │ execute │         │  ← 递归！
│                    └─────────┘         │
└─────────────────────────────────────────┘
```

## 核心代码结构

### 1. 统一执行接口

```rust
impl TaskExecutor {
    pub fn execute<'a>(
        task: &'a TaskDefinition,
        ctx: &'a WorkflowContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = StepResult<Value>> + Send + 'a>> {
        Box::pin(async move {
            match task {
                // 原子任务
                TaskDefinition::Call(call) => Self::execute_call(call, ctx, input).await,

                // 组合任务 - 递归！
                TaskDefinition::Do(do_task) => Self::execute_do(do_task, ctx, input).await,
                // ...
            }
        })
    }
}
```

### 2. 原子任务实现

直接执行具体操作：

```rust
async fn execute_call(
    call: &CallTaskDefinition,
    ctx: &WorkflowContext,
    input: Value,
) -> StepResult<Value> {
    match call.call.as_str() {
        "http" => {
            let http_node = HTTPNode::try_from(call)?;
            http_node.execute(ctx, input).await
        }
        // ...
    }
}
```

### 3. 组合任务实现

递归调用 `execute()` 处理子任务：

```rust
async fn execute_do(
    do_task: &DoTaskDefinition,
    ctx: &WorkflowContext,
    input: Value,
) -> StepResult<Value> {
    let mut current_data = input;

    // 遍历所有子任务
    for entry in &do_task.do_.entries {
        for (_name, task) in entry.iter() {
            // 递归！无论子任务是原子的还是组合的，都能正确处理
            current_data = Self::execute(task, ctx, current_data).await?;
        }
    }

    Ok(current_data)
}
```

## Composite Pattern 的优势

### 1. 统一接口
```rust
// 无论任务简单还是复杂，都用同样的方式调用
let result = TaskExecutor::execute(&task, &ctx, input).await?;
```

### 2. 递归处理
```yaml
do:
  - task1:
      call: http      # 原子任务
  - task2:
      do:             # 组合任务
        - subtask1:
            call: http
        - subtask2:
            do:       # 嵌套的组合任务
              - subsubtask1:
                  call: http
```
无论嵌套多深，都能正确处理。

### 3. 类型安全
```rust
match task {
    TaskDefinition::Call(_) => { /* ... */ }
    TaskDefinition::Do(_) => { /* ... */ }
    // 编译器确保所有类型都被处理
}
```

### 4. 易于扩展
添加新任务类型只需：
1. 在 `execute()` 中添加新的 match 分支
2. 实现对应的 `execute_xxx()` 方法

## 关键技术细节

### 递归 async 函数需要 Box

Rust 的 async 函数不能直接递归，因为会产生无限大小的 Future。解决方案：

```rust
// ✗ 错误：直接递归
pub async fn execute(...) -> StepResult<Value> {
    Self::execute(...).await  // 编译错误！
}

// ✓ 正确：使用 Box::pin
pub fn execute<'a>(
    ...
) -> Pin<Box<dyn Future<Output = StepResult<Value>> + Send + 'a>> {
    Box::pin(async move {
        Self::execute(...).await  // OK！
    })
}
```

### Map 类型的访问

`serverless_workflow_core` 使用自定义的 `Map` 类型：

```rust
// Map 结构
pub struct Map<K, V> {
    pub entries: Vec<HashMap<K, V>>
}

// 访问方式
for entry in &map.entries {
    for (key, value) in entry.iter() {
        // ...
    }
}
```

## 运行示例

```bash
# 查看 Composite Pattern 演示
cargo run --example composite_pattern

# 探索 API 结构
cargo run --example explore_api
```

## 下一步

- [ ] 实现 Set 任务
- [ ] 实现 For 任务的完整逻辑
- [ ] 实现 Switch 任务
- [ ] 实现 Try/Catch 错误处理
- [ ] 实现 Fork 的 compete 模式
- [ ] 添加数据转换（input/output/export）
- [ ] 添加条件评估引擎（用于 Switch, For while）
