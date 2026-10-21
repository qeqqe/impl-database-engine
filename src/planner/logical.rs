use std::fmt;

use crate::planner::catalog::IndexDef;
use crate::sql::{ColumnDef, JoinType, SetOperator, TransactionMode};

use super::catalog::TableSchema;
use super::error::PlanResult;
use super::expr::{AggregateCall, ScalarExpr, SortKey};
use super::schema::{Field, Schema};

#[derive(Debug, Clone, PartialEq)]
pub enum LogicalPlan {
    Scan {
        table: String,
        alias: Option<String>,
        projection: Vec<usize>,
        schema: Schema,
    },
    Values {
        rows: Vec<Vec<ScalarExpr>>,
        schema: Schema,
    },
    Filter {
        input: Box<LogicalPlan>,
        predicate: ScalarExpr,
    },
    Project {
        input: Box<LogicalPlan>,
        exprs: Vec<ScalarExpr>,
        schema: Schema,
    },
    Join {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        join_type: JoinType,
        on: Option<ScalarExpr>,
        schema: Schema,
    },
    Aggregate {
        input: Box<LogicalPlan>,
        group_by: Vec<ScalarExpr>,
        aggregates: Vec<AggregateCall>,
        schema: Schema,
    },
    Distinct {
        input: Box<LogicalPlan>,
    },
    Sort {
        input: Box<LogicalPlan>,
        keys: Vec<SortKey>,
    },
    Limit {
        input: Box<LogicalPlan>,
        limit: Option<usize>,
        offset: usize,
    },
    SetOperation {
        op: SetOperator,
        all: bool,
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        schema: Schema,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateAssignment {
    pub column: usize,
    pub value: ScalarExpr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    Query(LogicalPlan),
    Insert {
        table: String,
        input: LogicalPlan,
    },
    Update {
        table: String,
        input: LogicalPlan,
        assignments: Vec<UpdateAssignment>,
    },
    Delete {
        table: String,
        input: LogicalPlan,
    },
    CreateTable {
        schema: TableSchema,
        if_not_exists: bool,
    },
    DropTable {
        name: String,
        if_exists: bool,
    },
    CreateIndex {
        index: IndexDef,
        if_not_exists: bool,
    },
    DropIndex {
        name: String,
        if_exists: bool,
    },
    AlterTable {
        table: String,
        action: AlterAction,
    },
    Begin(TransactionMode),
    Commit,
    Rollback,
    ShowTables,
    Describe(String),
    Explain {
        analyze: bool,
        plan: Box<Plan>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlterAction {
    AddColumn { column: ColumnDef, fill: ScalarExpr },
    DropColumn { column: usize },
    RenameTable { name: String },
    RenameColumn { column: usize, name: String },
    Nothing,
}

impl LogicalPlan {
    pub fn filter(input: LogicalPlan, predicate: ScalarExpr) -> LogicalPlan {
        LogicalPlan::Filter {
            input: Box::new(input),
            predicate,
        }
    }

    pub fn project(input: LogicalPlan, exprs: Vec<ScalarExpr>, fields: Vec<Field>) -> LogicalPlan {
        LogicalPlan::Project {
            input: Box::new(input),
            exprs,
            schema: Schema::new(fields),
        }
    }

    pub fn join(
        left: LogicalPlan,
        right: LogicalPlan,
        join_type: JoinType,
        on: Option<ScalarExpr>,
    ) -> LogicalPlan {
        let left_nullable = matches!(join_type, JoinType::Right | JoinType::Full);
        let right_nullable = matches!(join_type, JoinType::Left | JoinType::Full);
        let schema = left
            .schema()
            .with_nullable(left_nullable)
            .concat(&right.schema().with_nullable(right_nullable));
        LogicalPlan::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type,
            on,
            schema,
        }
    }

    pub fn schema(&self) -> &Schema {
        match self {
            LogicalPlan::Scan { schema, .. }
            | LogicalPlan::Values { schema, .. }
            | LogicalPlan::Project { schema, .. }
            | LogicalPlan::Join { schema, .. }
            | LogicalPlan::Aggregate { schema, .. }
            | LogicalPlan::SetOperation { schema, .. } => schema,
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Distinct { input }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. } => input.schema(),
        }
    }

    pub fn children(&self) -> Vec<&LogicalPlan> {
        match self {
            LogicalPlan::Scan { .. } | LogicalPlan::Values { .. } => vec![],
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Project { input, .. }
            | LogicalPlan::Aggregate { input, .. }
            | LogicalPlan::Distinct { input }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. } => vec![input],
            LogicalPlan::Join { left, right, .. }
            | LogicalPlan::SetOperation { left, right, .. } => {
                vec![left, right]
            }
        }
    }

    pub fn map_children<F>(self, f: F) -> PlanResult<LogicalPlan>
    where
        F: FnMut(LogicalPlan) -> PlanResult<LogicalPlan>,
    {
        self.try_map_children(f)
    }

    pub fn try_map_children<E, F>(self, mut f: F) -> Result<LogicalPlan, E>
    where
        F: FnMut(LogicalPlan) -> Result<LogicalPlan, E>,
    {
        let mut boxed =
            |p: Box<LogicalPlan>| -> Result<Box<LogicalPlan>, E> { Ok(Box::new(f(*p)?)) };
        Ok(match self {
            leaf @ (LogicalPlan::Scan { .. } | LogicalPlan::Values { .. }) => leaf,
            LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
                input: boxed(input)?,
                predicate,
            },
            LogicalPlan::Project {
                input,
                exprs,
                schema,
            } => LogicalPlan::Project {
                input: boxed(input)?,
                exprs,
                schema,
            },
            LogicalPlan::Join {
                left,
                right,
                join_type,
                on,
                ..
            } => {
                let left = boxed(left)?;
                let right = boxed(right)?;
                LogicalPlan::join(*left, *right, join_type, on)
            }
            LogicalPlan::Aggregate {
                input,
                group_by,
                aggregates,
                schema,
            } => LogicalPlan::Aggregate {
                input: boxed(input)?,
                group_by,
                aggregates,
                schema,
            },
            LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
                input: boxed(input)?,
            },
            LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
                input: boxed(input)?,
                keys,
            },
            LogicalPlan::Limit {
                input,
                limit,
                offset,
            } => LogicalPlan::Limit {
                input: boxed(input)?,
                limit,
                offset,
            },
            LogicalPlan::SetOperation {
                op,
                all,
                left,
                right,
                schema,
            } => LogicalPlan::SetOperation {
                op,
                all,
                left: boxed(left)?,
                right: boxed(right)?,
                schema,
            },
        })
    }

    pub fn transform_up<F>(self, f: &mut F) -> PlanResult<LogicalPlan>
    where
        F: FnMut(LogicalPlan) -> PlanResult<LogicalPlan>,
    {
        self.try_transform_up(f)
    }

    pub fn try_transform_up<E, F>(self, f: &mut F) -> Result<LogicalPlan, E>
    where
        F: FnMut(LogicalPlan) -> Result<LogicalPlan, E>,
    {
        let rewritten = self.try_map_children(|child| child.try_transform_up(f))?;
        f(rewritten)
    }

    pub fn map_exprs<E, F>(self, mut f: F) -> Result<LogicalPlan, E>
    where
        F: FnMut(ScalarExpr) -> Result<ScalarExpr, E>,
    {
        let mut all = |exprs: Vec<ScalarExpr>| -> Result<Vec<ScalarExpr>, E> {
            exprs.into_iter().map(&mut f).collect()
        };
        Ok(match self {
            LogicalPlan::Filter { input, predicate } => {
                let predicate = all(vec![predicate])?.remove(0);
                LogicalPlan::Filter { input, predicate }
            }
            LogicalPlan::Project {
                input,
                exprs,
                schema,
            } => LogicalPlan::Project {
                input,
                exprs: all(exprs)?,
                schema,
            },
            LogicalPlan::Values { rows, schema } => LogicalPlan::Values {
                rows: rows.into_iter().map(&mut all).collect::<Result<_, E>>()?,
                schema,
            },
            LogicalPlan::Join {
                left,
                right,
                join_type,
                on,
                schema,
            } => LogicalPlan::Join {
                left,
                right,
                join_type,
                on: match on {
                    Some(on) => Some(all(vec![on])?.remove(0)),
                    None => None,
                },
                schema,
            },
            LogicalPlan::Aggregate {
                input,
                group_by,
                aggregates,
                schema,
            } => {
                let group_by = all(group_by)?;
                let aggregates = aggregates
                    .into_iter()
                    .map(|a| {
                        Ok(AggregateCall {
                            arg: match a.arg {
                                Some(arg) => Some(all(vec![arg])?.remove(0)),
                                None => None,
                            },
                            ..a
                        })
                    })
                    .collect::<Result<_, E>>()?;
                LogicalPlan::Aggregate {
                    input,
                    group_by,
                    aggregates,
                    schema,
                }
            }
            LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
                input,
                keys: keys
                    .into_iter()
                    .map(|k| {
                        Ok(SortKey {
                            expr: all(vec![k.expr])?.remove(0),
                            ..k
                        })
                    })
                    .collect::<Result<_, E>>()?,
            },
            other => other,
        })
    }

    pub fn exprs(&self) -> Vec<&ScalarExpr> {
        match self {
            LogicalPlan::Filter { predicate, .. } => vec![predicate],
            LogicalPlan::Project { exprs, .. } => exprs.iter().collect(),
            LogicalPlan::Values { rows, .. } => rows.iter().flatten().collect(),
            LogicalPlan::Join { on, .. } => on.iter().collect(),
            LogicalPlan::Aggregate {
                group_by,
                aggregates,
                ..
            } => group_by
                .iter()
                .chain(aggregates.iter().filter_map(|a| a.arg.as_ref()))
                .collect(),
            LogicalPlan::Sort { keys, .. } => keys.iter().map(|k| &k.expr).collect(),
            _ => vec![],
        }
    }

    pub fn contains_subquery(&self) -> bool {
        self.exprs().iter().any(|e| e.contains_subquery())
            || self.children().iter().any(|c| c.contains_subquery())
    }

    pub fn transform_down<F>(self, f: &mut F) -> PlanResult<LogicalPlan>
    where
        F: FnMut(LogicalPlan) -> PlanResult<LogicalPlan>,
    {
        f(self)?.map_children(|child| child.transform_down(f))
    }

    pub fn empty(schema: Schema) -> LogicalPlan {
        LogicalPlan::Values {
            rows: vec![],
            schema,
        }
    }

    pub fn is_empty_relation(&self) -> bool {
        matches!(self, LogicalPlan::Values { rows, .. } if rows.is_empty())
    }

    fn describe(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogicalPlan::Scan {
                table,
                alias,
                schema,
                ..
            } => {
                write!(f, "Scan: {table}")?;
                if let Some(a) = alias {
                    write!(f, " AS {a}")?;
                }
                write!(f, " {}", ColumnList(schema))
            }
            LogicalPlan::Values { rows, schema } => {
                write!(f, "Values: {} rows {}", rows.len(), ColumnList(schema))
            }
            LogicalPlan::Filter { predicate, .. } => write!(f, "Filter: {predicate}"),
            LogicalPlan::Project { exprs, schema, .. } => {
                f.write_str("Project: ")?;
                for (i, (expr, field)) in exprs.iter().zip(schema.fields()).enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    let rendered = expr.to_string();
                    if rendered == field.qualified_name() {
                        write!(f, "{rendered}")?;
                    } else {
                        write!(f, "{rendered} AS {}", field.name)?;
                    }
                }
                Ok(())
            }
            LogicalPlan::Join { join_type, on, .. } => {
                write!(f, "Join: {join_type}")?;
                if let Some(on) = on {
                    write!(f, " ON {on}")?;
                }
                Ok(())
            }
            LogicalPlan::Aggregate {
                group_by,
                aggregates,
                ..
            } => {
                f.write_str("Aggregate: group_by=[")?;
                write_joined(f, group_by)?;
                f.write_str("] aggregates=[")?;
                write_joined(f, aggregates)?;
                f.write_str("]")
            }
            LogicalPlan::Distinct { .. } => f.write_str("Distinct"),
            LogicalPlan::Sort { keys, .. } => {
                f.write_str("Sort: ")?;
                write_joined(f, keys)
            }
            LogicalPlan::Limit { limit, offset, .. } => match limit {
                Some(l) => write!(f, "Limit: limit={l} offset={offset}"),
                None => write!(f, "Limit: limit=ALL offset={offset}"),
            },
            LogicalPlan::SetOperation {
                op, all, schema, ..
            } => {
                write!(f, "SetOperation: {op}")?;
                if *all {
                    f.write_str(" ALL")?;
                }
                write!(f, " {}", ColumnList(schema))
            }
        }
    }

    fn fmt_tree(&self, f: &mut fmt::Formatter<'_>, depth: usize) -> fmt::Result {
        write!(f, "{:indent$}", "", indent = depth * 2)?;
        self.describe(f)?;
        writeln!(f)?;
        for child in self.children() {
            child.fmt_tree(f, depth + 1)?;
        }
        Ok(())
    }
}

impl fmt::Display for LogicalPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_tree(f, 0)
    }
}

pub(crate) struct ColumnList<'a>(pub &'a Schema);

impl fmt::Display for ColumnList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[")?;
        for (i, field) in self.0.fields().iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(&field.name)?;
        }
        f.write_str("]")
    }
}

pub(crate) fn write_joined<T: fmt::Display>(
    f: &mut fmt::Formatter<'_>,
    items: &[T],
) -> fmt::Result {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}
