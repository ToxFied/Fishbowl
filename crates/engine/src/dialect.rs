//! The SQL dialect Fishbowl parses: `sqlparser`'s SQLite dialect, with the
//! bit operators SQLite has and the parser's SQLite dialect leaves out.
//!
//! SQLite gives `&`, `|`, `<<` and `>>` one shared precedence, between `+`
//! and the comparisons, and reads them left to right, so `4 | 6 & 3` is
//! `(4 | 6) & 3`. Unary `~` binds tighter than any binary operator, so
//! `~1 * 2` is `(~1) * 2`. The parser's defaults differ on both counts.
//! See <https://www.sqlite.org/lang_expr.html#operators_and_parse_affecting_attributes>.
//!
//! It also reads SQLite's `x IN t1` shorthand as `x IN (SELECT * FROM t1)`,
//! `REINDEX`, and the index hints `INDEXED BY` and `NOT INDEXED`.

use std::any::TypeId;

use sqlparser::ast::{Expr, Statement, UnaryOperator, VacuumStatement};
use sqlparser::dialect::{Dialect, Precedence, SQLiteDialect};
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer};

/// SQLite's dialect plus `<<`, `>>` and SQLite's operator precedence. Every
/// method SQLite's dialect overrides is passed on to it, and the parser's
/// "is this SQLite?" checks still see SQLite.
#[derive(Debug, Default)]
pub struct FishbowlDialect(SQLiteDialect);

impl Dialect for FishbowlDialect {
    fn dialect(&self) -> TypeId {
        TypeId::of::<SQLiteDialect>()
    }

    fn supports_bitwise_shift_operators(&self) -> bool {
        true
    }

    fn get_next_precedence(&self, parser: &Parser) -> Option<Result<u8, ParserError>> {
        match parser.peek_token().token {
            Token::Ampersand | Token::Pipe | Token::ShiftLeft | Token::ShiftRight => {
                Some(Ok(self.prec_value(Precedence::Ampersand)))
            }
            _ => None,
        }
    }

    fn parse_prefix(&self, parser: &mut Parser) -> Option<Result<Expr, ParserError>> {
        if parser.peek_token().token != Token::Tilde {
            return None;
        }
        parser.next_token();
        // Only operators that bind tighter than `*` and `||` join the operand.
        let operand = parser.parse_subexpr(self.prec_value(Precedence::MulDivModOp));
        Some(operand.map(|expr| Expr::UnaryOp {
            op: UnaryOperator::BitwiseNot,
            expr: Box::new(expr),
        }))
    }

    fn is_delimited_identifier_start(&self, ch: char) -> bool {
        self.0.is_delimited_identifier_start(ch)
    }

    fn identifier_quote_style(&self, identifier: &str) -> Option<char> {
        self.0.identifier_quote_style(identifier)
    }

    fn is_identifier_start(&self, ch: char) -> bool {
        self.0.is_identifier_start(ch)
    }

    fn is_identifier_part(&self, ch: char) -> bool {
        self.0.is_identifier_part(ch)
    }

    fn supports_filter_during_aggregation(&self) -> bool {
        self.0.supports_filter_during_aggregation()
    }

    fn supports_start_transaction_modifier(&self) -> bool {
        self.0.supports_start_transaction_modifier()
    }

    fn parse_statement(&self, parser: &mut Parser) -> Option<Result<Statement, ParserError>> {
        if parser.parse_keyword(Keyword::REINDEX) {
            return Some(parse_reindex(parser));
        }
        crate::trigger::parse_create(parser).or_else(|| self.0.parse_statement(parser))
    }

    /// Lets the parser read the hints [`tokenize`] writes.
    fn supports_table_hints(&self) -> bool {
        true
    }

    /// Keeps the `FORCE` or `IGNORE` of a hint from being read as an alias.
    fn is_table_factor_alias(&self, explicit: bool, kw: &Keyword, parser: &mut Parser) -> bool {
        let hint =
            matches!(kw, Keyword::FORCE | Keyword::IGNORE) && parser.peek_keyword(Keyword::INDEX);
        explicit || (self.is_table_alias(kw, parser) && !hint)
    }

    fn parse_infix(
        &self,
        parser: &mut Parser,
        expr: &Expr,
        precedence: u8,
    ) -> Option<Result<Expr, ParserError>> {
        self.parse_in_table(parser, expr)
            .or_else(|| self.0.parse_infix(parser, expr, precedence))
    }

    fn supports_in_empty_list(&self) -> bool {
        self.0.supports_in_empty_list()
    }

    fn supports_limit_comma(&self) -> bool {
        self.0.supports_limit_comma()
    }

    fn supports_asc_desc_in_column_definition(&self) -> bool {
        self.0.supports_asc_desc_in_column_definition()
    }

    fn supports_dollar_placeholder(&self) -> bool {
        self.0.supports_dollar_placeholder()
    }

    fn supports_notnull_operator(&self) -> bool {
        self.0.supports_notnull_operator()
    }

    fn supports_comma_separated_trim(&self) -> bool {
        self.0.supports_comma_separated_trim()
    }

    fn supports_numeric_literal_underscores(&self) -> bool {
        self.0.supports_numeric_literal_underscores()
    }
}

impl FishbowlDialect {
    /// `x [NOT] IN name`, SQLite's shorthand for `x [NOT] IN (SELECT * FROM
    /// name)` with a table or view. As in SQLite, the table must have one
    /// column; the planner checks that like any other subquery.
    fn parse_in_table(
        &self,
        parser: &mut Parser,
        expr: &Expr,
    ) -> Option<Result<Expr, ParserError>> {
        let negated = parser.peek_keyword(Keyword::NOT);
        let in_at = usize::from(negated);
        if !matches!(parser.peek_nth_token(in_at).token, Token::Word(w) if w.keyword == Keyword::IN)
            || !matches!(parser.peek_nth_token(in_at + 1).token, Token::Word(_))
        {
            return None;
        }
        for _ in 0..=in_at {
            parser.next_token();
        }
        let subquery = parser.parse_object_name(false).and_then(|name| {
            Parser::new(self)
                .try_with_sql(&format!("SELECT * FROM {name}"))?
                .parse_query()
        });
        Some(subquery.map(|subquery| Expr::InSubquery {
            expr: Box::new(expr.clone()),
            subquery,
            negated,
        }))
    }
}

/// `REINDEX [name]`. The parser has no node for SQLite's REINDEX, so this
/// borrows the one for Redshift's `VACUUM REINDEX [table]`, which means the
/// same thing.
fn parse_reindex(parser: &mut Parser) -> Result<Statement, ParserError> {
    let table_name = match parser.peek_token().token {
        Token::EOF | Token::SemiColon => None,
        _ => Some(parser.parse_object_name(false)?),
    };
    Ok(Statement::Vacuum(VacuumStatement {
        full: false,
        sort_only: false,
        delete_only: false,
        reindex: true,
        recluster: false,
        table_name,
        threshold: None,
        boost: false,
    }))
}

/// Splits `sql` into tokens and rewrites SQLite's index hints, which the
/// parser does not know, into the MySQL ones it does: `INDEXED BY i` becomes
/// `FORCE INDEX (i)` and `NOT INDEXED` becomes `IGNORE INDEX ()`. Working on
/// tokens leaves strings and comments alone. A hint must follow a table name
/// or alias, so `WHERE NOT indexed` still reads a column named `indexed`.
pub fn tokenize(dialect: &FishbowlDialect, sql: &str) -> Result<Vec<TokenWithSpan>, ParserError> {
    let tokens = Tokenizer::new(dialect, sql).tokenize_with_location()?;
    // An unquoted word, in any case.
    let word = |i: usize, text: &str| match &tokens[i].token {
        Token::Word(w) => w.quote_style.is_none() && w.value.eq_ignore_ascii_case(text),
        _ => false,
    };
    // A table name or alias: quoted, or a word that is not a keyword.
    let is_name = |token: &Token| match token {
        Token::Word(w) => w.quote_style.is_some() || w.keyword == Keyword::NoKeyword,
        _ => false,
    };
    let next = |i: usize| {
        (i + 1..tokens.len()).find(|&j| !matches!(tokens[j].token, Token::Whitespace(_)))
    };
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let after_name = out
            .iter()
            .rfind(|t| !matches!(t.token, Token::Whitespace(_)))
            .is_some_and(|t| is_name(&t.token));
        let span = tokens[i].span;
        let mut emit = |token: Token| out.push(TokenWithSpan::new(token, span));
        if after_name && word(i, "INDEXED") {
            if let Some(by) = next(i).filter(|&j| word(j, "BY"))
                && let Some(name) = next(by).filter(|&j| matches!(tokens[j].token, Token::Word(_)))
            {
                emit(Token::make_keyword("FORCE"));
                emit(Token::make_keyword("INDEX"));
                emit(Token::LParen);
                emit(tokens[name].token.clone());
                emit(Token::RParen);
                i = name + 1;
                continue;
            }
        } else if after_name
            && word(i, "NOT")
            && let Some(indexed) = next(i).filter(|&j| word(j, "INDEXED"))
        {
            emit(Token::make_keyword("IGNORE"));
            emit(Token::make_keyword("INDEX"));
            emit(Token::LParen);
            emit(Token::RParen);
            i = indexed + 1;
            continue;
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    Ok(out)
}
