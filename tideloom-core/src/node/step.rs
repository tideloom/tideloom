use std::any::Any;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use futures::future::try_join_all;

type TaskOutput = Arc<dyn Any + Send + Sync>;
// TODO: String maybe not good
type TaskInput = HashMap<String, TaskOutput>;

#[derive(Debug)]
pub struct StepError {
    inner: Box<dyn StdError + Send + Sync>,
}

impl StepError {
    pub fn new(inner: impl StdError + Send + Sync + 'static) -> Self {
        Self {
            inner: Box::new(inner),
        }
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self::new(MessageError(message.into()))
    }
}

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.inner, f)
    }
}

impl StdError for StepError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.inner.source()
    }
}

impl<E> From<E> for StepError
where
    E: StdError + Send + Sync + 'static,
{
    fn from(error: E) -> Self {
        StepError::new(error)
    }
}

impl From<String> for StepError {
    fn from(value: String) -> Self {
        StepError::message(value)
    }
}

impl From<&str> for StepError {
    fn from(value: &str) -> Self {
        StepError::message(value)
    }
}

#[derive(Debug)]
struct MessageError(String);

impl fmt::Display for MessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl StdError for MessageError {}

pub type StepResult<T> = std::result::Result<T, StepError>;

pub trait StepType: Send + Sync {
    type Input: Send;
    type Output: Send;

    async fn execute(&self, input: Self::Input) -> StepResult<Self::Output>;
}

pub struct MapStep<F> {
    function: F,
}

impl<F> MapStep<F> {
    pub fn new(function: F) -> Self {
        Self { function }
    }
}

impl<F, I, O, Fut, E> StepType for MapStep<F>
where
    F: Fn(I) -> Fut + Send + Sync,
    Fut: Future<Output = std::result::Result<O, E>> + Send,
    E: Into<StepError>,
    I: Send,
    O: Send,
{
    type Input = Vec<I>;
    type Output = Vec<O>;

    async fn execute(&self, input: Self::Input) -> StepResult<Self::Output> {
        try_join_all(input.into_iter().map(&self.function))
            .await
            .map_err(Into::into)
    }
}
