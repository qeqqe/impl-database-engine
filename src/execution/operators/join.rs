use std::collections::{HashMap, VecDeque};

use crate::planner::physical_expr::PhysExpr;
use crate::planner::schema::Schema;
use crate::planner::value::{Row, Value};
use crate::sql::JoinType;

use super::super::error::ExecResult;
use super::super::operator::{BoxedOperator, Operator, concat_rows, not_open, null_row};

fn preserves_left(join_type: JoinType) -> bool {
    matches!(join_type, JoinType::Left | JoinType::Full)
}

fn preserves_right(join_type: JoinType) -> bool {
    matches!(join_type, JoinType::Right | JoinType::Full)
}

struct BuildSide {
    rows: Vec<Row>,
    matched: Vec<bool>,
}

impl BuildSide {
    fn drain(input: &mut BoxedOperator<'_>) -> ExecResult<Self> {
        let mut rows = Vec::new();
        while let Some(row) = input.next()? {
            rows.push(row);
        }
        let matched = vec![false; rows.len()];
        Ok(Self { rows, matched })
    }

    fn unmatched_padded(&self, left_width: usize) -> impl Iterator<Item = Row> + '_ {
        self.rows
            .iter()
            .zip(&self.matched)
            .filter(|(_, matched)| !**matched)
            .map(move |(row, _)| concat_rows(&null_row(left_width), row))
    }
}

struct JoinDriver<'a> {
    left: BoxedOperator<'a>,
    right: BoxedOperator<'a>,
    join_type: JoinType,
    schema: Schema,
    pending: VecDeque<Row>,
    left_exhausted: bool,
}

impl<'a> JoinDriver<'a> {
    fn new(
        left: BoxedOperator<'a>,
        right: BoxedOperator<'a>,
        join_type: JoinType,
        schema: Schema,
    ) -> Self {
        Self {
            left,
            right,
            join_type,
            schema,
            pending: VecDeque::new(),
            left_exhausted: false,
        }
    }

    fn left_width(&self) -> usize {
        self.left.schema().len()
    }

    fn right_width(&self) -> usize {
        self.right.schema().len()
    }

    fn open_children(&mut self) -> ExecResult<BuildSide> {
        self.pending.clear();
        self.left_exhausted = false;
        self.left.open()?;
        self.right.open()?;
        BuildSide::drain(&mut self.right)
    }

    fn close_children(&mut self) -> ExecResult<()> {
        self.pending.clear();
        let left = self.left.close();
        let right = self.right.close();
        left.and(right)
    }

    fn next_with<F>(&mut self, build: &mut BuildSide, mut probe: F) -> ExecResult<Option<Row>>
    where
        F: FnMut(&Row, &mut BuildSide, &mut VecDeque<Row>) -> ExecResult<bool>,
    {
        loop {
            if let Some(row) = self.pending.pop_front() {
                return Ok(Some(row));
            }
            if self.left_exhausted {
                return Ok(None);
            }
            match self.left.next()? {
                Some(left_row) => {
                    let matched = probe(&left_row, build, &mut self.pending)?;
                    if !matched && preserves_left(self.join_type) {
                        self.pending
                            .push_back(concat_rows(&left_row, &null_row(self.right_width())));
                    }
                }
                None => {
                    self.left_exhausted = true;
                    if preserves_right(self.join_type) {
                        let width = self.left_width();
                        self.pending.extend(build.unmatched_padded(width));
                    }
                }
            }
        }
    }
}

pub struct NestedLoopJoinExec<'a> {
    driver: JoinDriver<'a>,
    predicate: Option<PhysExpr>,
    build: Option<BuildSide>,
}

impl<'a> NestedLoopJoinExec<'a> {
    pub fn new(
        left: BoxedOperator<'a>,
        right: BoxedOperator<'a>,
        join_type: JoinType,
        predicate: Option<PhysExpr>,
        schema: Schema,
    ) -> Self {
        Self {
            driver: JoinDriver::new(left, right, join_type, schema),
            predicate,
            build: None,
        }
    }
}

impl Operator for NestedLoopJoinExec<'_> {
    fn schema(&self) -> &Schema {
        &self.driver.schema
    }

    fn open(&mut self) -> ExecResult<()> {
        self.build = Some(self.driver.open_children()?);
        Ok(())
    }

    fn next(&mut self) -> ExecResult<Option<Row>> {
        let build = self
            .build
            .as_mut()
            .ok_or_else(|| not_open("NestedLoopJoin"))?;
        let predicate = &self.predicate;
        self.driver.next_with(build, |left_row, build, out| {
            let mut matched = false;
            for (i, right_row) in build.rows.iter().enumerate() {
                let row = concat_rows(left_row, right_row);
                let keep = match predicate {
                    Some(p) => p.eval_predicate(&row)?,
                    None => true,
                };
                if keep {
                    matched = true;
                    build.matched[i] = true;
                    out.push_back(row);
                }
            }
            Ok(matched)
        })
    }

    fn close(&mut self) -> ExecResult<()> {
        self.build = None;
        self.driver.close_children()
    }
}

struct HashTable {
    side: BuildSide,
    buckets: HashMap<Vec<Value>, Vec<usize>>,
}

pub struct HashJoinExec<'a> {
    driver: JoinDriver<'a>,
    left_keys: Vec<PhysExpr>,
    right_keys: Vec<PhysExpr>,
    residual: Option<PhysExpr>,
    table: Option<HashTable>,
}

impl<'a> HashJoinExec<'a> {
    pub fn new(
        left: BoxedOperator<'a>,
        right: BoxedOperator<'a>,
        join_type: JoinType,
        left_keys: Vec<PhysExpr>,
        right_keys: Vec<PhysExpr>,
        residual: Option<PhysExpr>,
        schema: Schema,
    ) -> Self {
        Self {
            driver: JoinDriver::new(left, right, join_type, schema),
            left_keys,
            right_keys,
            residual,
            table: None,
        }
    }
}

fn eval_key(exprs: &[PhysExpr], row: &[Value]) -> ExecResult<Option<Vec<Value>>> {
    let key = exprs
        .iter()
        .map(|e| e.eval(row))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(if key.iter().any(Value::is_null) {
        None
    } else {
        Some(key)
    })
}

impl Operator for HashJoinExec<'_> {
    fn schema(&self) -> &Schema {
        &self.driver.schema
    }

    fn open(&mut self) -> ExecResult<()> {
        let side = self.driver.open_children()?;
        let mut buckets: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
        for (i, row) in side.rows.iter().enumerate() {
            if let Some(key) = eval_key(&self.right_keys, row)? {
                buckets.entry(key).or_default().push(i);
            }
        }
        self.table = Some(HashTable { side, buckets });
        Ok(())
    }

    fn next(&mut self) -> ExecResult<Option<Row>> {
        let HashTable { side, buckets } =
            self.table.as_mut().ok_or_else(|| not_open("HashJoin"))?;
        let left_keys = &self.left_keys;
        let residual = &self.residual;
        self.driver.next_with(side, |left_row, build, out| {
            let Some(key) = eval_key(left_keys, left_row)? else {
                return Ok(false);
            };
            let mut matched = false;
            for &i in buckets.get(&key).map(Vec::as_slice).unwrap_or_default() {
                let row = concat_rows(left_row, &build.rows[i]);
                let keep = match residual {
                    Some(p) => p.eval_predicate(&row)?,
                    None => true,
                };
                if keep {
                    matched = true;
                    build.matched[i] = true;
                    out.push_back(row);
                }
            }
            Ok(matched)
        })
    }

    fn close(&mut self) -> ExecResult<()> {
        self.table = None;
        self.driver.close_children()
    }
}
