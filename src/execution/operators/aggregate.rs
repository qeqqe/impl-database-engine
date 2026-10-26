use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::planner::expr::AggregateFunction;
use crate::planner::physical::PhysAggregate;
use crate::planner::physical_expr::PhysExpr;
use crate::planner::schema::Schema;
use crate::planner::value::{EvalError, Row, Value};
use crate::sql::BinaryOperator;

use super::super::error::ExecResult;
use super::super::operator::{BoxedOperator, Operator, not_open};

enum State {
    Count(i64),
    Sum(Option<Value>),
    Avg { sum: f64, count: i64 },
    Min(Option<Value>),
    Max(Option<Value>),
}

struct Accumulator {
    state: State,
    seen: Option<HashSet<Value>>,
}

impl Accumulator {
    fn new(aggregate: &PhysAggregate) -> Self {
        let state = match aggregate.func {
            AggregateFunction::Count => State::Count(0),
            AggregateFunction::Sum => State::Sum(None),
            AggregateFunction::Avg => State::Avg { sum: 0.0, count: 0 },
            AggregateFunction::Min => State::Min(None),
            AggregateFunction::Max => State::Max(None),
        };
        Self {
            state,
            seen: aggregate.distinct.then(HashSet::new),
        }
    }

    fn update(&mut self, value: Option<Value>) -> ExecResult<()> {
        let Some(value) = value else {
            if let State::Count(n) = &mut self.state {
                *n += 1;
            }
            return Ok(());
        };
        if value.is_null() {
            return Ok(());
        }
        if let Some(seen) = &mut self.seen
            && !seen.insert(value.clone())
        {
            return Ok(());
        }
        match &mut self.state {
            State::Count(n) => *n += 1,
            State::Sum(acc) => {
                *acc = Some(match acc.take() {
                    None => value,
                    Some(current) => Value::binary(BinaryOperator::Plus, &current, &value)?,
                });
            }
            State::Avg { sum, count } => {
                *sum += match value {
                    Value::Integer(i) => i as f64,
                    Value::Float(x) => x,
                    other => {
                        return Err(EvalError::TypeMismatch(format!(
                            "AVG cannot be applied to {}",
                            other.data_type()
                        ))
                        .into());
                    }
                };
                *count += 1;
            }
            State::Min(acc) => replace_if(acc, value, Ordering::Less)?,
            State::Max(acc) => replace_if(acc, value, Ordering::Greater)?,
        }
        Ok(())
    }

    fn finish(self) -> Value {
        match self.state {
            State::Count(n) => Value::Integer(n),
            State::Sum(acc) | State::Min(acc) | State::Max(acc) => acc.unwrap_or(Value::Null),
            State::Avg { count: 0, .. } => Value::Null,
            State::Avg { sum, count } => Value::Float(sum / count as f64),
        }
    }
}

fn replace_if(acc: &mut Option<Value>, candidate: Value, wanted: Ordering) -> ExecResult<()> {
    let replace = match acc {
        None => true,
        Some(current) => candidate.sql_cmp(current)? == Some(wanted),
    };
    if replace {
        *acc = Some(candidate);
    }
    Ok(())
}

pub struct HashAggregateExec<'a> {
    input: BoxedOperator<'a>,
    group_by: Vec<PhysExpr>,
    aggregates: Vec<PhysAggregate>,
    schema: Schema,
    output: Option<std::vec::IntoIter<Row>>,
}

impl<'a> HashAggregateExec<'a> {
    pub fn new(
        input: BoxedOperator<'a>,
        group_by: Vec<PhysExpr>,
        aggregates: Vec<PhysAggregate>,
        schema: Schema,
    ) -> Self {
        Self {
            input,
            group_by,
            aggregates,
            schema,
            output: None,
        }
    }

    fn new_accumulators(&self) -> Vec<Accumulator> {
        self.aggregates.iter().map(Accumulator::new).collect()
    }

    fn consume(&mut self) -> ExecResult<Vec<Row>> {
        let mut index: HashMap<Vec<Value>, usize> = HashMap::new();
        let mut groups: Vec<(Vec<Value>, Vec<Accumulator>)> = Vec::new();

        while let Some(row) = self.input.next()? {
            let key = self
                .group_by
                .iter()
                .map(|e| e.eval(&row))
                .collect::<Result<Vec<_>, _>>()?;
            let slot = match index.get(&key) {
                Some(&slot) => slot,
                None => {
                    index.insert(key.clone(), groups.len());
                    groups.push((key, self.new_accumulators()));
                    groups.len() - 1
                }
            };
            for (acc, aggregate) in groups[slot].1.iter_mut().zip(&self.aggregates) {
                let value = aggregate.arg.as_ref().map(|a| a.eval(&row)).transpose()?;
                acc.update(value)?;
            }
        }

        if groups.is_empty() && self.group_by.is_empty() {
            groups.push((Vec::new(), self.new_accumulators()));
        }

        Ok(groups
            .into_iter()
            .map(|(mut key, accumulators)| {
                key.extend(accumulators.into_iter().map(Accumulator::finish));
                key
            })
            .collect())
    }
}

impl Operator for HashAggregateExec<'_> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self) -> ExecResult<()> {
        self.input.open()?;
        let rows = self.consume()?;
        self.output = Some(rows.into_iter());
        Ok(())
    }

    fn next(&mut self) -> ExecResult<Option<Row>> {
        let output = self
            .output
            .as_mut()
            .ok_or_else(|| not_open("HashAggregate"))?;
        Ok(output.next())
    }

    fn close(&mut self) -> ExecResult<()> {
        self.output = None;
        self.input.close()
    }
}
