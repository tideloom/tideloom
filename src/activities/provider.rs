use serde_json::Value as JsonValue;

use super::runner::{ActivityRunner, NotImplementedRunner};
use crate::nodes::node::Node;

pub struct ActivityRunnerProvider {
    runners: Vec<Box<dyn ActivityRunner + Send + Sync>>, // pluggable runners
    fallback: Box<dyn ActivityRunner + Send + Sync>,      // default runner
}

impl ActivityRunnerProvider {
    pub fn new() -> Self {
        Self { runners: Vec::new(), fallback: Box::new(NotImplementedRunner) }
    }

    pub fn with_runner<R: ActivityRunner + Send + Sync + 'static>(mut self, r: R) -> Self {
        self.runners.push(Box::new(r));
        self
    }

    pub fn add_runner<R: ActivityRunner + Send + Sync + 'static>(&mut self, r: R) {
        self.runners.push(Box::new(r));
    }

    pub fn run(&self, node: &Node) -> JsonValue {
        let runner = self
            .runners
            .iter()
            .find(|r| r.can_run(node))
            .map(|r| &**r)
            .unwrap_or(&*self.fallback);
        runner.run(node)
    }
}

