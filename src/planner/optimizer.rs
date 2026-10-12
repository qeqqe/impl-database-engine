//! An optimizer rewrites a plan into an equivalent one that cat be more
//! efficient to execute
mod constant_folding;

use crate::planner::{
    error::PlanResult,
    logical::{LogicalPlan, Plan},
};

pub trait OptimizerRule {
    fn name(&self) -> &'static str;
    fn rewrite(&self, plan: LogicalPlan) -> PlanResult<LogicalPlan>;
}

struct Optimizer {
    rules: Vec<Box<dyn OptimizerRule>>,
}

impl Default for Optimizer {
    fn default() -> Self {
        Self::new(vec![])
    }
}

impl Optimizer {
    pub fn new(rules: Vec<Box<dyn OptimizerRule>>) -> Self {
        Self { rules }
    }

    pub fn rule_names(&self) -> Vec<&'static str> {
        self.rules.iter().map(|f| f.name()).collect()
    }

    pub fn optimize(&self, plan: LogicalPlan) -> PlanResult<LogicalPlan> {
        self.rules
            .iter()
            .try_fold(plan, |plan, rule| rule.rewrite(plan))
    }

    pub fn optimize_plan(&self, plan: Plan) -> PlanResult<Plan> {
        Ok(match plan {
            Plan::Query(logical_plan) => Plan::Query(self.optimize(logical_plan)?),
            Plan::Insert { table, input } => Plan::Insert {
                table,
                input: self.optimize(input)?,
            },
            Plan::Update {
                table,
                input,
                assignments,
            } => Plan::Update {
                table,
                input: self.optimize(input)?,

                assignments: assignments, // TODO: fold assignment's values expr
            },
            Plan::Delete { table, input } => Plan::Delete {
                table,
                input: self.optimize(input)?,
            },
            other => other,
        })
    }
}
