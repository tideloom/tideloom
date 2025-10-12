use serverless_workflow_core::models::workflow::WorkflowDefinition;

fn main() {
    println!("Hello, world!");
}

struct Workflow {
    workflow_definition: WorkflowDefinition,
}

impl Workflow {
    fn from_yaml(yaml: &str) -> Self {
        let workflow_definition: WorkflowDefinition = serde_yaml::from_str(yaml).unwrap();
        Self {
            workflow_definition,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_yaml() {
        let yaml = "
document:
  dsl: '1.0.0'
  namespace: default
  name: call-http
  version: '1.0.0'
do:
- getPet:
    call: http
    with:
      method: get
      endpoint: https://petstore.swagger.io/v2/pet/{petId}
        ";
        let workflow = Workflow::from_yaml(yaml);
        assert_eq!(workflow.workflow_definition.document.name, "call-http");
    }
}
