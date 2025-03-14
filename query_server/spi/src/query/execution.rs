use std::fmt::Display;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use coordinator::service::CoordinatorRef;
use datafusion::arrow::datatypes::{Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use meta::model::MetaRef;

use super::dispatcher::{QueryInfo, QueryStatus};
use super::logical_planner::Plan;
use super::session::SessionCtx;
use crate::service::protocol::{Query, QueryId};
use crate::Result;

pub type QueryExecutionRef = Arc<dyn QueryExecution>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryType {
    Batch,
    Stream,
}

impl Display for QueryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Batch => write!(f, "batch"),
            Self::Stream => write!(f, "stream"),
        }
    }
}

#[async_trait]
pub trait QueryExecution: Send + Sync {
    fn query_type(&self) -> QueryType {
        QueryType::Batch
    }
    // 开始
    async fn start(&self) -> Result<Output>;
    // 停止
    fn cancel(&self) -> Result<()>;
    // query状态
    // 查询计划
    // 静态信息
    fn info(&self) -> QueryInfo;
    // 运行时信息
    fn status(&self) -> QueryStatus;
    // sql
    // 资源占用（cpu时间/内存/吞吐量等）
    // ......
}

#[derive(Clone)]
pub enum Output {
    StreamData(SchemaRef, Vec<RecordBatch>),
    Nil(()),
}

impl Output {
    pub fn schema(&self) -> SchemaRef {
        match self {
            Self::StreamData(schema, _) => schema.clone(),
            Self::Nil(_) => Arc::new(Schema::empty()),
        }
    }

    pub fn chunk_result(&self) -> &[RecordBatch] {
        match self {
            Self::StreamData(_, result) => result,
            Self::Nil(_) => &[],
        }
    }

    pub fn num_rows(&self) -> usize {
        self.chunk_result().iter().map(|e| e.num_rows()).sum()
    }

    /// Returns the number of records affected by the query operation
    ///
    /// If it is a select statement, returns the number of rows in the result set
    ///
    /// -1 means unknown
    ///
    /// panic! when StreamData's number of records greater than i64::Max
    pub fn affected_rows(&self) -> i64 {
        match self {
            Self::StreamData(_, result) => result
                .iter()
                .map(|e| e.num_rows())
                .reduce(|p, c| p + c)
                .unwrap_or(0) as i64,
            Self::Nil(_) => 0,
        }
    }
}

pub trait QueryExecutionFactory {
    fn create_query_execution(
        &self,
        plan: Plan,
        query_state_machine: QueryStateMachineRef,
    ) -> QueryExecutionRef;
}

pub type QueryStateMachineRef = Arc<QueryStateMachine>;

pub struct QueryStateMachine {
    pub session: SessionCtx,
    pub query_id: QueryId,
    pub query: Query,
    pub meta: MetaRef,
    pub coord: CoordinatorRef,

    state: AtomicPtr<QueryState>,
    start: Instant,
}

impl QueryStateMachine {
    pub fn begin(
        query_id: QueryId,
        query: Query,
        session: SessionCtx,
        coord: CoordinatorRef,
    ) -> Self {
        let meta = coord.meta_manager();

        Self {
            query_id,
            session,
            query,
            meta,
            coord,
            state: AtomicPtr::new(Box::into_raw(Box::new(QueryState::ACCEPTING))),
            start: Instant::now(),
        }
    }

    pub fn begin_analyze(&self) {
        // TODO record time
        self.translate_to(Box::new(QueryState::RUNNING(RUNNING::ANALYZING)));
    }

    pub fn end_analyze(&self) {
        // TODO record time
    }

    pub fn begin_optimize(&self) {
        // TODO record time
        self.translate_to(Box::new(QueryState::RUNNING(RUNNING::OPTMIZING)));
    }

    pub fn end_optimize(&self) {
        // TODO
    }

    pub fn begin_schedule(&self) {
        // TODO
        self.translate_to(Box::new(QueryState::RUNNING(RUNNING::SCHEDULING)));
    }

    pub fn end_schedule(&self) {
        // TODO
    }

    pub fn finish(&self) {
        // TODO
        self.translate_to(Box::new(QueryState::DONE(DONE::FINISHED)));
    }

    pub fn cancel(&self) {
        // TODO
        self.translate_to(Box::new(QueryState::DONE(DONE::CANCELLED)));
    }

    pub fn fail(&self) {
        // TODO
        self.translate_to(Box::new(QueryState::DONE(DONE::FAILED)));
    }

    pub fn state(&self) -> &QueryState {
        unsafe { &*self.state.load(Ordering::Relaxed) }
    }

    pub fn duration(&self) -> Duration {
        self.start.elapsed()
    }

    fn translate_to(&self, state: Box<QueryState>) {
        self.state.store(Box::into_raw(state), Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
pub enum QueryState {
    ACCEPTING,
    RUNNING(RUNNING),
    DONE(DONE),
}

impl AsRef<str> for QueryState {
    fn as_ref(&self) -> &str {
        match self {
            QueryState::ACCEPTING => "ACCEPTING",
            QueryState::RUNNING(e) => e.as_ref(),
            QueryState::DONE(e) => e.as_ref(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum RUNNING {
    DISPATCHING,
    ANALYZING,
    OPTMIZING,
    SCHEDULING,
}

impl AsRef<str> for RUNNING {
    fn as_ref(&self) -> &str {
        match self {
            Self::DISPATCHING => "DISPATCHING",
            Self::ANALYZING => "ANALYZING",
            Self::OPTMIZING => "OPTMIZING",
            Self::SCHEDULING => "SCHEDULING",
        }
    }
}

#[derive(Debug, Clone)]
pub enum DONE {
    FINISHED,
    FAILED,
    CANCELLED,
}

impl AsRef<str> for DONE {
    fn as_ref(&self) -> &str {
        match self {
            Self::FINISHED => "FINISHED",
            Self::FAILED => "FAILED",
            Self::CANCELLED => "CANCELLED",
        }
    }
}
