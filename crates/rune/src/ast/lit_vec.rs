use crate::ast;
use crate::{Parse, ParseError, Parser};
use runestick::Span;

/// A number literal.
#[derive(Debug, Clone)]
pub struct LitVec {
    /// The open bracket.
    pub open: ast::OpenBracket,
    /// Items in the array.
    pub items: Vec<ast::Expr>,
    /// The close bracket.
    pub close: ast::CloseBracket,
    /// If the entire array is constant.
    is_const: bool,
}

into_tokens!(LitVec { open, items, close });

impl LitVec {
    /// Access the span of the expression.
    pub fn span(&self) -> Span {
        self.open.span().join(self.close.span())
    }

    /// Test if the entire expression is constant.
    pub fn is_const(&self) -> bool {
        self.is_const
    }
}

/// Parse an array literal.
///
/// # Examples
///
/// ```rust
/// use rune::{parse_all, ast};
///
/// parse_all::<ast::LitVec>("[1, \"two\"]").unwrap();
/// parse_all::<ast::LitVec>("[1, 2,]").unwrap();
/// parse_all::<ast::LitVec>("[1, 2, foo()]").unwrap();
/// ```
impl Parse for LitVec {
    fn parse(parser: &mut Parser) -> Result<Self, ParseError> {
        let open = parser.parse()?;

        let mut items = Vec::new();
        let mut is_const = true;

        while !parser.peek::<ast::CloseBracket>()? {
            let expr = parser.parse::<ast::Expr>()?;

            if !expr.is_const() {
                is_const = false;
            }

            items.push(expr);

            if parser.peek::<ast::Comma>()? {
                parser.parse::<ast::Comma>()?;
            } else {
                break;
            }
        }

        let close = parser.parse()?;
        Ok(Self {
            open,
            items,
            close,
            is_const,
        })
    }
}
