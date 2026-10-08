use crate::sql::{Expr, FunctionArgs, Select, SelectColumn, Statement, TableRef, UnaryOperator};

use super::catalog::Catalog;
use super::error::{PlanError, PlanResult};
use super::expr::{AggregateCall, AggregateFunction, ScalarExpr, ScalarFunction};
use super::logical::{LogicalPlan, Plan};
use super::schema::{ColumnIdGenerator, Field, Schema};
use super::types::DataType;
use super::value::Value;

pub struct Binder<'a> {
    catalog: &'a Catalog,
    ids: ColumnIdGenerator,
}

#[derive(Debug, Clone)]
struct Relation {
    alias: String,
    fields: Vec<Field>,
}

#[derive(Debug, Default)]
struct Scope {
    relations: Vec<Relation>,
}

impl Scope {
    fn add(&mut self, alias: &str, fields: Vec<Field>) -> PlanResult<()> {
        if self
            .relations
            .iter()
            .any(|r| r.alias.eq_ignore_ascii_case(alias))
        {
            return Err(PlanError::DuplicateAlias(alias.to_string()));
        }

        self.relations.push(Relation {
            alias: alias.to_string(),
            fields,
        });
        Ok(())
    }

    fn relation(&self, alias: &str) -> PlanResult<&Relation> {
        self.relations
            .iter()
            .find(|r| r.alias.eq_ignore_ascii_case(alias))
            .ok_or_else(|| PlanError::AliasNotFound(alias.to_string()))
    }

    fn resolve(&self, table: Option<&str>, column: &str) -> PlanResult<&Field> {
        let not_found = || PlanError::ColumnNotFound {
            table: table.map(String::from),
            column: column.to_string(),
        };

        match table {
            Some(t) => self
                .relation(t)?
                .fields
                .iter()
                .find(|f| f.name.eq_ignore_ascii_case(column))
                .ok_or_else(not_found),
            None => {
                let mut matches = self.relations.iter().filter_map(|r| {
                    r.fields
                        .iter()
                        .find(|f| f.name.eq_ignore_ascii_case(column))
                        .map(|f| (r, f))
                });

                let (_, field) = matches.next().ok_or_else(not_found)?;
                let others: Vec<&Relation> = matches.map(|(r, _)| r).collect();
                if others.is_empty() {
                    Ok(field)
                } else {
                    let mut candidates = vec![field.qualifier.clone().unwrap_or_default()];
                    candidates.extend(others.iter().map(|r| r.alias.clone()));
                    Err(PlanError::AmbiguousColumn {
                        column: column.to_string(),
                        candidates,
                    })
                }
            }
        }
    }

    fn all_fields(&self) -> impl Iterator<Item = &Field> {
        self.relations.iter().flat_map(|r| r.fields.iter())
    }
}

fn column_ref(field: &Field) -> ScalarExpr {
    ScalarExpr::column(field.id, field.qualified_name(), field.data_type)
}

#[derive(Debug, Default)]
struct AggregateSet {
    calls: Vec<AggregateCall>,
    fields: Vec<Field>,
}

impl AggregateSet {
    fn register(&mut self, call: AggregateCall, ids: &mut ColumnIdGenerator) -> ScalarExpr {
        if let Some(i) = self.calls.iter().position(|c| c == &call) {
            return column_ref(&self.fields[i]);
        }
        let field = Field {
            id: ids.next_id(),
            qualifier: None,
            name: call.to_string(),
            data_type: call.data_type(),
            nullable: call.nullable(),
        };
        let expr = column_ref(&field);
        self.calls.push(call);
        self.fields.push(field);
        expr
    }
}

enum AggregateMode<'s> {
    Forbidden(&'static str),
    Allowed(&'s mut AggregateSet),
    InsideAggregate(String),
}

struct ExprBinder<'s> {
    scope: &'s Scope,
    ids: &'s mut ColumnIdGenerator,
    mode: AggregateMode<'s>,
}

impl ExprBinder<'_> {
    fn bind(&mut self, expr: &Expr) -> PlanResult<ScalarExpr> {
        match expr {
            Expr::Column { table, name } => {
                Ok(column_ref(self.scope.resolve(table.as_deref(), name)?))
            }
            Expr::Literal(lit) => Ok(ScalarExpr::Literal(Value::from_literal(lit))),
            Expr::Nested(inner) => self.bind(inner),
            Expr::UnaryOp { op, expr: inner } => {
                let inner = self.bind(inner)?;
                let t = inner.data_type();
                let valid = match op {
                    UnaryOperator::Not => t.is_boolean(),
                    UnaryOperator::Minus | UnaryOperator::Plus => t.is_numeric() || t.is_null(),
                };
                if !valid {
                    return Err(PlanError::TypeMismatch(format!(
                        "operator {} cannot be applied to {t} in {expr}",
                        op.symbol().trim()
                    )));
                }
                Ok(ScalarExpr::Unary {
                    op: *op,
                    expr: Box::new(inner),
                })
            }
            Expr::BinaryOp { left, op, right } => {
                let l = self.bind(left)?;
                let r = self.bind(right)?;
                let (lt, rt) = (l.data_type(), r.data_type());
                let valid = if op.is_logical() {
                    lt.is_boolean() && rt.is_boolean()
                } else if op.is_comparison() {
                    lt.is_comparable_with(rt)
                } else if op.is_arithmetic() {
                    lt.numeric_result(rt).is_some()
                } else {
                    true
                };
                if !valid {
                    return Err(PlanError::TypeMismatch(format!(
                        "operator {} cannot be applied to {lt} and {rt} in {expr}",
                        op.symbol()
                    )));
                }
                Ok(ScalarExpr::binary(*op, l, r))
            }
            Expr::IsNull {
                expr: inner,
                negated,
            } => Ok(ScalarExpr::IsNull {
                expr: Box::new(self.bind(inner)?),
                negated: *negated,
            }),
            Expr::InList {
                expr: inner,
                list,
                negated,
            } => {
                let inner = self.bind(inner)?;
                let list = list
                    .iter()
                    .map(|item| self.bind(item))
                    .collect::<PlanResult<Vec<_>>>()?;
                let t = inner.data_type();
                if let Some(bad) = list.iter().find(|i| !t.is_comparable_with(i.data_type())) {
                    return Err(PlanError::TypeMismatch(format!(
                        "IN list item {bad} of type {} is not comparable with {t}",
                        bad.data_type()
                    )));
                }
                Ok(ScalarExpr::InList {
                    expr: Box::new(inner),
                    list,
                    negated: *negated,
                })
            }
            Expr::Between {
                expr: inner,
                low,
                high,
                negated,
            } => {
                let inner = self.bind(inner)?;
                let low = self.bind(low)?;
                let high = self.bind(high)?;
                let t = inner.data_type();
                if !t.is_comparable_with(low.data_type()) || !t.is_comparable_with(high.data_type())
                {
                    return Err(PlanError::TypeMismatch(format!(
                        "BETWEEN bounds are not comparable with {t} in {expr}"
                    )));
                }
                let range = ScalarExpr::and(
                    ScalarExpr::binary(crate::sql::BinaryOperator::GtEq, inner.clone(), low),
                    ScalarExpr::binary(crate::sql::BinaryOperator::LtEq, inner, high),
                );
                Ok(if *negated {
                    ScalarExpr::negate(range)
                } else {
                    range
                })
            }
            Expr::Like {
                expr: inner,
                pattern,
                negated,
                case_insensitive,
            } => {
                let inner = self.bind(inner)?;
                let t = inner.data_type();
                if !(t.is_textual() || t.is_null()) {
                    return Err(PlanError::TypeMismatch(format!(
                        "LIKE requires a text operand, got {t} in {expr}"
                    )));
                }
                Ok(ScalarExpr::Like {
                    expr: Box::new(inner),
                    pattern: pattern.clone(),
                    negated: *negated,
                    case_insensitive: *case_insensitive,
                })
            }
            Expr::Function { name, args } => self.bind_function(expr, name, args),
        }
    }

    fn bind_function(
        &mut self,
        expr: &Expr,
        name: &str,
        args: &FunctionArgs,
    ) -> PlanResult<ScalarExpr> {
        if let Some(func) = AggregateFunction::from_name(name) {
            return self.bind_aggregate(expr, func, args);
        }
        let func = ScalarFunction::from_name(name)
            .ok_or_else(|| PlanError::UnknownFunction(name.to_string()))?;
        let args = match args {
            FunctionArgs::List {
                args,
                distinct: false,
            } => args
                .iter()
                .map(|a| self.bind(a))
                .collect::<PlanResult<Vec<_>>>()?,
            _ => {
                return Err(PlanError::Unsupported(format!(
                    "* or DISTINCT argument to scalar function {expr}"
                )));
            }
        };
        let types: Vec<DataType> = args.iter().map(ScalarExpr::data_type).collect();
        func.return_type(&types)?;
        Ok(ScalarExpr::Function { func, args })
    }

    fn bind_aggregate(
        &mut self,
        expr: &Expr,
        _func: AggregateFunction,
        _args: &FunctionArgs,
    ) -> PlanResult<ScalarExpr> {
        match &self.mode {
            AggregateMode::Forbidden(clause) => Err(PlanError::AggregateNotAllowed { clause }),
            AggregateMode::InsideAggregate(outer) => {
                Err(PlanError::NestedAggregate(format!("{expr} inside {outer}")))
            }
            AggregateMode::Allowed(_) => Err(PlanError::Unsupported(format!(
                "aggregate {expr} is not bound yet"
            ))),
        }
    }
}

fn expect_boolean(expr: &ScalarExpr, clause: &str) -> PlanResult<()> {
    let t = expr.data_type();
    if t.is_boolean() {
        Ok(())
    } else {
        Err(PlanError::TypeMismatch(format!(
            "argument of {clause} must be BOOLEAN, got {t}"
        )))
    }
}

enum ItemSource<'q> {
    Field(Field),
    Ast { expr: Expr, alias: Option<&'q str> },
}

struct BoundItem {
    expr: ScalarExpr,
    name: String,
    qualifier: Option<String>,
}

impl BoundItem {
    fn field(&self, input: &Schema, ids: &mut ColumnIdGenerator) -> Field {
        let id = match &self.expr {
            ScalarExpr::Column(c) => c.id,
            _ => ids.next_id(),
        };
        Field {
            id,
            qualifier: self.qualifier.clone(),
            name: self.name.clone(),
            data_type: self.expr.data_type(),
            nullable: self.expr.nullable(input),
        }
    }
}

impl<'a> Binder<'a> {
    pub fn new(catalog: &'a Catalog) -> Self {
        Self {
            catalog,
            ids: ColumnIdGenerator::default(),
        }
    }

    pub fn bind(&mut self, statement: &Statement) -> PlanResult<Plan> {
        match statement {
            Statement::Select(select) => self.bind_select(select).map(Plan::Query),
            Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_)
            | Statement::CreateTable(_)
            | Statement::DropTable(_) => Err(PlanError::Unsupported(
                "DML and DDL binding is not implemented yet".into(),
            )),
            Statement::Begin => Ok(Plan::Begin),
            Statement::Commit => Ok(Plan::Commit),
            Statement::Rollback => Ok(Plan::Rollback),
            Statement::ShowTables => Ok(Plan::ShowTables),
            Statement::Describe(table) => {
                let schema = self.catalog.get_table(table)?;
                Ok(Plan::Describe(schema.name.clone()))
            }
        }
    }

    pub fn bind_select(&mut self, select: &Select) -> PlanResult<LogicalPlan> {
        if !select.group_by.is_empty()
            || select.having.is_some()
            || !select.order_by.is_empty()
            || select.distinct
            || select.limit.is_some()
            || select.offset.is_some()
        {
            return Err(PlanError::Unsupported(
                "GROUP BY, HAVING, ORDER BY, DISTINCT, LIMIT and OFFSET".into(),
            ));
        }

        let (mut plan, scope) = self.bind_from(&select.from)?;

        if let Some(where_clause) = &select.where_clause {
            let predicate = self.bind_scalar(where_clause, &scope, "WHERE")?;
            expect_boolean(&predicate, "WHERE")?;
            plan = LogicalPlan::filter(plan, predicate);
        }

        let sources = self.expand_select_items(&select.columns, &scope)?;
        let items = sources
            .iter()
            .map(|source| self.bind_item(source, &scope))
            .collect::<PlanResult<Vec<_>>>()?;

        let mut exprs: Vec<ScalarExpr> = Vec::with_capacity(items.len());
        let mut fields: Vec<Field> = Vec::with_capacity(items.len());
        for item in &items {
            fields.push(item.field(plan.schema(), &mut self.ids));
            exprs.push(item.expr.clone());
        }

        Ok(LogicalPlan::project(plan, exprs, fields))
    }

    fn bind_from(&mut self, from: &TableRef) -> PlanResult<(LogicalPlan, Scope)> {
        let mut scope = Scope::default();
        let mut plan = self.bind_table(&from.base, from.base_alias.as_deref(), &mut scope)?;
        for join in &from.joins {
            let right = self.bind_table(&join.table, join.alias.as_deref(), &mut scope)?;
            let on = match &join.on {
                Some(expr) => {
                    let predicate = self.bind_scalar(expr, &scope, "JOIN conditions")?;
                    expect_boolean(&predicate, "JOIN ... ON")?;
                    Some(predicate)
                }
                None => None,
            };
            plan = LogicalPlan::join(plan, right, join.join_type, on);
        }
        Ok((plan, scope))
    }

    fn bind_table(
        &mut self,
        name: &str,
        alias: Option<&str>,
        scope: &mut Scope,
    ) -> PlanResult<LogicalPlan> {
        let catalog = self.catalog;
        let table = catalog.get_table(name)?;
        let visible = alias.unwrap_or(name);
        let fields: Vec<Field> = table
            .columns
            .iter()
            .enumerate()
            .map(|(i, column)| Field {
                id: self.ids.next_id(),
                qualifier: Some(visible.to_string()),
                name: column.name.clone(),
                data_type: table.column_type(i),
                nullable: table.is_nullable(i),
            })
            .collect();
        scope.add(visible, fields.clone())?;
        Ok(LogicalPlan::Scan {
            table: table.name.clone(),
            alias: alias.map(String::from),
            projection: (0..fields.len()).collect(),
            schema: Schema::new(fields),
        })
    }

    fn bind_scalar(
        &mut self,
        expr: &Expr,
        scope: &Scope,
        clause: &'static str,
    ) -> PlanResult<ScalarExpr> {
        ExprBinder {
            scope,
            ids: &mut self.ids,
            mode: AggregateMode::Forbidden(clause),
        }
        .bind(expr)
    }

    fn expand_select_items<'q>(
        &self,
        columns: &'q [SelectColumn],
        scope: &Scope,
    ) -> PlanResult<Vec<ItemSource<'q>>> {
        let mut out = Vec::with_capacity(columns.len());
        for column in columns {
            match column {
                SelectColumn::Wildcard => {
                    out.extend(scope.all_fields().cloned().map(ItemSource::Field));
                }
                SelectColumn::QualifiedWildcard(table) => {
                    out.extend(
                        scope
                            .relation(table)?
                            .fields
                            .iter()
                            .cloned()
                            .map(ItemSource::Field),
                    );
                }
                SelectColumn::Column { table, name } => out.push(ItemSource::Ast {
                    expr: Expr::Column {
                        table: table.clone(),
                        name: name.clone(),
                    },
                    alias: None,
                }),
                SelectColumn::Expr { expr, alias } => out.push(ItemSource::Ast {
                    expr: expr.clone(),
                    alias: alias.as_deref(),
                }),
            }
        }
        Ok(out)
    }

    fn bind_item(&mut self, source: &ItemSource<'_>, scope: &Scope) -> PlanResult<BoundItem> {
        match source {
            ItemSource::Field(field) => Ok(BoundItem {
                expr: column_ref(field),
                name: field.name.clone(),
                qualifier: field.qualifier.clone(),
            }),
            ItemSource::Ast { expr, alias } => {
                let bound = self.bind_scalar(expr, scope, "SELECT")?;
                let (name, qualifier) = match (alias, &bound, expr) {
                    (Some(alias), _, _) => (alias.to_string(), None),
                    (None, ScalarExpr::Column(c), Expr::Column { .. }) => {
                        match scope.all_fields().find(|f| f.id == c.id) {
                            Some(f) => (f.name.clone(), f.qualifier.clone()),
                            None => (expr.to_string(), None),
                        }
                    }
                    (None, _, expr) => (expr.to_string(), None),
                };
                Ok(BoundItem {
                    expr: bound,
                    name,
                    qualifier,
                })
            }
        }
    }
}
