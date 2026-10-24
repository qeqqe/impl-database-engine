#[allow(dead_code)]
pub mod binder;
pub mod catalog;
pub mod error;
pub mod expr;
pub mod formats;
pub mod logical;
pub mod optimizer;
pub mod physical;
pub mod physical_expr;
pub mod schema;
pub mod subquery;
pub mod test_catalog;
pub mod types;
pub mod value;

use crate::{
    planner::{
        binder::Binder,
        catalog::Catalog,
        error::{PlanError, PlanResult},
        logical::Plan,
        optimizer::Optimizer,
        physical::{PhysicalPlan, PhysicalPlanner},
    },
    sql::{Parser, Statement},
};

pub fn plan_statement(catalog: &Catalog, statement: &Statement) -> PlanResult<Plan> {
    let plan = Binder::new(catalog).bind(statement)?;
    Optimizer::default().optimize_plan(plan)
}

pub fn plan_sql(catalog: &Catalog, sql: &str) -> PlanResult<Plan> {
    plan_statement(catalog, &Parser::parse(sql)?)
}

pub fn plan_query(catalog: &Catalog, sql: &str) -> PlanResult<PhysicalPlan> {
    match plan_sql(catalog, sql)? {
        Plan::Query(logical) => PhysicalPlanner::new(catalog).plan(&logical),
        other => Err(PlanError::Unsupported(format!(
            "expected a query, got {}",
            plan_kind(&other)
        ))),
    }
}

fn plan_kind(plan: &Plan) -> &'static str {
    match plan {
        Plan::Query(_) => "SELECT",
        Plan::Insert { .. } => "INSERT",
        Plan::Update { .. } => "UPDATE",
        Plan::Delete { .. } => "DELETE",
        Plan::CreateTable { .. } => "CREATE TABLE",
        Plan::DropTable { .. } => "DROP TABLE",
        Plan::CreateIndex { .. } => "CREATE INDEX",
        Plan::DropIndex { .. } => "DROP INDEX",
        Plan::AlterTable { .. } => "ALTER TABLE",
        Plan::Begin(_) => "BEGIN",
        Plan::Commit => "COMMIT",
        Plan::Rollback => "ROLLBACK",
        Plan::ShowTables => "SHOW TABLES",
        Plan::Describe(_) => "DESCRIBE",
        Plan::Explain { .. } => "EXPLAIN",
    }
}
