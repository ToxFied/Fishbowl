//! The SQL dialect Fishbowl parses: `sqlparser`'s SQLite dialect, with the
//! bit operators SQLite has and the parser's SQLite dialect leaves out.
//!
//! SQLite gives `&`, `|`, `<<` and `>>` one shared precedence, between `+`
//! and the comparisons, and reads them left to right, so `4 | 6 & 3` is
//! `(4 | 6) & 3`. Unary `~` binds tighter than any binary operator, so
//! `~1 * 2` is `(~1) * 2`. The parser's defaults differ on both counts.
//! See <https://www.sqlite.org/lang_expr.html#operators_and_parse_affecting_attributes>.

use std::any::TypeId;

use sqlparser::ast::{Expr, Statement, UnaryOperator};
use sqlparser::dialect::{Dialect, Precedence, SQLiteDialect};
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

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
        crate::trigger::parse_create(parser).or_else(|| self.0.parse_statement(parser))
    }

    fn parse_infix(
        &self,
        parser: &mut Parser,
        expr: &Expr,
        precedence: u8,
    ) -> Option<Result<Expr, ParserError>> {
        self.0.parse_infix(parser, expr, precedence)
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
