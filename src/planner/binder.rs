use crate::sql::{
    CreateTable, Delete, DropTable, Expr, FunctionArgs, Insert, InsertSource, LiteralValue,
    OrderBy, Select, SelectColumn, Statement, TableRef, UnaryOperator, Update,
};

use super::catalog::{Catalog, TableSchema};
use super::error::{PlanError, PlanResult};
use super::expr::{AggregateCall, AggregateFunction, ScalarExpr, ScalarFunction, SortKey};
use super::logical::{LogicalPlan, Plan, UpdateAssignment};
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
        func: AggregateFunction,
        args: &FunctionArgs,
    ) -> PlanResult<ScalarExpr> {
        match &self.mode {
            AggregateMode::Forbidden(clause) => {
                return Err(PlanError::AggregateNotAllowed { clause });
            }
            AggregateMode::InsideAggregate(outer) => {
                return Err(PlanError::NestedAggregate(format!("{expr} inside {outer}")));
            }
            AggregateMode::Allowed(_) => {}
        }

        let (arg, distinct) = match args {
            FunctionArgs::Star => (None, false),
            FunctionArgs::List { args, distinct } => match args.as_slice() {
                [single] => {
                    let mut inner = ExprBinder {
                        scope: self.scope,
                        ids: &mut *self.ids,
                        mode: AggregateMode::InsideAggregate(expr.to_string()),
                    };
                    (Some(inner.bind(single)?), *distinct)
                }
                _ => {
                    return Err(PlanError::InvalidAggregate(format!(
                        "{expr} takes exactly one argument"
                    )));
                }
            },
        };
        func.return_type(arg.as_ref().map(ScalarExpr::data_type))?;

        let call = AggregateCall {
            func,
            arg,
            distinct,
        };
        match &mut self.mode {
            AggregateMode::Allowed(set) => Ok(set.register(call, self.ids)),
            _ => Err(PlanError::Internal(
                "aggregate mode changed during binding".into(),
            )),
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

fn ast_contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function { name, args } => {
            AggregateFunction::from_name(name).is_some()
                || matches!(args, FunctionArgs::List { args, .. } if args.iter().any(ast_contains_aggregate))
        }
        Expr::Column { .. } | Expr::Literal(_) => false,
        Expr::BinaryOp { left, right, .. } => {
            ast_contains_aggregate(left) || ast_contains_aggregate(right)
        }
        Expr::UnaryOp { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Like { expr, .. }
        | Expr::Nested(expr) => ast_contains_aggregate(expr),
        Expr::InList { expr, list, .. } => {
            ast_contains_aggregate(expr) || list.iter().any(ast_contains_aggregate)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            ast_contains_aggregate(expr)
                || ast_contains_aggregate(low)
                || ast_contains_aggregate(high)
        }
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

enum OrderTarget {
    Item(usize),
    Expr(ScalarExpr),
}

struct GroupContext {
    group_by: Vec<ScalarExpr>,
    group_fields: Vec<Field>,
    aggregates: AggregateSet,
}

impl GroupContext {
    fn output_schema(&self) -> Schema {
        let mut fields = self.group_fields.clone();
        fields.extend(self.aggregates.fields.iter().cloned());
        Schema::new(fields)
    }

    fn rewrite(&self, expr: ScalarExpr, output: &Schema) -> PlanResult<ScalarExpr> {
        let rewritten = expr.transform_down(&mut |e| {
            Ok(self
                .group_by
                .iter()
                .position(|g| g == e)
                .map(|i| column_ref(&self.group_fields[i])))
        })?;
        let mut offending = None;
        rewritten.any(&mut |e| match e {
            ScalarExpr::Column(c) if !output.contains(c.id) => {
                offending = Some(c.name.clone());
                true
            }
            _ => false,
        });
        match offending {
            Some(column) => Err(PlanError::NonAggregateColumn { column }),
            None => Ok(rewritten),
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
            Statement::Insert(insert) => self.bind_insert(insert),
            Statement::Update(update) => self.bind_update(update),
            Statement::Delete(delete) => self.bind_delete(delete),
            Statement::CreateTable(create) => self.bind_create_table(create),
            Statement::DropTable(drop) => self.bind_drop_table(drop),
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
        let (mut plan, scope) = self.bind_from(&select.from)?;

        if let Some(where_clause) = &select.where_clause {
            let predicate = self.bind_scalar(where_clause, &scope, "WHERE")?;
            expect_boolean(&predicate, "WHERE")?;
            plan = LogicalPlan::filter(plan, predicate);
        }

        let sources = self.expand_select_items(&select.columns, &scope)?;

        let grouped = !select.group_by.is_empty()
            || select.having.is_some()
            || sources
                .iter()
                .any(|s| matches!(s, ItemSource::Ast { expr, .. } if ast_contains_aggregate(expr)))
            || select
                .order_by
                .iter()
                .any(|o| ast_contains_aggregate(&o.expr));

        let mut group = if grouped {
            Some(self.bind_group_by(&select.group_by, &sources, &scope, plan.schema())?)
        } else {
            None
        };

        let mut items = sources
            .iter()
            .map(|source| self.bind_item(source, &scope, group.as_mut()))
            .collect::<PlanResult<Vec<_>>>()?;

        let having = select
            .having
            .as_ref()
            .map(|h| self.bind_post_aggregate(h, &scope, group.as_mut(), "HAVING"))
            .transpose()?;

        let order_targets = select
            .order_by
            .iter()
            .map(|ob| self.bind_order_target(ob, &sources, &items, &scope, group.as_mut()))
            .collect::<PlanResult<Vec<_>>>()?;

        let (plan, items, order_targets) = match group {
            None => (plan, items, order_targets),
            Some(group) => {
                let output = group.output_schema();
                for item in items.iter_mut() {
                    item.expr = group.rewrite(item.expr.clone(), &output)?;
                }
                let having = having.map(|h| group.rewrite(h, &output)).transpose()?;
                let order_targets = order_targets
                    .into_iter()
                    .map(|t| match t {
                        OrderTarget::Expr(e) => group.rewrite(e, &output).map(OrderTarget::Expr),
                        item => Ok(item),
                    })
                    .collect::<PlanResult<Vec<_>>>()?;

                plan = LogicalPlan::Aggregate {
                    input: Box::new(plan),
                    group_by: group.group_by,
                    aggregates: group.aggregates.calls,
                    schema: output,
                };
                if let Some(predicate) = having {
                    expect_boolean(&predicate, "HAVING")?;
                    plan = LogicalPlan::filter(plan, predicate);
                }
                (plan, items, order_targets)
            }
        };

        self.finish_select(plan, select, items, order_targets)
    }

    fn finish_select(
        &mut self,
        plan: LogicalPlan,
        select: &Select,
        items: Vec<BoundItem>,
        order_targets: Vec<OrderTarget>,
    ) -> PlanResult<LogicalPlan> {
        let visible = items.len();
        let mut exprs: Vec<ScalarExpr> = Vec::with_capacity(items.len());
        let mut fields: Vec<Field> = Vec::with_capacity(items.len());
        for item in &items {
            fields.push(item.field(plan.schema(), &mut self.ids));
            exprs.push(item.expr.clone());
        }

        let mut sort_positions = Vec::with_capacity(order_targets.len());
        for target in order_targets {
            let position = match target {
                OrderTarget::Item(i) => i,
                OrderTarget::Expr(expr) => match exprs.iter().position(|e| e == &expr) {
                    Some(i) => i,
                    None => {
                        if select.distinct {
                            return Err(PlanError::InvalidOrderBy(format!(
                                "for SELECT DISTINCT, ORDER BY expression {expr} must appear in the select list"
                            )));
                        }
                        let hidden = BoundItem {
                            name: expr.to_string(),
                            qualifier: None,
                            expr,
                        };
                        fields.push(hidden.field(plan.schema(), &mut self.ids));
                        exprs.push(hidden.expr);
                        exprs.len() - 1
                    }
                },
            };
            sort_positions.push(position);
        }

        let mut plan = LogicalPlan::project(plan, exprs, fields.clone());

        if select.distinct {
            plan = LogicalPlan::Distinct {
                input: Box::new(plan),
            };
        }

        if !select.order_by.is_empty() {
            let keys = select
                .order_by
                .iter()
                .zip(sort_positions)
                .map(|(ob, position)| SortKey {
                    expr: column_ref(&fields[position]),
                    ascending: ob.ascending,
                    nulls_first: ob.nulls_first.unwrap_or(!ob.ascending),
                })
                .collect();
            plan = LogicalPlan::Sort {
                input: Box::new(plan),
                keys,
            };
        }

        if select.limit.is_some() || select.offset.is_some() {
            plan = LogicalPlan::Limit {
                input: Box::new(plan),
                limit: select.limit,
                offset: select.offset.unwrap_or(0),
            };
        }

        if fields.len() > visible {
            let visible_fields = fields[..visible].to_vec();
            let exprs = visible_fields.iter().map(column_ref).collect();
            plan = LogicalPlan::project(plan, exprs, visible_fields);
        }

        Ok(plan)
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

    fn bind_post_aggregate(
        &mut self,
        expr: &Expr,
        scope: &Scope,
        group: Option<&mut GroupContext>,
        clause: &'static str,
    ) -> PlanResult<ScalarExpr> {
        match group {
            Some(group) => ExprBinder {
                scope,
                ids: &mut self.ids,
                mode: AggregateMode::Allowed(&mut group.aggregates),
            }
            .bind(expr),
            None => self.bind_scalar(expr, scope, clause),
        }
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

    fn bind_item(
        &mut self,
        source: &ItemSource<'_>,
        scope: &Scope,
        group: Option<&mut GroupContext>,
    ) -> PlanResult<BoundItem> {
        match source {
            ItemSource::Field(field) => Ok(BoundItem {
                expr: column_ref(field),
                name: field.name.clone(),
                qualifier: field.qualifier.clone(),
            }),
            ItemSource::Ast { expr, alias } => {
                let bound = self.bind_post_aggregate(expr, scope, group, "SELECT")?;
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

    fn bind_group_by(
        &mut self,
        group_by: &[Expr],
        sources: &[ItemSource<'_>],
        scope: &Scope,
        input: &Schema,
    ) -> PlanResult<GroupContext> {
        let mut exprs: Vec<ScalarExpr> = Vec::with_capacity(group_by.len());
        for expr in group_by {
            let bound = match expr {
                Expr::Literal(LiteralValue::Integer(position)) => {
                    let source = select_item_at(sources, *position, "GROUP BY")?;
                    self.bind_source_scalar(source, scope, "GROUP BY")?
                }
                Expr::Column { table: None, name } => {
                    match self.bind_scalar(expr, scope, "GROUP BY") {
                        Err(PlanError::ColumnNotFound { .. }) => {
                            let source = sources
                            .iter()
                            .find(|s| matches!(s, ItemSource::Ast { alias: Some(a), .. } if a.eq_ignore_ascii_case(name)))
                            .ok_or_else(|| PlanError::ColumnNotFound {
                                table: None,
                                column: name.clone(),
                            })?;
                            self.bind_source_scalar(source, scope, "GROUP BY")?
                        }
                        other => other?,
                    }
                }
                other => self.bind_scalar(other, scope, "GROUP BY")?,
            };
            if !exprs.contains(&bound) {
                exprs.push(bound);
            }
        }

        let group_fields = exprs
            .iter()
            .map(|expr| match expr {
                ScalarExpr::Column(c) => input
                    .index_of(c.id)
                    .map(|i| input.field(i).clone())
                    .ok_or_else(|| {
                        PlanError::Internal(format!("group column {} not in input", c.name))
                    }),
                other => Ok(Field {
                    id: self.ids.next_id(),
                    qualifier: None,
                    name: other.to_string(),
                    data_type: other.data_type(),
                    nullable: other.nullable(input),
                }),
            })
            .collect::<PlanResult<Vec<_>>>()?;

        Ok(GroupContext {
            group_by: exprs,
            group_fields,
            aggregates: AggregateSet::default(),
        })
    }

    fn bind_source_scalar(
        &mut self,
        source: &ItemSource<'_>,
        scope: &Scope,
        clause: &'static str,
    ) -> PlanResult<ScalarExpr> {
        match source {
            ItemSource::Field(field) => Ok(column_ref(field)),
            ItemSource::Ast { expr, .. } => self.bind_scalar(expr, scope, clause),
        }
    }

    fn bind_order_target(
        &mut self,
        order_by: &OrderBy,
        sources: &[ItemSource<'_>],
        items: &[BoundItem],
        scope: &Scope,
        group: Option<&mut GroupContext>,
    ) -> PlanResult<OrderTarget> {
        match &order_by.expr {
            Expr::Literal(LiteralValue::Integer(position)) => {
                select_item_at(sources, *position, "ORDER BY")?;
                Ok(OrderTarget::Item(*position as usize - 1))
            }
            Expr::Column { table: None, name } => {
                let matching: Vec<usize> = items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| item.name.eq_ignore_ascii_case(name))
                    .map(|(i, _)| i)
                    .collect();
                match matching.as_slice() {
                    [] => self
                        .bind_post_aggregate(&order_by.expr, scope, group, "ORDER BY")
                        .map(OrderTarget::Expr),
                    [first, rest @ ..] => {
                        if rest.iter().all(|i| items[*i].expr == items[*first].expr) {
                            Ok(OrderTarget::Item(*first))
                        } else {
                            Err(PlanError::InvalidOrderBy(format!(
                                "ORDER BY {name} is ambiguous"
                            )))
                        }
                    }
                }
            }
            other => self
                .bind_post_aggregate(other, scope, group, "ORDER BY")
                .map(OrderTarget::Expr),
        }
    }

    fn bind_insert(&mut self, insert: &Insert) -> PlanResult<Plan> {
        let catalog = self.catalog;
        let table = catalog.get_table(&insert.table)?;

        let targets: Vec<usize> = match &insert.columns {
            None => (0..table.columns.len()).collect(),
            Some(names) => {
                let mut targets = Vec::with_capacity(names.len());
                for name in names {
                    let index =
                        table
                            .column_index(name)
                            .ok_or_else(|| PlanError::ColumnNotFound {
                                table: Some(table.name.clone()),
                                column: name.clone(),
                            })?;
                    if targets.contains(&index) {
                        return Err(PlanError::DuplicateColumn(name.clone()));
                    }
                    targets.push(index);
                }
                targets
            }
        };

        let fields: Vec<Field> = table
            .columns
            .iter()
            .enumerate()
            .map(|(i, column)| Field {
                id: self.ids.next_id(),
                qualifier: Some(table.name.clone()),
                name: column.name.clone(),
                data_type: table.column_type(i),
                nullable: table.is_nullable(i),
            })
            .collect();

        let input = match &insert.source {
            InsertSource::Values(rows) => {
                let empty = Scope::default();
                let mut bound_rows = Vec::with_capacity(rows.len());
                for row in rows {
                    if row.len() != targets.len() {
                        return Err(PlanError::InvalidInsert(format!(
                            "expected {} values, got {}",
                            targets.len(),
                            row.len()
                        )));
                    }
                    let mut provided: Vec<Option<ScalarExpr>> = vec![None; table.columns.len()];
                    for (value, &target) in row.iter().zip(&targets) {
                        provided[target] = Some(self.bind_scalar(value, &empty, "VALUES")?);
                    }
                    bound_rows.push(self.complete_insert_row(table, provided)?);
                }
                LogicalPlan::Values {
                    rows: bound_rows,
                    schema: Schema::new(fields),
                }
            }
            InsertSource::Select(select) => {
                let source = self.bind_select(select)?;
                let width = source.schema().len();
                if width != targets.len() {
                    return Err(PlanError::InvalidInsert(format!(
                        "INSERT has {} target columns but SELECT returns {width}",
                        targets.len()
                    )));
                }
                let mut provided: Vec<Option<ScalarExpr>> = vec![None; table.columns.len()];
                for (field, &target) in source.schema().fields().iter().zip(&targets) {
                    provided[target] = Some(column_ref(field));
                }
                let exprs = self.complete_insert_row(table, provided)?;
                LogicalPlan::project(source, exprs, fields)
            }
        };

        Ok(Plan::Insert {
            table: table.name.clone(),
            input,
        })
    }

    fn complete_insert_row(
        &mut self,
        table: &TableSchema,
        provided: Vec<Option<ScalarExpr>>,
    ) -> PlanResult<Vec<ScalarExpr>> {
        provided
            .into_iter()
            .enumerate()
            .map(|(i, value)| {
                let value = match value {
                    Some(v) => v,
                    None => self.bind_default(table, i)?,
                };
                self.coerce_to_column(table, i, value)
            })
            .collect()
    }

    fn bind_default(&mut self, table: &TableSchema, column: usize) -> PlanResult<ScalarExpr> {
        match table.columns[column].default_value() {
            Some(expr) => self.bind_scalar(expr, &Scope::default(), "DEFAULT"),
            None => Ok(ScalarExpr::Literal(Value::Null)),
        }
    }

    fn coerce_to_column(
        &self,
        table: &TableSchema,
        column: usize,
        value: ScalarExpr,
    ) -> PlanResult<ScalarExpr> {
        let target = table.column_type(column);
        let actual = value.data_type();
        if !actual.can_assign_to(target) {
            return Err(PlanError::TypeMismatch(format!(
                "cannot assign {actual} to column {}.{} of type {target}",
                table.name, table.columns[column].name
            )));
        }
        if !table.is_nullable(column) && matches!(value, ScalarExpr::Literal(Value::Null)) {
            return Err(PlanError::NotNullViolation {
                table: table.name.clone(),
                column: table.columns[column].name.clone(),
            });
        }
        Ok(value.cast(target))
    }

    fn bind_update(&mut self, update: &Update) -> PlanResult<Plan> {
        let catalog = self.catalog;
        let table = catalog.get_table(&update.table)?;
        let mut scope = Scope::default();
        let mut plan = self.bind_table(&update.table, update.alias.as_deref(), &mut scope)?;
        if let Some(where_clause) = &update.where_clause {
            let predicate = self.bind_scalar(where_clause, &scope, "WHERE")?;
            expect_boolean(&predicate, "WHERE")?;
            plan = LogicalPlan::filter(plan, predicate);
        }

        let mut assignments: Vec<UpdateAssignment> = Vec::with_capacity(update.assignments.len());
        for assignment in &update.assignments {
            let column = table.column_index(&assignment.column).ok_or_else(|| {
                PlanError::ColumnNotFound {
                    table: Some(table.name.clone()),
                    column: assignment.column.clone(),
                }
            })?;
            if assignments.iter().any(|a| a.column == column) {
                return Err(PlanError::DuplicateColumn(assignment.column.clone()));
            }
            let value = self.bind_scalar(&assignment.value, &scope, "UPDATE")?;
            assignments.push(UpdateAssignment {
                column,
                value: self.coerce_to_column(table, column, value)?,
            });
        }

        Ok(Plan::Update {
            table: table.name.clone(),
            input: plan,
            assignments,
        })
    }

    fn bind_delete(&mut self, delete: &Delete) -> PlanResult<Plan> {
        let catalog = self.catalog;
        let table = catalog.get_table(&delete.table)?;
        let mut scope = Scope::default();
        let mut plan = self.bind_table(&delete.table, delete.alias.as_deref(), &mut scope)?;
        if let Some(where_clause) = &delete.where_clause {
            let predicate = self.bind_scalar(where_clause, &scope, "WHERE")?;
            expect_boolean(&predicate, "WHERE")?;
            plan = LogicalPlan::filter(plan, predicate);
        }
        Ok(Plan::Delete {
            table: table.name.clone(),
            input: plan,
        })
    }

    fn bind_create_table(&mut self, create: &CreateTable) -> PlanResult<Plan> {
        if self.catalog.has_table(&create.name) && !create.if_not_exists {
            return Err(PlanError::TableAlreadyExists(create.name.clone()));
        }
        let schema = TableSchema::from_create(create)?;
        for column in 0..schema.columns.len() {
            if schema.columns[column].default_value().is_some() {
                let value = self.bind_default(&schema, column)?;
                self.coerce_to_column(&schema, column, value)?;
            }
        }
        Ok(Plan::CreateTable {
            schema,
            if_not_exists: create.if_not_exists,
        })
    }

    fn bind_drop_table(&mut self, drop: &DropTable) -> PlanResult<Plan> {
        if !drop.if_exists {
            self.catalog.get_table(&drop.name)?;
        }
        Ok(Plan::DropTable {
            name: drop.name.clone(),
            if_exists: drop.if_exists,
        })
    }
}

fn select_item_at<'s, 'q>(
    sources: &'s [ItemSource<'q>],
    position: i64,
    clause: &'static str,
) -> PlanResult<&'s ItemSource<'q>> {
    usize::try_from(position)
        .ok()
        .and_then(|p| p.checked_sub(1))
        .and_then(|p| sources.get(p))
        .ok_or(PlanError::PositionOutOfRange { clause, position })
}
