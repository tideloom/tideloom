use serverless_workflow_core::models::{task::TaskDefinition, workflow::WorkflowDefinition};

fn main() {
    let yaml = r#"
document:
  dsl: '1.0.1'
  namespace: test
  name: explore
  version: '0.1.0'
do:
  - step1:
      call: http
      with:
        method: get
        endpoint: https://httpbin.org/get
"#;

    let workflow: WorkflowDefinition = serde_yaml::from_str(yaml).unwrap();

    // Explore the structure
    println!("Workflow name: {}", workflow.document.name);
    println!("Do entries count: {}", workflow.do_.entries.len());

    for entry in &workflow.do_.entries {
        println!("\nEntry keys: {:?}", entry.keys().collect::<Vec<_>>());
        for (name, task) in entry.iter() {
            println!("  Task name: {}", name);
            match task {
                TaskDefinition::Call(call) => {
                    println!("    Call type: {}", call.call);
                }
                TaskDefinition::Do(do_task) => {
                    println!("    Do task with {} entries", do_task.do_.entries.len());
                }
                _ => println!("    Other task type"),
            }
        }
    }
}
