//! Turns a parsed SQL statement into a [`Plan`].
//!
//! This is also where names are resolved ("binding"): `WHERE a > 1` becomes
//! "column 0 > 1", checked against the catalog. Anything Fishbowl does not
//! implement yet is rejected here with [`Error::Unsupported`], never ignored.

use sqlparser::ast;

use crate::catalog::{Catalog, Column, TableSchema};
use crate::error::{Error, Result, unsupported};
use crate::expr::{BinaryOp, Expr, UnaryOp};
use crate::plan::{Plan, PlanNode, QueryPlan};
use crate::value::{Affinity, Value};

pub fn plan_statement(catalog: &Catalog, stmt: &ast::Statement) -> Result<Plan> {
    match stmt {
        ast::Statement::CreateTable(create) => plan_create_table(create),
        ast::Statement::Insert(insert) => plan_insert(catalog, insert),
        ast::Statement::Query(query) => plan_query(catalog, query).map(Plan::Query),
        other => unsupported(statement_kind(other)),
    }
}

fn plan_create_table(create: &ast::CreateTable) -> Result<Plan> {
    if create.query.is_some() {
        return unsupported("CREATE TABLE ... AS SELECT");
    }
    if !create.constraints.is_empty() {
        return unsupported("table constraints");
    }
    if create.columns.is_empty() {
        return Err(Error::Parse("a table needs at least one column".into()));
    }
    let name = object_name(&create.name)?;
    let mut columns: Vec<Column> = Vec::new();
    for def in &create.columns {
        let declared_type = match def.data_type {
            ast::DataType::Unspecified => String::new(),
            ref t => t.to_string(),
        };
        let mut not_null = false;
        for option in &def.options {
            match option.option {
                ast::ColumnOption::NotNull => not_null = true,
                ast::ColumnOption::Null => {}
                ref other => return unsupported(format!("column constraint {other}")),
            }
        }
        if columns
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(&def.name.value))
        {
            return Err(Error::Catalog(format!(
                "duplicate column name: {}",
                def.name.value
            )));
        }
        columns.push(Column {
            name: def.name.value.clone(),
            affinity: Affinity::from_declared_type(&declared_type),
            declared_type,
            not_null,
        });
    }
    Ok(Plan::CreateTable {
        schema: TableSchema { name, columns },
        if_not_exists: create.if_not_exists,
    })
}

fn plan_insert(catalog: &Catalog, insert: &ast::Insert) -> Result<Plan> {
    if insert.or.is_some() || insert.on.is_some() || insert.returning.is_some() {
        return unsupported("INSERT OR / ON CONFLICT / RETURNING");
    }
    let ast::TableObject::TableName(table_name) = &insert.table else {
        return unsupported("INSERT INTO a table function");
    };
    let schema = catalog.table(&object_name(table_name)?)?;
    let targets = if insert.columns.is_empty() {
        (0..schema.columns.len()).collect()
    } else {
        let mut targets = Vec::new();
        for column in &insert.columns {
            let name = object_name(column)?;
            let index = schema.column_index(&name).ok_or_else(|| {
                Error::Catalog(format!("table {} has no column named {name}", schema.name))
            })?;
            targets.push(index);
        }
        targets
    };

    let Some(source) = &insert.source else {
        return unsupported("INSERT without VALUES");
    };
    let ast::SetExpr::Values(values) = source.body.as_ref() else {
        return unsupported("INSERT ... SELECT");
    };
    // VALUES rows are constants: there is no table in scope to reference.
    let scope = Scope::empty();
    let mut rows = Vec::new();
    for row in &values.rows {
        if row.content.len() != targets.len() {
            return Err(Error::Catalog(format!(
                "{} values for {} columns",
                row.content.len(),
                targets.len()
            )));
        }
        rows.push(
            row.content
                .iter()
                .map(|e| scope.bind(e))
                .collect::<Result<_>>()?,
        );
    }
    Ok(Plan::Insert {
        table: schema.name.clone(),
        targets,
        rows,
    })
}

fn plan_query(catalog: &Catalog, query: &ast::Query) -> Result<QueryPlan> {
    if query.with.is_some() {
        return unsupported("WITH (common table expressions)");
    }
    if query.order_by.is_some() {
        return unsupported("ORDER BY");
    }
    if query.limit_clause.is_some() || query.fetch.is_some() {
        return unsupported("LIMIT");
    }
    let select = match query.body.as_ref() {
        ast::SetExpr::Select(select) => select,
        ast::SetExpr::SetOperation { .. } => return unsupported("compound SELECT (UNION etc.)"),
        ast::SetExpr::Query(_) => return unsupported("parenthesized SELECT"),
        ast::SetExpr::Values(_) => return unsupported("VALUES as a query"),
        _ => return unsupported("this kind of query"),
    };
    // `SELECT ALL` is the default behavior, spelled out.
    if !matches!(select.distinct, None | Some(ast::Distinct::All)) {
        return unsupported("DISTINCT");
    }
    if !matches!(&select.group_by, ast::GroupByExpr::Expressions(e, m) if e.is_empty() && m.is_empty())
    {
        return unsupported("GROUP BY");
    }
    if select.having.is_some() {
        return unsupported("HAVING");
    }

    let (scope, mut root) = match select.from.as_slice() {
        [] => (Scope::empty(), PlanNode::OneRow),
        [from] => {
            if !from.joins.is_empty() {
                return unsupported("JOIN");
            }
            let ast::TableFactor::Table {
                name,
                alias,
                args: None,
                ..
            } = &from.relation
            else {
                return unsupported("subquery or table function in FROM");
            };
            let schema = catalog.table(&object_name(name)?)?;
            let scope = Scope {
                table: Some(schema),
                alias: alias.as_ref().map(|a| a.name.value.clone()),
            };
            let scan = PlanNode::SeqScan {
                table: schema.name.clone(),
                reason: format!("{} has no index, so every row is read", schema.name),
            };
            (scope, scan)
        }
        _ => return unsupported("JOIN (more than one table in FROM)"),
    };

    if let Some(predicate) = &select.selection {
        root = PlanNode::Filter {
            input: Box::new(root),
            predicate: scope.bind(predicate)?,
        };
    }

    let mut columns = Vec::new();
    let mut exprs = Vec::new();
    for item in &select.projection {
        match item {
            ast::SelectItem::UnnamedExpr(e) => {
                columns.push(output_name(e));
                exprs.push(scope.bind(e)?);
            }
            ast::SelectItem::ExprWithAlias { expr, alias } => {
                columns.push(alias.value.clone());
                exprs.push(scope.bind(expr)?);
            }
            ast::SelectItem::Wildcard(_) => {
                scope.expand_wildcard(None, &mut columns, &mut exprs)?
            }
            ast::SelectItem::QualifiedWildcard(
                ast::SelectItemQualifiedWildcardKind::ObjectName(name),
                _,
            ) => scope.expand_wildcard(Some(&object_name(name)?), &mut columns, &mut exprs)?,
            _ => return unsupported("this kind of select item"),
        }
    }
    Ok(QueryPlan {
        columns,
        root: PlanNode::Project {
            input: Box::new(root),
            exprs,
        },
    })
}

/// SQLite names an unaliased result column after its source text.
fn output_name(expr: &ast::Expr) -> String {
    match expr {
        ast::Expr::Identifier(ident) => ident.value.clone(),
        ast::Expr::CompoundIdentifier(parts) => {
            parts.last().map(|p| p.value.clone()).unwrap_or_default()
        }
        other => other.to_string(),
    }
}

/// The names visible to an expression: at most one table for now.
struct Scope<'a> {
    table: Option<&'a TableSchema>,
    alias: Option<String>,
}

impl<'a> Scope<'a> {
    fn empty() -> Self {
        Scope {
            table: None,
            alias: None,
        }
    }

    fn matches_qualifier(&self, qualifier: &str) -> bool {
        match (self.table, &self.alias) {
            (_, Some(alias)) => alias.eq_ignore_ascii_case(qualifier),
            (Some(table), None) => table.name.eq_ignore_ascii_case(qualifier),
            (None, None) => false,
        }
    }

    fn column(&self, qualifier: Option<&str>, name: &str) -> Result<Expr> {
        let found = self
            .table
            .filter(|_| qualifier.is_none_or(|q| self.matches_qualifier(q)));
        let index = found.and_then(|t| t.column_index(name));
        match (found, index) {
            (Some(table), Some(index)) => Ok(Expr::Column {
                index,
                name: table.columns[index].name.clone(),
            }),
            _ => {
                let full = qualifier.map_or(name.to_string(), |q| format!("{q}.{name}"));
                Err(Error::Catalog(format!("no such column: {full}")))
            }
        }
    }

    fn expand_wildcard(
        &self,
        qualifier: Option<&str>,
        columns: &mut Vec<String>,
        exprs: &mut Vec<Expr>,
    ) -> Result<()> {
        let table = match (self.table, qualifier) {
            (Some(t), None) => t,
            (Some(t), Some(q)) if self.matches_qualifier(q) => t,
            (_, Some(q)) => return Err(Error::Catalog(format!("no such table: {q}"))),
            (None, None) => return Err(Error::Catalog("no tables specified".into())),
        };
        for (index, column) in table.columns.iter().enumerate() {
            columns.push(column.name.clone());
            exprs.push(Expr::Column {
                index,
                name: column.name.clone(),
            });
        }
        Ok(())
    }

    fn bind(&self, expr: &ast::Expr) -> Result<Expr> {
        Ok(match expr {
            ast::Expr::Identifier(ident) => self.column(None, &ident.value)?,
            ast::Expr::CompoundIdentifier(parts) => match parts.as_slice() {
                [table, column] => self.column(Some(&table.value), &column.value)?,
                _ => return unsupported("schema-qualified column names"),
            },
            ast::Expr::Value(v) => Expr::Literal(literal(&v.value)?),
            ast::Expr::Nested(inner) => self.bind(inner)?,
            ast::Expr::IsNull(inner) => Expr::IsNull {
                expr: Box::new(self.bind(inner)?),
                negated: false,
            },
            ast::Expr::IsNotNull(inner) => Expr::IsNull {
                expr: Box::new(self.bind(inner)?),
                negated: true,
            },
            ast::Expr::UnaryOp { op, expr } => {
                let op = match op {
                    ast::UnaryOperator::Minus => UnaryOp::Negate,
                    ast::UnaryOperator::Plus => UnaryOp::Plus,
                    ast::UnaryOperator::Not => UnaryOp::Not,
                    other => return unsupported(format!("unary operator {other}")),
                };
                Expr::Unary {
                    op,
                    expr: Box::new(self.bind(expr)?),
                }
            }
            ast::Expr::BinaryOp { left, op, right } => Expr::Binary {
                op: binary_op(op)?,
                left: Box::new(self.bind(left)?),
                right: Box::new(self.bind(right)?),
            },
            other => return unsupported(expr_kind(other)),
        })
    }
}

fn binary_op(op: &ast::BinaryOperator) -> Result<BinaryOp> {
    use ast::BinaryOperator as B;
    Ok(match op {
        B::Plus => BinaryOp::Add,
        B::Minus => BinaryOp::Subtract,
        B::Multiply => BinaryOp::Multiply,
        B::Divide => BinaryOp::Divide,
        B::Modulo => BinaryOp::Remainder,
        B::StringConcat => BinaryOp::Concat,
        B::Eq => BinaryOp::Eq,
        B::NotEq => BinaryOp::NotEq,
        B::Lt => BinaryOp::Lt,
        B::LtEq => BinaryOp::LtEq,
        B::Gt => BinaryOp::Gt,
        B::GtEq => BinaryOp::GtEq,
        B::And => BinaryOp::And,
        B::Or => BinaryOp::Or,
        other => return unsupported(format!("operator {other}")),
    })
}

fn literal(value: &ast::Value) -> Result<Value> {
    Ok(match value {
        ast::Value::Null => Value::Null,
        ast::Value::Boolean(b) => Value::Integer(i64::from(*b)),
        ast::Value::SingleQuotedString(s) => Value::Text(s.clone()),
        ast::Value::Number(text, _) => {
            let is_integer = text.bytes().all(|b| b.is_ascii_digit());
            match text.parse::<i64>() {
                Ok(i) if is_integer => Value::Integer(i),
                // Integers too large for 64 bits become reals, as in SQLite.
                _ => Value::Real(
                    text.parse()
                        .map_err(|_| Error::Parse(format!("bad number {text}")))?,
                ),
            }
        }
        other => return unsupported(format!("literal {other}")),
    })
}

fn object_name(name: &ast::ObjectName) -> Result<String> {
    match name.0.as_slice() {
        [ast::ObjectNamePart::Identifier(ident)] => Ok(ident.value.clone()),
        _ => unsupported(format!("qualified name {name}")),
    }
}

/// A short, stable label for an unsupported expression, such as "CASE" or
/// "function call". The sqllogictest report groups failures by this label.
fn expr_kind(expr: &ast::Expr) -> String {
    match expr {
        ast::Expr::Function(_) => "function call".into(),
        ast::Expr::Case { .. } => "CASE".into(),
        ast::Expr::Between { .. } => "BETWEEN".into(),
        ast::Expr::InList { .. } => "IN (list)".into(),
        ast::Expr::InSubquery { .. } => "IN (subquery)".into(),
        ast::Expr::Subquery(_) => "scalar subquery".into(),
        ast::Expr::Exists { .. } => "EXISTS".into(),
        ast::Expr::Cast { .. } => "CAST".into(),
        ast::Expr::Like { .. } | ast::Expr::ILike { .. } => "LIKE".into(),
        ast::Expr::Collate { .. } => "COLLATE".into(),
        ast::Expr::IsDistinctFrom(..) | ast::Expr::IsNotDistinctFrom(..) => {
            "IS [NOT] DISTINCT FROM".into()
        }
        ast::Expr::IsTrue(_)
        | ast::Expr::IsFalse(_)
        | ast::Expr::IsNotTrue(_)
        | ast::Expr::IsNotFalse(_) => "IS TRUE / IS FALSE".into(),
        _ => "this kind of expression".into(),
    }
}

fn statement_kind(stmt: &ast::Statement) -> String {
    let text = stmt.to_string();
    let words: Vec<&str> = text.split_whitespace().take(2).collect();
    match words.as_slice() {
        [first, second] if ["CREATE", "DROP", "ALTER"].contains(first) => {
            format!("{first} {second}")
        }
        [first, ..] => first.to_string(),
        [] => "empty statement".into(),
    }
}
