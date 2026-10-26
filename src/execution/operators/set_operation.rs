use std::collections::{HashMap, HashSet};

use crate::planner::schema::Schema;
use crate::planner::value::Row;
use crate::sql::SetOperator;

use super::super::error::ExecResult;
use super::super::operator::{BoxedOperator, Operator, not_open};

enum State {
    Closed,
    Left,
    Right,
    Done,
}

pub struct SetOpExec<'a> {
    op: SetOperator,
    all: bool,
    left: BoxedOperator<'a>,
    right: BoxedOperator<'a>,
    schema: Schema,
    state: State,
    seen: HashSet<Row>,
    counts: HashMap<Row, usize>,
}

impl<'a> SetOpExec<'a> {
    pub fn new(
        op: SetOperator,
        all: bool,
        left: BoxedOperator<'a>,
        right: BoxedOperator<'a>,
        schema: Schema,
    ) -> Self {
        Self {
            op,
            all,
            left,
            right,
            schema,
            state: State::Closed,
            seen: HashSet::new(),
            counts: HashMap::new(),
        }
    }

    fn next_union(&mut self) -> ExecResult<Option<Row>> {
        loop {
            let row = match self.state {
                State::Left => match self.left.next()? {
                    Some(row) => row,
                    None => {
                        self.state = State::Right;
                        continue;
                    }
                },
                State::Right => match self.right.next()? {
                    Some(row) => row,
                    None => {
                        self.state = State::Done;
                        return Ok(None);
                    }
                },
                State::Done => return Ok(None),
                State::Closed => return Err(not_open("SetOpExec")),
            };
            if self.all || self.seen.insert(row.clone()) {
                return Ok(Some(row));
            }
        }
    }

    fn keep(&mut self, row: &Row) -> bool {
        match (self.op, self.all) {
            (SetOperator::Intersect, true) => match self.counts.get_mut(row) {
                Some(count) if *count > 0 => {
                    *count -= 1;
                    true
                }
                _ => false,
            },
            (SetOperator::Intersect, false) => match self.counts.get_mut(row) {
                Some(count) if *count > 0 => {
                    *count = 0;
                    true
                }
                _ => false,
            },
            (SetOperator::Except, true) => match self.counts.get_mut(row) {
                Some(count) if *count > 0 => {
                    *count -= 1;
                    false
                }
                _ => true,
            },
            (SetOperator::Except, false) => {
                if self.counts.contains_key(row) {
                    false
                } else {
                    self.counts.insert(row.clone(), 0);
                    true
                }
            }
            (SetOperator::Union, _) => true,
        }
    }
}

impl Operator for SetOpExec<'_> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self) -> ExecResult<()> {
        self.seen.clear();
        self.counts.clear();
        self.left.open()?;
        self.right.open()?;
        if self.op != SetOperator::Union {
            while let Some(row) = self.right.next()? {
                *self.counts.entry(row).or_insert(0) += 1;
            }
        }
        self.state = State::Left;
        Ok(())
    }

    fn next(&mut self) -> ExecResult<Option<Row>> {
        if self.op == SetOperator::Union {
            return self.next_union();
        }
        match self.state {
            State::Closed => return Err(not_open("SetOpExec")),
            State::Right | State::Done => return Ok(None),
            State::Left => {}
        }
        while let Some(row) = self.left.next()? {
            if self.keep(&row) {
                return Ok(Some(row));
            }
        }
        self.state = State::Done;
        Ok(None)
    }

    fn close(&mut self) -> ExecResult<()> {
        self.state = State::Closed;
        self.seen.clear();
        self.counts.clear();
        let left = self.left.close();
        let right = self.right.close();
        left.and(right)
    }
}
