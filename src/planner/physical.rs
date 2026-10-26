use core::fmt;
use std::ops::Bound;

use crate::{
    planner::{
        catalog::Catalog,
        error::PlanResult,
        expr::{AggregateFunction, ScalarExpr, SortKey},
        logical::{ColumnList, LogicalPlan, write_joined},
        physical_expr::PhysExpr,
        schema::{ColumnId, Schema},
        types::DataType,
        value::Value,
    },
    sql::{BinaryOperator, JoinType, SetOperator},
};

// tis stores the node's id range
#[derive(Clone, PartialEq, Debug, Eq)]
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
pub struct ValueRange {
    pub lower: Bound<Value>,
    pub upper: Bound<Value>,
}

impl ValueRange {
    pub fn point(value: Value) -> Self {
        Self {
            lower: Bound::Included(value.clone()),
            upper: Bound::Included(value),
        }
    }

    pub fn contains(&self, value: &Value) -> bool {
        if value.is_null() {
            return false;
        }
        let above_lower = match &self.lower {
            Bound::Included(l) => value >= l,
            Bound::Excluded(l) => value > l,
            Bound::Unbounded => true,
        };
        let below_upper = match &self.upper {
            Bound::Included(u) => value <= u,
            Bound::Excluded(u) => value < u,
            Bound::Unbounded => true,
        };
        above_lower && below_upper
    }

    pub fn is_empty(&self) -> bool {
        match (&self.lower, &self.upper) {
            (Bound::Included(l), Bound::Included(u)) => l > u,
            (Bound::Included(l) | Bound::Excluded(l), Bound::Included(u) | Bound::Excluded(u)) => {
                l >= u
            }
            _ => false,
        }
    }

    fn intersect(self, other: ValueRange) -> ValueRange {
        ValueRange {
            lower: tighter_value_bound(self.lower, other.lower, std::cmp::Ordering::Greater),
            upper: tighter_value_bound(self.upper, other.upper, std::cmp::Ordering::Less),
        }
    }
}

fn tighter_value_bound(
    a: Bound<Value>,
    b: Bound<Value>,
    tighter: std::cmp::Ordering,
) -> Bound<Value> {
    match (a, b) {
        (Bound::Unbounded, x) | (x, Bound::Unbounded) => x,
        (a, b) => {
            let (Bound::Included(x) | Bound::Excluded(x)) = &a else {
                unreachable!()
            };
            let (Bound::Included(y) | Bound::Excluded(y)) = &b else {
                unreachable!()
            };
            match x.cmp(y) {
                std::cmp::Ordering::Equal if matches!(a, Bound::Excluded(_)) => a,
                std::cmp::Ordering::Equal => b,
                order if order == tighter => a,
                _ => b,
            }
        }
    }
}

impl fmt::Display for ValueRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.lower, &self.upper) {
            (Bound::Included(l), Bound::Included(u)) if l == u => {
                return write!(f, "{}", Quoted(l));
            }
            _ => {}
        }
        match &self.lower {
            Bound::Included(l) => write!(f, "[{}", Quoted(l))?,
            Bound::Excluded(l) => write!(f, "({}", Quoted(l))?,
            Bound::Unbounded => f.write_str("(-inf")?,
        }
        f.write_str(", ")?;
        match &self.upper {
            Bound::Included(u) => write!(f, "{}]", Quoted(u)),
            Bound::Excluded(u) => write!(f, "{})", Quoted(u)),
            Bound::Unbounded => f.write_str("+inf)"),
        }
    }
}

struct Quoted<'a>(&'a Value);

impl fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Value::Text(s) => write!(f, "'{}'", s.replace('\'', "''")),
            other => write!(f, "{other}"),
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
    IndexLookup {
        table: String,
        index: String,
        column: usize,
        column_type: DataType,
        ranges: Vec<ValueRange>,
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
    SetOperation {
        op: SetOperator,
        all: bool,
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        schema: Schema,
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
            | PhysicalPlan::IndexLookup { schema, .. }
            | PhysicalPlan::SetOperation { schema, .. }
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
            | PhysicalPlan::IndexLookup { .. }
            | PhysicalPlan::Values { .. } => vec![],
            PhysicalPlan::Filter { input, .. }
            | PhysicalPlan::Project { input, .. }
            | PhysicalPlan::HashAggregate { input, .. }
            | PhysicalPlan::Sort { input, .. }
            | PhysicalPlan::TopN { input, .. }
            | PhysicalPlan::Limit { input, .. }
            | PhysicalPlan::Distinct { input } => vec![input],
            PhysicalPlan::NestedLoopJoin { left, right, .. }
            | PhysicalPlan::SetOperation { left, right, .. }
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
            PhysicalPlan::IndexLookup {
                table,
                index,
                ranges,
                schema,
                ..
            } => {
                write!(f, "IndexLookup: {table} via {index} in ")?;
                if ranges.is_empty() {
                    f.write_str("nothing")?;
                }
                write_joined(f, ranges)?;
                write!(f, " {}", ColumnList(schema))
            }
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
            PhysicalPlan::SetOperation {
                op: SetOperator::Union,
                all: true,
                ..
            } => f.write_str("Append"),
            PhysicalPlan::SetOperation { op, all, .. } => {
                write!(f, "HashSetOp: {op}")?;
                if *all {
                    f.write_str(" ALL")?;
                }
                Ok(())
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

pub struct PhysicalPlanner<'a> {
    catalog: &'a Catalog,
}

impl<'a> PhysicalPlanner<'a> {
    pub fn new(catalog: &'a Catalog) -> Self {
        Self { catalog }
    }

    pub fn plan(&self, plan: &LogicalPlan) -> PlanResult<PhysicalPlan> {
        match plan {
            LogicalPlan::Scan {
                table,
                projection,
                schema,
                ..
            } => Ok(PhysicalPlan::SeqScan {
                table: table.clone(),
                projection: projection.clone(),
                schema: schema.clone(),
            }),
            LogicalPlan::Values { rows, schema } => Ok(PhysicalPlan::Values {
                rows: rows
                    .iter()
                    .map(|row| lower_all(row, &Schema::empty()))
                    .collect::<PlanResult<_>>()?,
                schema: schema.clone(),
            }),
            LogicalPlan::Filter { input, predicate } => self.plan_filter(input, predicate),
            LogicalPlan::Project {
                input,
                exprs,
                schema,
            } => {
                let input = self.plan(input)?;
                Ok(PhysicalPlan::Project {
                    exprs: lower_all(exprs, input.schema())?,
                    input: Box::new(input),
                    schema: schema.clone(),
                })
            }
            LogicalPlan::Join {
                left,
                right,
                join_type,
                on,
                schema,
            } => self.plan_join(left, right, *join_type, on.as_ref(), schema),
            LogicalPlan::Aggregate {
                input,
                group_by,
                aggregates,
                schema,
            } => {
                let input = self.plan(input)?;
                let group_by = lower_all(group_by, input.schema())?;
                let aggregates = aggregates
                    .iter()
                    .map(|a| {
                        Ok(PhysAggregate {
                            func: a.func,
                            arg: a
                                .arg
                                .as_ref()
                                .map(|e| PhysExpr::lower(e, input.schema()))
                                .transpose()?,
                            distinct: a.distinct,
                        })
                    })
                    .collect::<PlanResult<_>>()?;
                Ok(PhysicalPlan::HashAggregate {
                    input: Box::new(input),
                    group_by,
                    aggregates,
                    schema: schema.clone(),
                })
            }
            LogicalPlan::Distinct { input } => Ok(PhysicalPlan::Distinct {
                input: Box::new(self.plan(input)?),
            }),
            LogicalPlan::SetOperation {
                op,
                all,
                left,
                right,
                schema,
            } => Ok(PhysicalPlan::SetOperation {
                op: *op,
                all: *all,
                left: Box::new(self.plan(left)?),
                right: Box::new(self.plan(right)?),
                schema: schema.clone(),
            }),
            LogicalPlan::Sort { input, keys } => {
                let input = self.plan(input)?;
                Ok(PhysicalPlan::Sort {
                    keys: lower_sort_keys(keys, input.schema())?,
                    input: Box::new(input),
                })
            }
            LogicalPlan::Limit {
                input,
                limit,
                offset,
            } => match (input.as_ref(), limit) {
                (LogicalPlan::Sort { input, keys }, Some(limit)) => {
                    let input = self.plan(input)?;
                    Ok(PhysicalPlan::TopN {
                        keys: lower_sort_keys(keys, input.schema())?,
                        input: Box::new(input),
                        limit: *limit,
                        offset: *offset,
                    })
                }
                _ => Ok(PhysicalPlan::Limit {
                    input: Box::new(self.plan(input)?),
                    limit: *limit,
                    offset: *offset,
                }),
            },
        }
    }

    fn plan_filter(&self, input: &LogicalPlan, predicate: &ScalarExpr) -> PlanResult<PhysicalPlan> {
        if let LogicalPlan::Scan {
            table,
            projection,
            schema,
            ..
        } = input
            && let Some(access) = self.choose_access(table, projection, schema, predicate)?
        {
            return Ok(access);
        }
        let input = self.plan(input)?;
        Ok(PhysicalPlan::Filter {
            predicate: PhysExpr::lower(predicate, input.schema())?,
            input: Box::new(input),
        })
    }

    fn choose_access(
        &self,
        table: &str,
        projection: &[usize],
        schema: &Schema,
        predicate: &ScalarExpr,
    ) -> PlanResult<Option<PhysicalPlan>> {
        let key_scan = self.try_index_scan(table, projection, schema, predicate)?;
        let key_is_point = key_scan.as_ref().and_then(key_scan_range).is_some_and(|range| {
            matches!((range.lower, range.upper), (Bound::Included(l), Bound::Included(u)) if l == u)
        });
        if key_is_point {
            return Ok(key_scan);
        }
        let lookup = self.try_index_lookup(table, projection, schema, predicate)?;
        Ok(match (key_scan, lookup) {
            (_, Some((plan, true))) => Some(plan),
            (Some(scan), _) => Some(scan),
            (None, lookup) => lookup.map(|(plan, _)| plan),
        })
    }

    fn try_index_lookup(
        &self,
        table: &str,
        projection: &[usize],
        schema: &Schema,
        predicate: &ScalarExpr,
    ) -> PlanResult<Option<(PhysicalPlan, bool)>> {
        let table_schema = self.catalog.get_table(table)?;
        let conjuncts = predicate.clone().split_conjunction();
        let mut best: Option<(&str, usize, DataType, Vec<ValueRange>, bool)> = None;
        for index in self.catalog.indexes_on(table) {
            let column = index.columns[0];
            let Some(position) = projection.iter().position(|&c| c == column) else {
                continue;
            };
            let column_id = schema.field(position).id;
            let column_type = table_schema.column_type(column);
            let mut points: Option<Vec<Value>> = None;
            let mut range: Option<ValueRange> = None;
            for conjunct in &conjuncts {
                match index_bound(conjunct, column_id, column_type) {
                    Some(IndexBound::Points(values)) if points.is_none() => points = Some(values),
                    Some(IndexBound::Range(r)) => {
                        range = Some(match range {
                            Some(prev) => prev.intersect(r),
                            None => r,
                        })
                    }
                    _ => {}
                }
            }
            let candidate = match (points, range) {
                (Some(mut values), _) => {
                    values.sort();
                    values.dedup();
                    (values.into_iter().map(ValueRange::point).collect(), true)
                }
                (None, Some(range)) if range.is_empty() => (Vec::new(), true),
                (None, Some(range)) => (vec![range], false),
                (None, None) => continue,
            };
            let better = match &best {
                None => true,
                Some((_, _, _, _, best_exact)) => candidate.1 && !best_exact,
            };
            if better {
                best = Some((&index.name, column, column_type, candidate.0, candidate.1));
            }
        }
        let Some((index, column, column_type, ranges, exact)) = best else {
            return Ok(None);
        };
        let lookup = PhysicalPlan::IndexLookup {
            table: table.to_string(),
            index: index.to_string(),
            column,
            column_type,
            ranges,
            projection: projection.to_vec(),
            schema: schema.clone(),
        };
        Ok(Some((
            PhysicalPlan::Filter {
                predicate: PhysExpr::lower(predicate, schema)?,
                input: Box::new(lookup),
            },
            exact,
        )))
    }

    fn try_index_scan(
        &self,
        table: &str,
        projection: &[usize],
        schema: &Schema,
        predicate: &ScalarExpr,
    ) -> PlanResult<Option<PhysicalPlan>> {
        let Some(key_column) = self.catalog.get_table(table)?.integer_key_column() else {
            return Ok(None);
        };
        let Some(key_position) = projection.iter().position(|&c| c == key_column) else {
            return Ok(None);
        };
        let key_id = schema.field(key_position).id;

        let mut range = KeyRange::full();
        let mut used_any = false;
        let mut residual = Vec::new();
        for conjunct in predicate.clone().split_conjunction() {
            match key_bound(&conjunct, key_id) {
                Some(bound) => {
                    range = range.intersect(&bound);
                    used_any = true;
                }
                None => residual.push(conjunct),
            }
        }
        if !used_any {
            return Ok(None);
        }

        let scan = PhysicalPlan::IndexScan {
            table: table.to_string(),
            key_column,
            range,
            projection: projection.to_vec(),
            schema: schema.clone(),
        };
        Ok(Some(match ScalarExpr::conjunction(residual) {
            Some(rest) => PhysicalPlan::Filter {
                predicate: PhysExpr::lower(&rest, schema)?,
                input: Box::new(scan),
            },
            None => scan,
        }))
    }

    fn plan_join(
        &self,
        left: &LogicalPlan,
        right: &LogicalPlan,
        join_type: JoinType,
        on: Option<&ScalarExpr>,
        schema: &Schema,
    ) -> PlanResult<PhysicalPlan> {
        let left_plan = self.plan(left)?;
        let right_plan = self.plan(right)?;
        let combined = left_plan.schema().concat(right_plan.schema());

        let mut left_keys = Vec::new();
        let mut right_keys = Vec::new();
        let mut residual = Vec::new();
        if join_type != JoinType::Cross {
            for conjunct in on
                .cloned()
                .map(ScalarExpr::split_conjunction)
                .unwrap_or_default()
            {
                match equi_join_keys(&conjunct, left.schema(), right.schema()) {
                    Some((l, r, lossy)) => {
                        left_keys.push(PhysExpr::lower(&l, left_plan.schema())?);
                        right_keys.push(PhysExpr::lower(&r, right_plan.schema())?);
                        if lossy {
                            residual.push(conjunct);
                        }
                    }
                    None => residual.push(conjunct),
                }
            }
        }

        if left_keys.is_empty() {
            return Ok(PhysicalPlan::NestedLoopJoin {
                predicate: on.map(|p| PhysExpr::lower(p, &combined)).transpose()?,
                left: Box::new(left_plan),
                right: Box::new(right_plan),
                join_type,
                schema: schema.clone(),
            });
        }

        Ok(PhysicalPlan::HashJoin {
            residual: ScalarExpr::conjunction(residual)
                .map(|r| PhysExpr::lower(&r, &combined))
                .transpose()?,
            left: Box::new(left_plan),
            right: Box::new(right_plan),
            join_type,
            left_keys,
            right_keys,
            schema: schema.clone(),
        })
    }
}

fn lower_all(exprs: &[ScalarExpr], schema: &Schema) -> PlanResult<Vec<PhysExpr>> {
    exprs.iter().map(|e| PhysExpr::lower(e, schema)).collect()
}

fn lower_sort_keys(keys: &[SortKey], schema: &Schema) -> PlanResult<Vec<PhysSortKey>> {
    keys.iter()
        .map(|k| {
            Ok(PhysSortKey {
                expr: PhysExpr::lower(&k.expr, schema)?,
                ascending: k.ascending,
                nulls_first: k.nulls_first,
            })
        })
        .collect()
}

fn key_bound(conjunct: &ScalarExpr, key_id: ColumnId) -> Option<KeyRange> {
    let ScalarExpr::Binary { op, left, right } = conjunct else {
        return None;
    };
    let (op, value) = match (left.as_ref(), right.as_ref()) {
        (ScalarExpr::Column(c), ScalarExpr::Literal(Value::Integer(v))) if c.id == key_id => {
            (*op, *v)
        }
        (ScalarExpr::Literal(Value::Integer(v)), ScalarExpr::Column(c)) if c.id == key_id => {
            (op.flip()?, *v)
        }
        _ => return None,
    };
    let (lower, upper) = match op {
        BinaryOperator::Eq => (Bound::Included(value), Bound::Included(value)),
        BinaryOperator::Lt => (Bound::Unbounded, Bound::Excluded(value)),
        BinaryOperator::LtEq => (Bound::Unbounded, Bound::Included(value)),
        BinaryOperator::Gt => (Bound::Excluded(value), Bound::Unbounded),
        BinaryOperator::GtEq => (Bound::Included(value), Bound::Unbounded),
        _ => return None,
    };
    Some(KeyRange { lower, upper })
}

fn key_scan_range(plan: &PhysicalPlan) -> Option<&KeyRange> {
    match plan {
        PhysicalPlan::IndexScan { range, .. } => Some(range),
        PhysicalPlan::Filter { input, .. } => key_scan_range(input),
        _ => None,
    }
}

enum IndexBound {
    Points(Vec<Value>),
    Range(ValueRange),
}

fn index_value(column_type: DataType, value: &Value) -> Option<Value> {
    match (column_type, value) {
        (DataType::Integer, Value::Integer(_))
        | (DataType::Float, Value::Float(_))
        | (DataType::Boolean, Value::Boolean(_)) => Some(value.clone()),
        (DataType::Float, Value::Integer(i)) => {
            let float = *i as f64;
            (float as i128 == *i as i128).then_some(Value::Float(float))
        }
        (t, Value::Text(_)) if t.is_textual() => Some(value.clone()),
        _ => None,
    }
}

fn index_bound(
    conjunct: &ScalarExpr,
    column: ColumnId,
    column_type: DataType,
) -> Option<IndexBound> {
    match conjunct {
        ScalarExpr::Binary { op, left, right } => {
            let (op, value) = match (left.as_ref(), right.as_ref()) {
                (ScalarExpr::Column(c), ScalarExpr::Literal(v)) if c.id == column => (*op, v),
                (ScalarExpr::Literal(v), ScalarExpr::Column(c)) if c.id == column => {
                    (op.flip()?, v)
                }
                _ => return None,
            };
            let value = index_value(column_type, value)?;
            let range = |lower, upper| Some(IndexBound::Range(ValueRange { lower, upper }));
            match op {
                BinaryOperator::Eq => Some(IndexBound::Points(vec![value])),
                BinaryOperator::Lt => range(Bound::Unbounded, Bound::Excluded(value)),
                BinaryOperator::LtEq => range(Bound::Unbounded, Bound::Included(value)),
                BinaryOperator::Gt => range(Bound::Excluded(value), Bound::Unbounded),
                BinaryOperator::GtEq => range(Bound::Included(value), Bound::Unbounded),
                _ => None,
            }
        }
        ScalarExpr::InList {
            expr,
            list,
            negated: false,
        } if matches!(expr.as_ref(), ScalarExpr::Column(c) if c.id == column) => {
            let mut values = Vec::with_capacity(list.len());
            for item in list {
                match item {
                    ScalarExpr::Literal(Value::Null) => {}
                    ScalarExpr::Literal(v) => values.push(index_value(column_type, v)?),
                    _ => return None,
                }
            }
            Some(IndexBound::Points(values))
        }
        _ => None,
    }
}

fn equi_join_keys(
    conjunct: &ScalarExpr,
    left: &Schema,
    right: &Schema,
) -> Option<(ScalarExpr, ScalarExpr, bool)> {
    let ScalarExpr::Binary {
        op: BinaryOperator::Eq,
        left: a,
        right: b,
    } = conjunct
    else {
        return None;
    };
    let references_only = |e: &ScalarExpr, s: &Schema| !e.is_constant() && e.is_bound_by(s);
    let (l, r) = if references_only(a, left) && references_only(b, right) {
        (a.as_ref().clone(), b.as_ref().clone())
    } else if references_only(b, left) && references_only(a, right) {
        (b.as_ref().clone(), a.as_ref().clone())
    } else {
        return None;
    };
    let (left_type, right_type) = (l.data_type(), r.data_type());
    let key_type = hash_key_type(left_type, right_type)?;
    let lossy = left_type != right_type && key_type == DataType::Float;
    Some((l.cast(key_type), r.cast(key_type), lossy))
}

fn hash_key_type(left: DataType, right: DataType) -> Option<DataType> {
    match (left, right) {
        (a, b) if a == b && !a.is_null() => Some(a),
        (a, b) if a.is_numeric() && b.is_numeric() => Some(DataType::Float),
        (a, b) if a.is_textual() && b.is_textual() => Some(DataType::Text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_range_intersection() {
        let a = KeyRange {
            lower: Bound::Included(5),
            upper: Bound::Unbounded,
        };
        let b = KeyRange {
            lower: Bound::Excluded(5),
            upper: Bound::Excluded(10),
        };
        let r = a.intersect(&b);
        assert_eq!(r.lower, Bound::Excluded(5));
        assert_eq!(r.upper, Bound::Excluded(10));
        assert!(r.contains(6));
        assert!(!r.contains(5));
        assert!(!r.contains(10));
    }

    #[test]
    fn key_range_emptiness() {
        assert!(KeyRange::point(3).intersect(&KeyRange::point(4)).is_empty());
        assert!(!KeyRange::point(3).is_empty());
        let open = KeyRange {
            lower: Bound::Excluded(1),
            upper: Bound::Excluded(2),
        };
        assert!(open.is_empty());
    }
}
