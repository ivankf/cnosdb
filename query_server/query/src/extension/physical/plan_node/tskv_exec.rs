#![allow(clippy::too_many_arguments)]
use std::any::Any;
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use std::task::Poll;

use coordinator::reader::ReaderIterator;
use coordinator::service::CoordinatorRef;
use datafusion::arrow::datatypes::{SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result as DFResult};
use datafusion::execution::context::TaskContext;
use datafusion::physical_expr::PhysicalSortExpr;
use datafusion::physical_plan::metrics::{ExecutionPlanMetricsSet, MetricsSet};
use datafusion::physical_plan::{
    DisplayFormatType, ExecutionPlan, Partitioning, RecordBatchStream, SendableRecordBatchStream,
    Statistics,
};
use futures::{FutureExt, Stream};
use models::codec::Encoding;
use models::predicate::domain::PredicateRef;
use models::predicate::Split;
use models::schema::{ColumnType, TableColumn, TskvTableSchema, TskvTableSchemaRef, TIME_FIELD};
use spi::{QueryError, Result};
use trace::debug;
use tskv::query_iterator::{QueryOption, TableScanMetrics};

#[derive(Debug, Clone)]
pub struct TskvExec {
    // connection
    // db: CustomDataSource,
    table_schema: TskvTableSchemaRef,
    proj_schema: SchemaRef,
    filter: PredicateRef,
    coord: CoordinatorRef,
    splits: Vec<Split>,

    /// Execution metrics
    metrics: ExecutionPlanMetricsSet,
}

impl TskvExec {
    pub(crate) fn new(
        table_schema: TskvTableSchemaRef,
        proj_schema: SchemaRef,
        filter: PredicateRef,
        coord: CoordinatorRef,
        splits: Vec<Split>,
    ) -> Self {
        let metrics = ExecutionPlanMetricsSet::new();

        Self {
            table_schema,
            proj_schema,
            filter,
            coord,
            splits,
            metrics,
        }
    }
    pub fn filter(&self) -> PredicateRef {
        self.filter.clone()
    }
}

impl ExecutionPlan for TskvExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.proj_schema.clone()
    }

    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(self.splits.len())
    }

    fn output_ordering(&self) -> Option<&[PhysicalSortExpr]> {
        None
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _: Vec<Arc<dyn ExecutionPlan>>,
    ) -> DFResult<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(TskvExec {
            table_schema: self.table_schema.clone(),
            proj_schema: self.proj_schema.clone(),
            filter: self.filter.clone(),
            coord: self.coord.clone(),
            splits: self.splits.clone(),
            metrics: self.metrics.clone(),
        }))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> DFResult<SendableRecordBatchStream> {
        debug!(
            "Start TskvExec::execute for partition {} of context session_id {} and task_id {:?}",
            partition,
            context.session_id(),
            context.task_id()
        );

        let split = unsafe {
            debug_assert!(partition < self.splits.len(), "Partition not exists");
            self.splits.get_unchecked(partition).clone()
        };

        debug!("Split of partition: {:?}", split);

        let batch_size = context.session_config().batch_size();

        let metrics = TableScanMetrics::new(&self.metrics, partition, Some(context.memory_pool()));

        let table_stream = TableScanStream::new(
            self.table_schema.clone(),
            self.schema(),
            self.coord.clone(),
            split,
            batch_size,
            metrics,
        )
        .map_err(|err| DataFusionError::External(Box::new(err)))?;

        Ok(Box::pin(table_stream))
    }

    fn fmt_as(&self, t: DisplayFormatType, f: &mut Formatter) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default => {
                let filter = self.filter();
                let fields: Vec<_> = self
                    .proj_schema
                    .fields()
                    .iter()
                    .map(|x| x.name().to_owned())
                    .collect::<Vec<String>>();
                write!(
                    f,
                    "TskvExec: {}, projection=[{}]",
                    PredicateDisplay(&filter),
                    fields.join(","),
                )
            }
        }
    }

    fn statistics(&self) -> Statistics {
        // TODO
        Statistics::default()
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }
}

/// A wrapper to customize PredicateRef display
#[derive(Debug)]
struct PredicateDisplay<'a>(&'a PredicateRef);

impl<'a> Display for PredicateDisplay<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let filter = self.0;
        write!(
            f,
            "limit={:?}, predicate={:?}",
            filter.limit(),
            filter.filter(),
        )
    }
}

#[allow(dead_code)]
pub struct TableScanStream {
    proj_schema: SchemaRef,
    batch_size: usize,
    coord: CoordinatorRef,

    iterator: ReaderIterator,

    remain: Option<usize>,
    metrics: TableScanMetrics,
}

impl TableScanStream {
    pub fn new(
        table_schema: TskvTableSchemaRef,
        proj_schema: SchemaRef,
        coord: CoordinatorRef,
        split: Split,
        batch_size: usize,
        metrics: TableScanMetrics,
    ) -> Result<Self> {
        let mut proj_fileds = Vec::with_capacity(proj_schema.fields().len());
        for item in proj_schema.fields().iter() {
            let field_name = item.name();
            if field_name == TIME_FIELD {
                let (encoding, column_type) = match table_schema.column(TIME_FIELD) {
                    None => (Encoding::Default, ColumnType::Time(TimeUnit::Nanosecond)),
                    Some(v) => (v.encoding, v.column_type.clone()),
                };
                proj_fileds.push(TableColumn::new(
                    0,
                    TIME_FIELD.to_string(),
                    column_type,
                    encoding,
                ));
                continue;
            }

            if let Some(v) = table_schema.column(field_name) {
                proj_fileds.push(v.clone());
            } else {
                return Err(QueryError::CommonError {
                    msg: format!(
                        "table stream build fail, because can't found field: {}",
                        field_name
                    ),
                });
            }
        }

        let proj_table_schema = TskvTableSchema::new(
            table_schema.tenant.clone(),
            table_schema.db.clone(),
            table_schema.name.clone(),
            proj_fileds,
        );

        let remain = split.limit();
        let option = QueryOption::new(
            batch_size,
            split,
            None,
            proj_schema.clone(),
            proj_table_schema,
            metrics.tskv_metrics(),
        );

        let iterator = coord.read_record(option)?;

        Ok(Self {
            proj_schema,
            batch_size,
            coord,
            remain,
            iterator,
            metrics,
        })
    }

    pub fn with_iterator(
        proj_schema: SchemaRef,
        batch_size: usize,
        coord: CoordinatorRef,
        iterator: ReaderIterator,
        remain: Option<usize>,
        metrics: TableScanMetrics,
    ) -> Self {
        Self {
            proj_schema,
            batch_size,
            coord,
            iterator,
            remain,
            metrics,
        }
    }
}

impl Stream for TableScanStream {
    type Item = std::result::Result<RecordBatch, DataFusionError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let metrics = &this.metrics;
        let timer = metrics.elapsed_compute().timer();

        let result = match Box::pin(this.iterator.next()).poll_unpin(cx) {
            Poll::Ready(Some(Ok(batch))) => match metrics.record_memory(&batch) {
                Ok(_) => match this.remain.as_mut() {
                    Some(remain) => {
                        if *remain == 0 {
                            Poll::Ready(None)
                        } else if *remain > batch.num_rows() {
                            *remain -= batch.num_rows();
                            Poll::Ready(Some(Ok(batch)))
                        } else {
                            let batch = batch.slice(0, *remain);
                            *remain = 0;
                            Poll::Ready(Some(Ok(batch)))
                        }
                    }
                    None => Poll::Ready(Some(Ok(batch))),
                },
                Err(e) => Poll::Ready(Some(Err(e))),
            },
            Poll::Ready(Some(Err(e))) => {
                Poll::Ready(Some(Err(DataFusionError::External(Box::new(e)))))
            }
            Poll::Ready(None) => {
                metrics.done();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        };

        timer.done();
        metrics.record_poll(result)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // todo   (self.data.len(), Some(self.data.len()))
        (0, Some(0))
    }
}

impl RecordBatchStream for TableScanStream {
    fn schema(&self) -> SchemaRef {
        self.proj_schema.clone()
    }
}
