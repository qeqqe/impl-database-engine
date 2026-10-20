use core::fmt;
use std::ops::Bound;

use crate::{
    planner::{
        expr::AggregateFunction,
        logical::{ColumnList, write_joined},
        physical_expr::PhysExpr,
        schema::Schema,
    },
    sql::JoinType,
};
// tis stores the node's id range
#[derive(Clone, Copy, PartialEq, Debug, Eq)]
pub struct KeyRange {
    pub upper: Bound<i64>,
    pub lower: Bound<i64>,
}

impl KeyRange {
    pub fn full() -> Self {
        Self {
            upper: Bound::Unbounded,
            lower: Bound::Unbounded,
        }
    }

    pub fn point(key: i64) -> Self {
        Self {
            upper: Bound::Included(key),
            lower: Bound::Included(key),
        }
    }

    pub fn contains(&self, key: i64) -> bool {
        let above_lower = match self.lower {
            Bound::Included(l) => key >= l,
            Bound::Excluded(l) => key > l,
            Bound::Unbounded => true,
        };

        let below_upper = match self.upper {
            Bound::Included(u) => key <= u,
            Bound::Excluded(u) => key < u,
            Bound::Unbounded => true,
        };

        below_upper && above_lower
    }

    pub fn is_empty(&self) -> bool {
        match (self.lower, self.upper) {
            (Bound::Included(l), Bound::Included(u)) => l > u,
            (Bound::Included(l), Bound::Excluded(u)) | (Bound::Excluded(l), Bound::Included(u)) => {
                l >= u
            }
            (Bound::Excluded(l), Bound::Excluded(u)) => l >= u || l + 1 == u,
            _ => false,
        }
    }

    pub fn intersect(&self, other: &KeyRange) -> KeyRange {
        KeyRange {
            lower: tighter_lower(self.lower, other.lower),
            upper: tighter_upper(self.upper, other.upper),
        }
    }
}

fn tighter_lower(a: Bound<i64>, b: Bound<i64>) -> Bound<i64> {
    match (a, b) {
        (Bound::Unbounded, x) | (x, Bound::Unbounded) => x,
        (Bound::Included(x), Bound::Included(y)) => Bound::Included(x.max(y)),
        (Bound::Excluded(x), Bound::Excluded(y)) => Bound::Excluded(x.max(y)),
        (Bound::Included(i), Bound::Excluded(e)) | (Bound::Excluded(e), Bound::Included(i)) => {
            if i > e {
                Bound::Included(i)
            } else {
                Bound::Excluded(e)
            }
        }
    }
}

fn tighter_upper(a: Bound<i64>, b: Bound<i64>) -> Bound<i64> {
    match (a, b) {
        (Bound::Unbounded, x) | (x, Bound::Unbounded) => x,
        (Bound::Included(x), Bound::Included(y)) => Bound::Included(x.min(y)),
        (Bound::Excluded(x), Bound::Excluded(y)) => Bound::Excluded(x.min(y)),
        (Bound::Included(i), Bound::Excluded(e)) | (Bound::Excluded(e), Bound::Included(i)) => {
            if i < e {
                Bound::Included(i)
            } else {
                Bound::Excluded(e)
            }
        }
    }
}

impl fmt::Display for KeyRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.lower {
            Bound::Included(l) => write!(f, "[{l}")?,
            Bound::Excluded(l) => write!(f, "({l}")?,
            Bound::Unbounded => f.write_str("(-inf")?,
        }
        f.write_str(", ")?;
        match self.upper {
            Bound::Included(u) => write!(f, "{u}]"),
            Bound::Excluded(u) => write!(f, "{u})"),
            Bound::Unbounded => f.write_str("+inf)"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhysAggregate {
    pub func: AggregateFunction,
    pub arg: Option<PhysExpr>,
    pub distinct: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhysSortKey {
    pub expr: PhysExpr,
    pub ascending: bool,
    pub nulls_first: bool,
}

#[derive(Clone, PartialEq, Debug)]
pub enum PhysicalPlan {
    SeqScan {
        table: String,
        projection: Vec<usize>,
        schema: Schema,
    },
    IndexScan {
        table: String,
        key_column: usize,
        range: KeyRange,
        projection: Vec<usize>,
        schema: Schema,
    },
    Values {
        rows: Vec<Vec<PhysExpr>>,
        schema: Schema,
    },
    Filter {
        input: Box<PhysicalPlan>,
        predicate: PhysExpr,
    },
    Project {
        input: Box<PhysicalPlan>,
        exprs: Vec<PhysExpr>,
        schema: Schema,
    },
    NestedLoopJoin {
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        join_type: JoinType,
        predicate: Option<PhysExpr>,
        schema: Schema,
    },
    HashJoin {
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        join_type: JoinType,
        left_keys: Vec<PhysExpr>,
        right_keys: Vec<PhysExpr>,
        residual: Option<PhysExpr>,
        schema: Schema,
    },
    HashAggregate {
        input: Box<PhysicalPlan>,
        group_by: Vec<PhysExpr>,
        aggregates: Vec<PhysAggregate>,
        schema: Schema,
    },
    Sort {
        input: Box<PhysicalPlan>,
        keys: Vec<PhysSortKey>,
    },
    TopN {
        input: Box<PhysicalPlan>,
        keys: Vec<PhysSortKey>,
        limit: usize,
        offset: usize,
    },
    Limit {
        input: Box<PhysicalPlan>,
        limit: Option<usize>,
        offset: usize,
    },
    Distinct {
        input: Box<PhysicalPlan>,
    },
}

impl PhysicalPlan {
    pub fn schema(&self) -> &Schema {
        match self {
            PhysicalPlan::SeqScan { schema, .. }
            | PhysicalPlan::IndexScan { schema, .. }
            | PhysicalPlan::Values { schema, .. }
            | PhysicalPlan::Project { schema, .. }
            | PhysicalPlan::NestedLoopJoin { schema, .. }
            | PhysicalPlan::HashJoin { schema, .. }
            | PhysicalPlan::HashAggregate { schema, .. } => schema,

            PhysicalPlan::Filter { input, .. }
            | PhysicalPlan::Sort { input, .. }
            | PhysicalPlan::TopN { input, .. }
            | PhysicalPlan::Limit { input, .. }
            | PhysicalPlan::Distinct { input } => input.schema(),
        }
    }

    pub fn children(&self) -> Vec<&PhysicalPlan> {
        match self {
            PhysicalPlan::SeqScan { .. }
            | PhysicalPlan::IndexScan { .. }
            | PhysicalPlan::Values { .. } => vec![],
            PhysicalPlan::Filter { input, .. }
            | PhysicalPlan::Project { input, .. }
            | PhysicalPlan::HashAggregate { input, .. }
            | PhysicalPlan::Sort { input, .. }
            | PhysicalPlan::TopN { input, .. }
            | PhysicalPlan::Limit { input, .. }
            | PhysicalPlan::Distinct { input } => vec![input],
            PhysicalPlan::NestedLoopJoin { left, right, .. }
            | PhysicalPlan::HashJoin { left, right, .. } => vec![left, right],
        }
    }

    fn describe(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PhysicalPlan::SeqScan { table, schema, .. } => {
                write!(f, "SeqScan: {table} {}", ColumnList(schema))
            }
            PhysicalPlan::IndexScan {
                table,
                range,
                schema,
                ..
            } => write!(
                f,
                "IndexScan: {table} key in {range} {}",
                ColumnList(schema)
            ),
            PhysicalPlan::Values { rows, schema } => {
                write!(f, "Values: {} rows {}", rows.len(), ColumnList(schema))
            }
            PhysicalPlan::Filter { predicate, .. } => write!(f, "Filter: {predicate}"),
            PhysicalPlan::Project { exprs, .. } => {
                f.write_str("Project: ")?;
                write_joined(f, exprs)
            }
            PhysicalPlan::NestedLoopJoin {
                join_type,
                predicate,
                ..
            } => {
                write!(f, "NestedLoopJoin: {join_type}")?;
                if let Some(p) = predicate {
                    write!(f, " ON {p}")?;
                }
                Ok(())
            }
            PhysicalPlan::HashJoin {
                join_type,
                left_keys,
                right_keys,
                residual,
                ..
            } => {
                write!(f, "HashJoin: {join_type} left=[")?;
                write_joined(f, left_keys)?;
                f.write_str("] right=[")?;
                write_joined(f, right_keys)?;
                f.write_str("]")?;
                if let Some(r) = residual {
                    write!(f, " residual={r}")?;
                }
                Ok(())
            }
            PhysicalPlan::HashAggregate {
                group_by,
                aggregates,
                ..
            } => {
                f.write_str("HashAggregate: group_by=[")?;
                write_joined(f, group_by)?;
                f.write_str("] aggregates=[")?;
                write_joined(f, aggregates)?;
                f.write_str("]")
            }
            PhysicalPlan::Sort { keys, .. } => {
                f.write_str("Sort: ")?;
                write_joined(f, keys)
            }
            PhysicalPlan::TopN {
                keys,
                limit,
                offset,
                ..
            } => {
                write!(f, "TopN: limit={limit} offset={offset} keys=")?;
                write_joined(f, keys)
            }
            PhysicalPlan::Limit { limit, offset, .. } => match limit {
                Some(l) => write!(f, "Limit: limit={l} offset={offset}"),
                None => write!(f, "Limit: limit=ALL offset={offset}"),
            },
            PhysicalPlan::Distinct { .. } => f.write_str("Distinct"),
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

impl fmt::Display for PhysicalPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_tree(f, 0)
    }
}

impl fmt::Display for PhysAggregate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let distinct = if self.distinct { "DISTINCT " } else { "" };
        match &self.arg {
            None => write!(f, "{}(*)", self.func.name()),
            Some(arg) => write!(f, "{}({distinct}{arg})", self.func.name()),
        }
    }
}

impl fmt::Display for PhysSortKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dir = if self.ascending { "ASC" } else { "DESC" };
        let nulls = if self.nulls_first { "FIRST" } else { "LAST" };
        write!(f, "{} {dir} NULLS {nulls}", self.expr)
    }
}
