use serde_json::Value;
use serverless_workflow_core::models::task::TaskDefinition;
use std::future::Future;
use std::pin::Pin;

use crate::nodes::{call::CallNode, r#do::DoNode, r#for::ForNode};
use crate::runtime::{StepResult, Task, TaskCtx, TaskInput, TaskOutput};

pub struct TaskExecutor;

impl TaskExecutor {
    /// 执行任意类型的任务
    ///
    /// 这是 Composite Pattern 的核心：无论是原子任务还是组合任务，
    /// 都通过同一个接口调用，组合任务会递归调用此方法处理子任务
    pub fn execute<'a>(
        task: &'a TaskDefinition,
        ctx: &'a TaskCtx,
        input: TaskInput,
    ) -> Pin<Box<dyn Future<Output = StepResult<TaskOutput>> + Send + 'a>> {
        Box::pin(async move {
            match task {
                // ============ 原子任务 ============
                TaskDefinition::Call(call_task) => {
                    let node = CallNode::try_from(call_task)?;
                    node.execute(ctx.clone(), input).await
                }

                TaskDefinition::Set(set_task) => Self::execute_set(set_task, ctx, input).await,

                TaskDefinition::Emit(emit_task) => Self::execute_emit(emit_task, ctx, input).await,

                TaskDefinition::Listen(listen_task) => {
                    Self::execute_listen(listen_task, ctx, input).await
                }

                TaskDefinition::Raise(raise_task) => {
                    Self::execute_raise(raise_task, ctx, input).await
                }

                TaskDefinition::Wait(wait_task) => Self::execute_wait(wait_task, ctx, input).await,

                // ============ 组合任务 ============
                // 这些任务包含子任务，需要递归执行
                TaskDefinition::Do(do_task) => {
                    let node = DoNode::try_from(do_task)?;
                    node.execute(ctx.clone(), input).await
                }

                TaskDefinition::Fork(fork_task) => Self::execute_fork(fork_task, ctx, input).await,

                TaskDefinition::For(for_task) => {
                    let node = ForNode::try_from(for_task)?;
                    node.execute(ctx.clone(), input).await
                }

                TaskDefinition::Switch(switch_task) => {
                    Self::execute_switch(switch_task, ctx, input).await
                }

                TaskDefinition::Try(try_task) => Self::execute_try(try_task, ctx, input).await,

                TaskDefinition::Run(run_task) => Self::execute_run(run_task, ctx, input).await,
            }
        })
    }

    async fn execute_set(
        _set: &serverless_workflow_core::models::task::SetTaskDefinition,
        _ctx: &TaskCtx,
        _input: TaskInput,
    ) -> StepResult<TaskOutput> {
        // TODO: 实现 set 任务
        todo!("implement set task")
    }

    async fn execute_emit(
        _emit: &serverless_workflow_core::models::task::EmitTaskDefinition,
        _ctx: &TaskCtx,
        _input: TaskInput,
    ) -> StepResult<TaskOutput> {
        // TODO: 实现 emit 任务
        todo!("implement emit task")
    }

    async fn execute_listen(
        _listen: &serverless_workflow_core::models::task::ListenTaskDefinition,
        _ctx: &TaskCtx,
        _input: TaskInput,
    ) -> StepResult<TaskOutput> {
        // TODO: 实现 listen 任务
        todo!("implement listen task")
    }

    async fn execute_raise(
        _raise: &serverless_workflow_core::models::task::RaiseTaskDefinition,
        _ctx: &TaskCtx,
        _input: TaskInput,
    ) -> StepResult<TaskOutput> {
        // TODO: 实现 raise 任务
        todo!("implement raise task")
    }

    async fn execute_wait(
        _wait: &serverless_workflow_core::models::task::WaitTaskDefinition,
        _ctx: &TaskCtx,
        _input: TaskInput,
    ) -> StepResult<TaskOutput> {
        // TODO: 实现 wait 任务
        todo!("implement wait task")
    }

    async fn execute_run(
        _run: &serverless_workflow_core::models::task::RunTaskDefinition,
        _ctx: &TaskCtx,
        _input: TaskInput,
    ) -> StepResult<TaskOutput> {
        // TODO: 实现 run 任务
        todo!("implement run task")
    }

    // ========== 组合任务实现 ==========
    // 关键：这些方法会递归调用 Self::execute()

    /// Fork: 并行执行多个子任务
    async fn execute_fork(
        fork_task: &serverless_workflow_core::models::task::ForkTaskDefinition,
        ctx: &TaskCtx,
        input: TaskInput,
    ) -> StepResult<TaskOutput> {
        use futures::future::try_join_all;

        let mut futures = Vec::new();

        // 为每个分支创建一个 future - 使用 .entries 访问
        for entry in &fork_task.fork.branches.entries {
            for (_branch_name, branch_task) in entry.iter() {
                let ctx = ctx.clone();
                let input = input.clone();
                let task = branch_task.clone();

                // 递归调用 execute
                let future = async move { Self::execute(&task, &ctx, input).await };

                futures.push(future);
            }
        }

        // 并行执行所有分支
        let results = try_join_all(futures).await?;

        // TODO: 根据 fork.compete 决定返回策略
        // 现在简单返回第一个结果
        Ok(results
            .into_iter()
            .next()
            .unwrap_or_else(|| TaskOutput::new(Value::Null)))
    }

    /// Switch: 条件分支
    async fn execute_switch(
        _switch_task: &serverless_workflow_core::models::task::SwitchTaskDefinition,
        _ctx: &TaskCtx,
        _input: TaskInput,
    ) -> StepResult<TaskOutput> {
        // TODO: 实现 switch 任务
        // 需要：
        // 1. 评估条件
        // 2. 选择匹配的分支
        // 3. 递归调用 Self::execute() 执行选中的任务
        todo!("implement switch task")
    }

    /// Try: 错误处理
    async fn execute_try(
        try_task: &serverless_workflow_core::models::task::TryTaskDefinition,
        ctx: &TaskCtx,
        input: TaskInput,
    ) -> StepResult<TaskOutput> {
        // try_ 是一个 Map<String, TaskDefinition>，需要执行其中的任务
        let mut current = input;

        // 尝试执行 try 块中的所有任务
        for entry in &try_task.try_.entries {
            for (_name, task) in entry.iter() {
                match Self::execute(task, ctx, current.clone()).await {
                    Ok(output) => {
                        current = output.into();
                    }
                    Err(err) => {
                        // 如果有 catch 块，执行它
                        // TODO: 实现 ErrorCatcherDefinition 的处理
                        return Err(err);
                    }
                }
            }
        }

        Ok(current.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "workflow execution wiring not implemented yet"]
    async fn test_execute_do_task() {
        let yaml = r#"
document:
  dsl: '1.0.1'
  namespace: test
  name: do-example
  version: '0.1.0'
do:
  - step1:
      call: http
      with:
        method: get
        endpoint: https://httpbin.org/get
  - step2:
      call: http
      with:
        method: post
        endpoint: https://httpbin.org/post
"#;

        let _workflow: serverless_workflow_core::models::workflow::WorkflowDefinition =
            serde_yaml::from_str(yaml).expect("invalid yaml");

        let _ctx = TaskCtx::default();
        let _input = json!({});

        // workflow.do_ 本身就是一个 Do 任务
        // TODO: 需要从 workflow 提取任务并执行
    }
}
