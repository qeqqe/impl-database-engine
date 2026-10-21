use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable(CreateTable),
    DropTable(DropTable),
    CreateIndex(CreateIndex),
    DropIndex(DropIndex),
    AlterTable(AlterTable),
    Select(Select),
    SetOperation(Box<SetOperation>),
    Insert(Insert),
    Update(Update),
    Delete(Delete),
    Begin(TransactionMode),
    Commit,
    Rollback,
    ShowTables,
    Describe(String),
    Explain {
        analyze: bool,
        statement: Box<Statement>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionMode {
    Deferred,
    Immediate,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub primary_key: Vec<String>,
    pub unique_keys: Vec<Vec<String>>,
    pub checks: Vec<CheckConstraint>,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckConstraint {
    pub name: Option<String>,
    pub expr: Expr,
}

impl fmt::Display for CheckConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) => write!(f, "CHECK constraint {name}"),
            None => write!(f, "CHECK ({})", self.expr),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: SqlDataType,
    pub constraints: Vec<ColumnConstraint>,
}

impl ColumnDef {
    pub fn is_not_null(&self) -> bool {
        self.constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::NotNull | ColumnConstraint::PrimaryKey))
    }

    pub fn default_value(&self) -> Option<&Expr> {
        self.constraints.iter().find_map(|c| match c {
            ColumnConstraint::Default(expr) => Some(expr),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlDataType {
    Text,
    Integer,
    Float,
    Boolean,
    Json,
    Timestamp,
    Uuid,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnConstraint {
    NotNull,
    Unique,
    PrimaryKey,
    Default(Expr),
    Check(CheckConstraint),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropTable {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlterTable {
    pub name: String,
    pub if_exists: bool,
    pub operation: AlterOperation,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlterOperation {
    AddColumn {
        column: ColumnDef,
        if_not_exists: bool,
    },
    DropColumn {
        name: String,
        if_exists: bool,
    },
    RenameTable {
        name: String,
    },
    RenameColumn {
        old: String,
        new: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndex {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropIndex {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub columns: Vec<SelectColumn>,
    pub from: Option<TableRef>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    Select(Box<Select>),
    SetOperation(Box<SetOperation>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetOperation {
    pub op: SetOperator,
    pub all: bool,
    pub left: Query,
    pub right: Query,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SetOperator {
    Union,
    Intersect,
    Except,
}

impl fmt::Display for SetOperator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            SetOperator::Union => "UNION",
            SetOperator::Intersect => "INTERSECT",
            SetOperator::Except => "EXCEPT",
        };
        f.write_str(name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    pub base: String,
    pub base_alias: Option<String>,
    pub joins: Vec<Join>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub table: String,
    pub alias: Option<String>,
    pub join_type: JoinType,
    pub on: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

impl fmt::Display for JoinType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            JoinType::Inner => "Inner",
            JoinType::Left => "Left",
            JoinType::Right => "Right",
            JoinType::Full => "Full",
            JoinType::Cross => "Cross",
        };
        f.write_str(name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectColumn {
    Wildcard,
    QualifiedWildcard(String),
    Column { table: Option<String>, name: String },
    Expr { expr: Expr, alias: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub expr: Expr,
    pub ascending: bool,
    pub nulls_first: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub source: InsertSource,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Select(Box<Select>),
    SetOperation(Box<SetOperation>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub table: String,
    pub alias: Option<String>,
    pub assignments: Vec<Assignment>,
    pub where_clause: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: String,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    pub table: String,
    pub alias: Option<String>,
    pub where_clause: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Column {
        table: Option<String>,
        name: String,
    },

    Literal(LiteralValue),

    BinaryOp {
        left: Box<Expr>,
        op: BinaryOperator,
        right: Box<Expr>,
    },

    UnaryOp {
        op: UnaryOperator,
        expr: Box<Expr>,
    },

    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },

    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },

    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },

    Like {
        expr: Box<Expr>,
        pattern: String,
        negated: bool,
        case_insensitive: bool,
    },

    Function {
        name: String,
        args: FunctionArgs,
    },

    Nested(Box<Expr>),

    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        else_result: Option<Box<Expr>>,
    },

    Cast {
        expr: Box<Expr>,
        data_type: SqlDataType,
    },

    Subquery(Box<Query>),

    Exists {
        query: Box<Query>,
        negated: bool,
    },

    InSubquery {
        expr: Box<Expr>,
        query: Box<Query>,
        negated: bool,
    },
}

impl Expr {
    pub fn visit_columns_mut<F>(&mut self, f: &mut F)
    where
        F: FnMut(&mut Option<String>, &mut String),
    {
        match self {
            Expr::Column { table, name } => f(table, name),
            Expr::Literal(_) | Expr::Subquery(_) | Expr::Exists { .. } => {}
            Expr::BinaryOp { left, right, .. } => {
                left.visit_columns_mut(f);
                right.visit_columns_mut(f);
            }
            Expr::UnaryOp { expr, .. }
            | Expr::IsNull { expr, .. }
            | Expr::Like { expr, .. }
            | Expr::Nested(expr)
            | Expr::Cast { expr, .. }
            | Expr::InSubquery { expr, .. } => expr.visit_columns_mut(f),
            Expr::InList { expr, list, .. } => {
                expr.visit_columns_mut(f);
                list.iter_mut().for_each(|e| e.visit_columns_mut(f));
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                expr.visit_columns_mut(f);
                low.visit_columns_mut(f);
                high.visit_columns_mut(f);
            }
            Expr::Function { args, .. } => {
                if let FunctionArgs::List { args, .. } = args {
                    args.iter_mut().for_each(|e| e.visit_columns_mut(f));
                }
            }
            Expr::Case {
                operand,
                branches,
                else_result,
            } => {
                if let Some(operand) = operand {
                    operand.visit_columns_mut(f);
                }
                for (when, then) in branches {
                    when.visit_columns_mut(f);
                    then.visit_columns_mut(f);
                }
                if let Some(else_result) = else_result {
                    else_result.visit_columns_mut(f);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FunctionArgs {
    Star,
    List { args: Vec<Expr>, distinct: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiteralValue {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOperator {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
    Concat,
}

impl BinaryOperator {
    pub fn is_comparison(&self) -> bool {
        matches!(
            self,
            BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq
        )
    }

    pub fn is_logical(&self) -> bool {
        matches!(self, BinaryOperator::And | BinaryOperator::Or)
    }

    pub fn is_arithmetic(&self) -> bool {
        matches!(
            self,
            BinaryOperator::Plus
                | BinaryOperator::Minus
                | BinaryOperator::Multiply
                | BinaryOperator::Divide
                | BinaryOperator::Modulo
        )
    }

    pub fn flip(&self) -> Option<BinaryOperator> {
        match self {
            BinaryOperator::Eq => Some(BinaryOperator::Eq),
            BinaryOperator::NotEq => Some(BinaryOperator::NotEq),
            BinaryOperator::Lt => Some(BinaryOperator::Gt),
            BinaryOperator::LtEq => Some(BinaryOperator::GtEq),
            BinaryOperator::Gt => Some(BinaryOperator::Lt),
            BinaryOperator::GtEq => Some(BinaryOperator::LtEq),
            _ => None,
        }
    }

    pub fn symbol(&self) -> &'static str {
        match self {
            BinaryOperator::Eq => "=",
            BinaryOperator::NotEq => "<>",
            BinaryOperator::Lt => "<",
            BinaryOperator::LtEq => "<=",
            BinaryOperator::Gt => ">",
            BinaryOperator::GtEq => ">=",
            BinaryOperator::And => "AND",
            BinaryOperator::Or => "OR",
            BinaryOperator::Plus => "+",
            BinaryOperator::Minus => "-",
            BinaryOperator::Multiply => "*",
            BinaryOperator::Divide => "/",
            BinaryOperator::Modulo => "%",
            BinaryOperator::Concat => "||",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOperator {
    Not,
    Minus,
    Plus,
}

impl UnaryOperator {
    pub fn symbol(&self) -> &'static str {
        match self {
            UnaryOperator::Not => "NOT ",
            UnaryOperator::Minus => "-",
            UnaryOperator::Plus => "+",
        }
    }
}

impl fmt::Display for LiteralValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LiteralValue::Null => f.write_str("NULL"),
            LiteralValue::Boolean(b) => f.write_str(if *b { "TRUE" } else { "FALSE" }),
            LiteralValue::Integer(i) => write!(f, "{i}"),
            LiteralValue::Float(x) => write!(f, "{x:?}"),
            LiteralValue::String(s) => write!(f, "'{}'", s.replace('\'', "''")),
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Column {
                table: Some(t),
                name,
            } => write!(f, "{t}.{name}"),
            Expr::Column { table: None, name } => f.write_str(name),
            Expr::Literal(lit) => write!(f, "{lit}"),
            Expr::BinaryOp { left, op, right } => write!(f, "{left} {} {right}", op.symbol()),
            Expr::UnaryOp { op, expr } => write!(f, "{}{expr}", op.symbol()),
            Expr::IsNull {
                expr,
                negated: false,
            } => write!(f, "{expr} IS NULL"),
            Expr::IsNull {
                expr,
                negated: true,
            } => write!(f, "{expr} IS NOT NULL"),
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "{expr} {not}IN (")?;
                write_list(f, list)?;
                f.write_str(")")
            }
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "{expr} {not}BETWEEN {low} AND {high}")
            }
            Expr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => {
                let not = if *negated { "NOT " } else { "" };
                let kw = if *case_insensitive { "ILIKE" } else { "LIKE" };
                write!(
                    f,
                    "{expr} {not}{kw} {}",
                    LiteralValue::String(pattern.clone())
                )
            }
            Expr::Function { name, args } => match args {
                FunctionArgs::Star => write!(f, "{}(*)", name.to_uppercase()),
                FunctionArgs::List { args, distinct } => {
                    write!(f, "{}(", name.to_uppercase())?;
                    if *distinct {
                        f.write_str("DISTINCT ")?;
                    }
                    write_list(f, args)?;
                    f.write_str(")")
                }
            },
            Expr::Nested(inner) => write!(f, "({inner})"),
            Expr::Case {
                operand,
                branches,
                else_result,
            } => {
                f.write_str("CASE")?;
                if let Some(operand) = operand {
                    write!(f, " {operand}")?;
                }
                for (condition, result) in branches {
                    write!(f, " WHEN {condition} THEN {result}")?;
                }
                if let Some(otherwise) = else_result {
                    write!(f, " ELSE {otherwise}")?;
                }
                f.write_str(" END")
            }
            Expr::Cast { expr, data_type } => write!(f, "CAST({expr} AS {data_type})"),
            Expr::Subquery(query) => write!(f, "({query})"),
            Expr::Exists { query, negated } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "{not}EXISTS ({query})")
            }
            Expr::InSubquery {
                expr,
                query,
                negated,
            } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "{expr} {not}IN ({query})")
            }
        }
    }
}

impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Query::Select(select) => write!(f, "{select}"),
            Query::SetOperation(operation) => write!(f, "{operation}"),
        }
    }
}

fn write_order_limit(
    f: &mut fmt::Formatter<'_>,
    order_by: &[OrderBy],
    limit: Option<usize>,
    offset: Option<usize>,
) -> fmt::Result {
    for (i, key) in order_by.iter().enumerate() {
        f.write_str(if i == 0 { " ORDER BY " } else { ", " })?;
        write!(f, "{}", key.expr)?;
        if !key.ascending {
            f.write_str(" DESC")?;
        }
        match key.nulls_first {
            Some(true) => f.write_str(" NULLS FIRST")?,
            Some(false) => f.write_str(" NULLS LAST")?,
            None => {}
        }
    }
    if let Some(limit) = limit {
        write!(f, " LIMIT {limit}")?;
    }
    if let Some(offset) = offset {
        write!(f, " OFFSET {offset}")?;
    }
    Ok(())
}

impl fmt::Display for SetOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let side = |f: &mut fmt::Formatter<'_>, query: &Query| match query {
            Query::SetOperation(_) => write!(f, "({query})"),
            Query::Select(select)
                if !select.order_by.is_empty()
                    || select.limit.is_some()
                    || select.offset.is_some() =>
            {
                write!(f, "({query})")
            }
            Query::Select(_) => write!(f, "{query}"),
        };
        side(f, &self.left)?;
        write!(f, " {}", self.op)?;
        if self.all {
            f.write_str(" ALL")?;
        }
        f.write_str(" ")?;
        side(f, &self.right)?;
        write_order_limit(f, &self.order_by, self.limit, self.offset)
    }
}

impl fmt::Display for Select {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SELECT ")?;
        if self.distinct {
            f.write_str("DISTINCT ")?;
        }
        for (i, column) in self.columns.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            match column {
                SelectColumn::Wildcard => f.write_str("*")?,
                SelectColumn::QualifiedWildcard(table) => write!(f, "{table}.*")?,
                SelectColumn::Column {
                    table: Some(table),
                    name,
                } => write!(f, "{table}.{name}")?,
                SelectColumn::Column { table: None, name } => f.write_str(name)?,
                SelectColumn::Expr { expr, alias } => {
                    write!(f, "{expr}")?;
                    if let Some(alias) = alias {
                        write!(f, " AS {alias}")?;
                    }
                }
            }
        }
        if let Some(from) = &self.from {
            write!(f, " FROM {from}")?;
        }
        if let Some(predicate) = &self.where_clause {
            write!(f, " WHERE {predicate}")?;
        }
        if !self.group_by.is_empty() {
            f.write_str(" GROUP BY ")?;
            write_list(f, &self.group_by)?;
        }
        if let Some(having) = &self.having {
            write!(f, " HAVING {having}")?;
        }
        write_order_limit(f, &self.order_by, self.limit, self.offset)
    }
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base)?;
        if let Some(alias) = &self.base_alias {
            write!(f, " AS {alias}")?;
        }
        for join in &self.joins {
            let keyword = match join.join_type {
                JoinType::Inner => "JOIN",
                JoinType::Left => "LEFT JOIN",
                JoinType::Right => "RIGHT JOIN",
                JoinType::Full => "FULL JOIN",
                JoinType::Cross => "CROSS JOIN",
            };
            write!(f, " {keyword} {}", join.table)?;
            if let Some(alias) = &join.alias {
                write!(f, " AS {alias}")?;
            }
            if let Some(on) = &join.on {
                write!(f, " ON {on}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for SqlDataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SqlDataType::Text => "TEXT",
            SqlDataType::Integer => "INTEGER",
            SqlDataType::Float => "FLOAT",
            SqlDataType::Boolean => "BOOLEAN",
            SqlDataType::Json => "JSON",
            SqlDataType::Timestamp => "TIMESTAMP",
            SqlDataType::Uuid => "UUID",
        })
    }
}

fn write_list(f: &mut fmt::Formatter<'_>, items: &[Expr]) -> fmt::Result {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}
