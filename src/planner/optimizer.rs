//! An optimizer rewrites a plan into an equivalent one that cat be more
//! efficient to execute

#![allow(dead_code)]
mod constant_folding;
mod predicate_pushdown;

use crate::planner::{
    error::PlanResult,
    logical::{LogicalPlan, Plan, UpdateAssignment},
    optimizer::{
        constant_folding::{ConstantFolding, fold_expr},
        predicate_pushdown::PredicatePushdown,
    },
};

pub trait OptimizerRule {
    fn name(&self) -> &'static str;
    fn rewrite(&self, plan: LogicalPlan) -> PlanResult<LogicalPlan>;
}

pub struct Optimizer {
    rules: Vec<Box<dyn OptimizerRule>>,
}

impl Default for Optimizer {
    fn default() -> Self {
        Self::new(vec![Box::new(ConstantFolding), Box::new(PredicatePushdown)])
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
                assignments: assignments
                    .into_iter()
                    .map(|a| {
                        Ok(UpdateAssignment {
                            column: a.column,
                            value: fold_expr(a.value)?,
                        })
                    })
                    .collect::<PlanResult<Vec<_>>>()?,
            },
            Plan::Delete { table, input } => Plan::Delete {
                table,
                input: self.optimize(input)?,
            },
            other => other,
        })
    }
}

#[cfg(test)]
mod test {
    use crate::planner::{expr::ScalarExpr, plan_sql, test_catalog::catalog};

    use super::*;

    // fn optimized(sql: &str) -> String {
    //     match plan_sql(&catalog(), sql).unwrap() {
    //         Plan::Query(plan) => plan.to_string(),
    //         other => panic!("expected query, got {other:?}"),
    //     }
    // }

    #[test]
    fn update_assignments_are_folded() {
        let Plan::Update { assignments, .. } =
            plan_sql(&catalog(), "UPDATE users SET age = 1 + 1").unwrap()
        else {
            panic!("expected update");
        };
        assert_eq!(assignments[0].value, ScalarExpr::literal(2i64));
    }
}
