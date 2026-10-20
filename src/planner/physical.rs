use core::fmt;
use std::ops::Bound;

use crate::{
    planner::{expr::AggregateFunction, physical_expr::PhysExpr, schema::Schema},
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
            Bound::Included(l) => key > l,
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
