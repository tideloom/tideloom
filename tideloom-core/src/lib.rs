pub mod runtime;
pub mod nodes;

use serverless_workflow_core::models::workflow::WorkflowDefinition;

/// Wrapper around `WorkflowDefinition` with convenience constructors.
pub struct Workflow {
    workflow_definition: WorkflowDefinition,
}

impl Workflow {
    /// Builds a workflow from a YAML string. Panics if input is invalid.
    pub fn from_yaml(yaml: &str) -> Self {
        let workflow_definition: WorkflowDefinition =
            serde_yaml::from_str(yaml).expect("invalid workflow yaml");
        Self {
            workflow_definition,
        }
    }

    /// Returns the underlying workflow definition.
    pub fn definition(&self) -> &WorkflowDefinition {
        &self.workflow_definition
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
        assert_eq!(
            workflow.definition().document.name,
            "call-http"
        );
    }
}
