//! An optimizer rewrites a plan into an equivalent one that cat be more
//! efficient to execute

#![allow(dead_code)]
pub mod constant_folding;
pub mod predicate_pushdown;
pub mod projection_pruning;

use crate::planner::{
    error::PlanResult,
    logical::{LogicalPlan, Plan, UpdateAssignment},
    optimizer::{
        constant_folding::{ConstantFolding, fold_expr},
        predicate_pushdown::PredicatePushdown,
        projection_pruning::ProjectionPruning,
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
        Self::new(vec![
            Box::new(ConstantFolding),
            Box::new(PredicatePushdown),
            Box::new(ConstantFolding),
            Box::new(ProjectionPruning),
        ])
    }
}

impl Optimizer {
    pub fn new(rules: Vec<Box<dyn OptimizerRule>>) -> Self {
        Self { rules }
    }

    pub fn rule_names(&self) -> Vec<&'static str> {
        self.rules.iter().map(|r| r.name()).collect()
    }

    pub fn optimize(&self, plan: LogicalPlan) -> PlanResult<LogicalPlan> {
        self.rules
            .iter()
            .try_fold(plan, |plan, rule| rule.rewrite(plan))
    }

    pub fn optimize_plan(&self, plan: Plan) -> PlanResult<Plan> {
        Ok(match plan {
            Plan::Query(input) => Plan::Query(self.optimize(input)?),
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
mod tests {
    use crate::planner::{
        Plan, expr::ScalarExpr, optimizer::Optimizer, plan_sql, test_catalog::catalog,
    };

    fn optimized(sql: &str) -> String {
        match plan_sql(&catalog(), sql).unwrap() {
            Plan::Query(plan) => plan.to_string(),
            other => panic!("expected query, got {other:?}"),
        }
    }

    #[test]
    fn default_rule_order() {
        assert_eq!(
            Optimizer::default().rule_names(),
            vec![
                "constant_folding",
                "predicate_pushdown",
                "constant_folding",
                "projection_pruning"
            ]
        );
    }

    #[test]
    fn inner_join_predicates_move_to_their_side() {
        assert_eq!(
            optimized(
                "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id WHERE u.age > 18 AND o.amount < 10"
            ),
            "Project: u.name\n  Join: Inner ON u.id = o.user_id\n    Filter: u.age > 18\n      Scan: users AS u [id, name, age]\n    Filter: o.amount < 10\n      Scan: orders AS o [user_id, amount]\n"
        );
    }

    #[test]
    fn cross_join_with_equality_becomes_inner_join() {
        let plan = optimized("SELECT u.name FROM users u, orders o WHERE u.id = o.user_id");
        assert!(plan.contains("Join: Inner ON u.id = o.user_id"), "{plan}");
        assert!(!plan.contains("Filter"), "{plan}");
    }

    #[test]
    fn left_join_keeps_where_on_nullable_side_above_join() {
        assert_eq!(
            optimized(
                "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount IS NULL AND u.age > 1"
            ),
            "Project: u.name\n  Filter: o.amount IS NULL\n    Join: Left ON u.id = o.user_id\n      Filter: u.age > 1\n        Scan: users AS u [id, name, age]\n      Scan: orders AS o [user_id, amount]\n"
        );
    }

    #[test]
    fn left_join_on_condition_filters_right_input_only() {
        assert_eq!(
            optimized(
                "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount > 5 AND u.age > 1"
            ),
            "Project: u.name\n  Join: Left ON (u.id = o.user_id) AND (u.age > 1)\n    Scan: users AS u [id, name, age]\n    Filter: o.amount > 5\n      Scan: orders AS o [user_id, amount]\n"
        );
    }

    #[test]
    fn full_join_blocks_pushdown() {
        let plan = optimized(
            "SELECT u.name FROM users u FULL JOIN orders o ON u.id = o.user_id WHERE u.age > 1",
        );
        assert!(
            plan.starts_with("Project: u.name\n  Filter: u.age > 1\n    Join: Full"),
            "{plan}"
        );
    }

    #[test]
    fn predicates_on_group_keys_move_below_aggregate() {
        assert_eq!(
            optimized(
                "SELECT city, COUNT(*) FROM users GROUP BY city HAVING city = 'x' AND COUNT(*) > 1"
            ),
            "Filter: COUNT(*) > 1\n  Aggregate: group_by=[users.city] aggregates=[COUNT(*)]\n    Filter: users.city = 'x'\n      Scan: users [city]\n"
        );
    }

    #[test]
    fn predicates_do_not_cross_limit() {
        let plan = optimized("SELECT name FROM users ORDER BY age LIMIT 2");
        assert!(plan.contains("Limit"), "{plan}");
        assert!(!plan.contains("Filter"), "{plan}");
    }

    #[test]
    fn constant_predicates_fold_away() {
        assert_eq!(
            optimized("SELECT name FROM users WHERE 1 + 1 = 2 AND age > 3"),
            "Project: users.name\n  Filter: users.age > 3\n    Scan: users [name, age]\n"
        );
        assert_eq!(
            optimized("SELECT name FROM users WHERE 1 = 0"),
            "Values: 0 rows [name]\n"
        );
        assert_eq!(
            optimized("SELECT name FROM users LIMIT 0"),
            "Values: 0 rows [name]\n"
        );
    }

    #[test]
    fn empty_input_to_scalar_aggregate_is_preserved() {
        let plan = optimized("SELECT COUNT(*) FROM users WHERE false");
        assert!(plan.contains("Aggregate: group_by=[]"), "{plan}");
    }

    #[test]
    fn inner_join_with_empty_side_is_empty() {
        let plan =
            optimized("SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id AND 1 = 2");
        assert!(plan.contains("Values: 0 rows"), "{plan}");
        assert!(!plan.contains("Join"), "{plan}");
    }

    #[test]
    fn pruning_keeps_only_needed_columns() {
        assert_eq!(
            optimized("SELECT COUNT(*) FROM orders"),
            "Aggregate: group_by=[] aggregates=[COUNT(*)]\n  Scan: orders []\n"
        );
    }

    #[test]
    fn distinct_keeps_all_projected_columns() {
        let plan = optimized("SELECT DISTINCT name, city FROM users");
        assert!(plan.contains("Scan: users [name, city]"), "{plan}");
    }

    #[test]
    fn update_and_delete_keep_full_rows() {
        let Plan::Update { input, .. } =
            plan_sql(&catalog(), "UPDATE users SET age = 1 + 1 WHERE id = 2").unwrap()
        else {
            panic!("expected update");
        };
        assert_eq!(input.schema().len(), 4);
    }

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
