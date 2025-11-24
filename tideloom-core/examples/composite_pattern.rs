use serde_json::json;
use tideloom_core::{
    Workflow,
    runtime::{TaskExecutor, WorkflowContext},
};

#[tokio::main]
async fn main() {
    // 演示 Composite Pattern
    // 这个 workflow 包含嵌套的任务：Do -> [Call, Do -> [Call, Call]]

    let yaml = r#"
document:
  dsl: '1.0.1'
  namespace: test
  name: composite-example
  version: '0.1.0'
do:
  - step1:
      call: http
      with:
        method: get
        endpoint: https://httpbin.org/get
  - nested:
      do:
        - step2a:
            call: http
            with:
              method: post
              endpoint: https://httpbin.org/post
        - step2b:
            call: http
            with:
              method: put
              endpoint: https://httpbin.org/put
"#;

    let workflow = Workflow::from_yaml(yaml);
    let ctx = WorkflowContext::default();
    let input = json!({});

    println!("Workflow: {}", workflow.definition().document.name);
    println!("Tasks in workflow.do_:");

    // 打印 workflow 结构
    for (i, entry) in workflow.definition().do_.entries.iter().enumerate() {
        println!("  Entry {}: {:?}", i, entry.keys().collect::<Vec<_>>());
        for (name, task) in entry.iter() {
            println!("    Task: {}", name);
            match task {
                serverless_workflow_core::models::task::TaskDefinition::Call(_) => {
                    println!("      Type: Call (Atomic Task)");
                }
                serverless_workflow_core::models::task::TaskDefinition::Do(do_task) => {
                    println!("      Type: Do (Composite Task)");
                    println!("      Subtasks: {}", do_task.do_.entries.len());
                }
                _ => {
                    println!("      Type: Other");
                }
            }
        }
    }

    println!("\n=== Composite Pattern 演示 ===");
    println!("统一接口处理所有任务类型:");
    println!("- 原子任务 (Call) 直接执行 HTTP 请求");
    println!("- 组合任务 (Do) 递归执行子任务");
    println!("- 无论嵌套多深，都通过 TaskExecutor::execute() 处理\n");

    // 执行 workflow - 展示递归执行
    println!("执行 workflow...");
    for (i, entry) in workflow.definition().do_.entries.iter().enumerate() {
        for (name, task) in entry.iter() {
            println!("\n执行 Task {}:  {}", i + 1, name);
            match TaskExecutor::execute(task, &ctx, input.clone()).await {
                Ok(_result) => {
                    println!("  ✓ 成功");
                }
                Err(err) => {
                    println!("  ✗ 失败: {}", err);
                }
            }
        }
    }

    println!("\n=== Composite Pattern 的优势 ===");
    println!("1. 统一接口: 所有任务通过 TaskExecutor::execute() 调用");
    println!("2. 递归处理: Do 任务递归调用 execute() 处理子任务");
    println!("3. 扩展性: 添加新任务类型只需实现对应的 execute_xxx 方法");
    println!("4. 类型安全: 编译时保证所有任务类型都被处理");
}
