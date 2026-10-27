use std::{
    cell::Cell,
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use crate::{
    execution::{
        builder::build_operator_with,
        collect,
        error::{ExecError, ExecResult},
        operator::{BoxedOperator, Operator},
        source::TableSource,
    },
    planner::{physical::PhysicalPlan, schema::Schema, value::Row},
};

#[derive(Default)]
pub struct OperatorStats {
    rows: Cell<u64>,
    opens: Cell<u64>,
    elapsed: Cell<Duration>,
}

impl OperatorStats {
    pub fn rows(&self) -> u64 {
        self.rows.get()
    }

    pub fn opens(&self) -> u64 {
        self.opens.get()
    }

    pub fn elapsed(&self) -> Duration {
        self.elapsed.get()
    }

    fn time<T>(&self, work: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let out = work();
        self.elapsed.set(self.elapsed.get() + start.elapsed());
        out
    }
}

struct CountedExec<'a> {
    inner: BoxedOperator<'a>,
    stats: Rc<OperatorStats>,
}

impl Operator for CountedExec<'_> {
    fn schema(&self) -> &Schema {
        self.inner.schema()
    }

    fn open(&mut self) -> ExecResult<()> {
        self.stats.opens.set(self.stats.opens.get() + 1);
        let inner = &mut self.inner;
        self.stats.time(|| inner.open())
    }

    fn next(&mut self) -> ExecResult<Option<Row>> {
        let inner = &mut self.inner;
        let row = self.stats.time(|| inner.next())?;
        if row.is_some() {
            self.stats.rows.set(self.stats.rows.get() + 1);
        }
        Ok(row)
    }

    fn close(&mut self) -> ExecResult<()> {
        let inner = &mut self.inner;
        self.stats.time(|| inner.close())
    }
}

pub struct Analyzed {
    pub rows: Vec<Row>,
    pub stats: Vec<Rc<OperatorStats>>,
    pub elapsed: Duration,
}

pub fn analyze_query(plan: &PhysicalPlan, source: &dyn TableSource) -> ExecResult<Analyzed> {
    let mut by_node: HashMap<*const PhysicalPlan, Rc<OperatorStats>> = HashMap::new();
    let mut root = build_operator_with(plan, source, &mut |node, inner| {
        let stats = Rc::new(OperatorStats::default());
        by_node.insert(node as *const PhysicalPlan, Rc::clone(&stats));
        Box::new(CountedExec { inner, stats })
    });
    let start = Instant::now();
    let rows = collect(root.as_mut())?;
    let elapsed = start.elapsed();
    drop(root);

    let mut stats = Vec::with_capacity(by_node.len());
    let mut pending = vec![plan];
    while let Some(node) = pending.pop() {
        let node_stats = by_node
            .remove(&(node as *const PhysicalPlan))
            .ok_or_else(|| {
                ExecError::Protocol("EXPLAIN ANALYZE lost an operator's counters".into())
            })?;
        stats.push(node_stats);
        pending.extend(node.children().into_iter().rev());
    }
    Ok(Analyzed {
        rows,
        stats,
        elapsed,
    })
}

pub fn render_analyzed(plan: &PhysicalPlan, analyzed: &Analyzed, timing: bool) -> Vec<String> {
    let text = plan.to_string();
    let mut lines: Vec<String> = text
        .lines()
        .zip(&analyzed.stats)
        .map(|(line, stats)| {
            let mut out = format!("{line} (rows={}", stats.rows());
            if stats.opens() != 1 {
                out.push_str(&format!(", opens={}", stats.opens()));
            }
            if timing {
                out.push_str(&format!(", time={:.3}ms", millis(stats.elapsed())));
            }
            out.push(')');
            out
        })
        .collect();
    let mut total = format!("Result: {} rows", analyzed.rows.len());
    if timing {
        total.push_str(&format!(" in {:.3}ms", millis(analyzed.elapsed)));
    }
    lines.push(total);
    lines
}

fn millis(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
