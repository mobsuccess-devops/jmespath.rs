//! Module for parsing JMESPath expressions into an AST.

extern crate rustc_serialize;

use std::fmt;
use std::iter::Peekable;

use ast::{Ast, KeyValuePair, Comparator};
use lexer::{Lexer, Token};

/// An alias for computations that can return an `Ast` or `ParseError`.
pub type ParseResult = Result<Ast, ParseError>;
type ParseStep = Result<Token, ParseError>;

/// Parses a JMESPath expression into an AST
pub fn parse(expr: &str) -> ParseResult {
    Parser::new(expr).parse()
}

/// Encountered when an invalid JMESPath expression is parsed.
#[derive(Clone, PartialEq, Debug)]
pub struct ParseError {
    /// The error message.
    msg: String,
    /// The line number of the error.
    line: usize,
    /// The column of the error.
    col: usize,
}

impl ParseError {
    fn new(expr: &str, pos: usize, msg: String) -> ParseError {
        // Find each new line and create a formatted error message.
        let mut line: usize = 0;
        let mut col: usize = 0;
        let mut buff = String::new();
        for l in expr.lines().collect::<Vec<&str>>() {
            buff.push_str(l);
            buff.push('\n');
            if buff.len() > pos {
                col = match line {
                    0 => pos,
                    _ => buff.len().checked_sub(2 + pos).unwrap_or(0)
                };
                ParseError::inject_err_pointer(&mut buff, col);
                break;
            }
            line += 1;
        }
        ParseError {
            msg: format!("Parse error at line {}, col {}; {}\n{}", line, col, msg, buff),
            line: line,
            col: col
        }
    }

    fn inject_err_pointer(string_buffer: &mut String, col: usize) {
        let span = (0..col).map(|_| ' ').collect::<String>();
        string_buffer.push_str(&span);
        string_buffer.push_str(&"^\n");
    }
}

/// Operators are pushed onto the operator stack.
#[derive(Debug, PartialEq)]
enum Operator {
    Basic(Token),
    Function(String, Vec<Ast>),
    MultiList(Vec<Ast>),
    MultiHash(Vec<Ast>),
    ArrayProjection,
    FilterProjection(Option<Ast>),
    SliceProjection(bool, Option<i32>, Option<i32>, Option<i32>)
}

impl fmt::Display for Operator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Operator::Basic(ref tok) => write!(f, "{}", tok.lexeme()),
            &Operator::Function(ref name, _) => write!(f, "{} function", name),
            &Operator::MultiList(_) => write!(f, "multi-list"),
            &Operator::MultiHash(_) => write!(f, "multi-hash"),
            &Operator::ArrayProjection => write!(f, "[*]"),
            &Operator::SliceProjection(_, _, _, _) => write!(f, "slice-projection"),
            &Operator::FilterProjection(_) => write!(f, "filter-projection")
        }
    }
}

impl Operator {
    /// Returns true if the current operator has a precedence < give.
    /// This function takes operator associativity into account. Left
    /// associative operators check with <, while right associative check
    /// with <=.
    #[inline]
    pub fn has_lower_precedence(&self, precedence: usize) -> bool {
        if self.is_right_associative() {
            self.precedence() < precedence
        } else {
            self.precedence() <= precedence
        }
    }

    /// Returns true if the operator is right associative.
    #[inline]
    pub fn is_right_associative(&self) -> bool {
        match self {
            // Projections are right associative.
            &Operator::Basic(Token::Star)
                | &Operator::ArrayProjection
                | &Operator::SliceProjection(_, _, _, _)
                | &Operator::FilterProjection(_) => true,
            // Left associative.
            _ => false
        }
    }

    /// Retrieves the precedence of an operator.
    #[inline]
    pub fn precedence(&self) -> usize {
        match self {
            &Operator::Basic(ref p) => p.precedence(),
            // Projections all share the "*" precedence.
            &Operator::ArrayProjection
                | &Operator::FilterProjection(Some(_))
                | &Operator::SliceProjection(_,_, _, _) => Token::Star.precedence(),
            &Operator::MultiList(_) => Token::Lbracket.precedence(),
            &Operator::MultiHash(_) => Token::Lbrace.precedence(),
            // These should never be popped by other operators. Only by closing characters.
            _ => 0
        }
    }

    #[inline]
    pub fn is_binary(&self) -> bool {
        match self {
            &Operator::Basic(ref p) if p == &Token::Ampersand => false,
            &Operator::Basic(ref p) if p == &Token::Not => false,
            &Operator::SliceProjection(is_binary, _, _, _) => is_binary,
            _ => true
        }
    }

    // Returns true if the token closes the passed in token.
    pub fn closes(&self, token: &Token) -> bool {
        match self {
            &Operator::Basic(ref p) if p == &Token::Lparen && token == &Token::Rparen => true,
            &Operator::Basic(ref p) if p == &Token::Lbrace && token == &Token::Rbrace => true,
            &Operator::Function(_, _) if token == &Token::Rparen => true,
            &Operator::FilterProjection(None) if token == &Token::Rbracket => true,
            &Operator::MultiList(_) if token == &Token::Rbracket => true,
            &Operator::MultiHash(_) if token == &Token::Rbrace => true,
            _ => false
        }
    }

    // Returns true if the operator is built up using the provided token separator.
    pub fn supports_token_separator(&self, token: &Token) -> bool {
        match self {
            // Support only commas.
            &Operator::Function(_, _)
                | &Operator::MultiList(_) => token == &Token::Comma,
            // Supports commas and colons.
            &Operator::MultiHash(_) => {
                match token {
                    &Token::Comma | &Token::Colon => true,
                    _ => false
                }
            },
            _ => false
        }
    }
}

/// Influences how to parse tokens and which tokens are allowed.
enum ParserState {
    NeedOperand,
    HasOperand
}

/// JMESPath parser. Returns an Ast
pub struct Parser<'a> {
    /// Token stream
    stream: Peekable<Lexer<'a>>,
    /// Expression being parsed
    expr: String,
    /// The current character offset in the expression
    pos: usize,
    /// Operand queue containing AST nodes.
    output_queue: Vec<Ast>,
    /// Operator stack containing operator states.
    operator_stack: Vec<Operator>,
    /// Current state of parser. Determines acceptable tokens and behavior.
    parser_state: ParserState
}

impl<'a> Parser<'a> {
    // Constructs a new lexer using the given expression string.
    pub fn new(expr: &'a str) -> Parser<'a> {
        Parser {
            stream: Lexer::new(expr).peekable(),
            expr: expr.to_string(),
            pos: 0,
            operator_stack: vec!(),
            output_queue: vec!(),
            parser_state: ParserState::NeedOperand
        }
    }

    /// Parses the expression into result containing an AST or ParseError.
    pub fn parse(&mut self) -> ParseResult {
        let mut token = self.advance();

        loop {
            token = match self.parser_state {
                ParserState::NeedOperand => try!(self.need_operand_state(token)),
                ParserState::HasOperand => try!(self.has_operand_state(token))
            };
            if token == Token::Eof {
                break;
            }
        }

        // Pop and process any remaining operators on the stack.
        while !self.operator_stack.is_empty() {
            try!(self.pop_token());
        }

        if self.stream.next().is_some() {
            Err(self.err(None, &"Did not reach token stream EOF"))
        } else {
            Ok(self.output_queue.pop().unwrap())
        }
    }

    #[inline]
    fn need_operand_state(&mut self, token: Token) -> ParseStep {
        match token {
            Token::Identifier(value) => {
                self.output_queue.push(Ast::Identifier(value));
                self.parser_state = ParserState::HasOperand;
                Ok(self.advance())
            },
            Token::Literal(value) => {
                self.output_queue.push(Ast::Literal(value));
                self.parser_state = ParserState::HasOperand;
                Ok(self.advance())
            },
            Token::QuotedIdentifier(ref value) => {
                self.parser_state = ParserState::HasOperand;
                let next_token = self.advance();
                if let Token::Lparen = next_token {
                    Err(self.err(None, &"Quoted strings can't be function names"))
                } else {
                    self.output_queue.push(Ast::Identifier(value.clone()));
                    Ok(next_token)
                }
            },
            Token::Lbracket => self.open_lbracket(true),
            Token::At => {
                self.output_queue.push(Ast::CurrentNode);
                Ok(self.advance())
            },
            Token::Star => {
                self.output_queue.push(Ast::CurrentNode);
                let next_token = self.advance();
                self.projection_rhs(next_token, Operator::Basic(Token::Star))
            },
            Token::Flatten => {
                self.output_queue.push(Ast::CurrentNode);
                let next_token = self.advance();
                self.projection_rhs(next_token, Operator::Basic(Token::Flatten))
            },
            Token::Filter => {
                self.output_queue.push(Ast::CurrentNode);
                self.open_filter()
            },
            Token::Lbrace => {
                let next_token = self.advance();
                self.push_operator(next_token, Operator::MultiHash(vec!()))
                    .and_then(|next_token| self.open_multi_hash_key(next_token))
            },
            Token::Ampersand => {
                let next_token = self.advance();
                self.push_operator(next_token, Operator::Basic(Token::Ampersand))
            },
            ref tok @ _ => Err(self.err(Some(tok), &format!("Unexpected prefix token: {}",
                                                            tok.token_name()))),
        }
    }

    #[inline]
    fn has_operand_state(&mut self, token: Token) -> ParseStep {
        // More easily advance and push a basic operator.
        macro_rules! next_op {
            ($x:expr) => {{
                let next_token = self.advance();
                self.parser_state = ParserState::NeedOperand;
                self.push_operator(next_token, Operator::Basic($x))
            }};
        }
        match token {
            Token::Dot => {
                match self.stream.peek() {
                    Some(&(_, Token::Star)) => {
                        self.advance();
                        let next_token = self.advance();
                        self.projection_rhs(next_token, Operator::Basic(Token::Star))
                    },
                    _ => self.parse_dot(Operator::Basic(Token::Dot))
                }
            },
            Token::Flatten => {
                let next_token = self.advance();
                self.projection_rhs(next_token, Operator::Basic(Token::Flatten))
            },
            Token::Filter => self.open_filter(),
            Token::Lbracket => self.open_lbracket(false),
            Token::Or => next_op!(Token::Or),
            Token::Pipe => next_op!(Token::Pipe),
            Token::Lt => next_op!(Token::Lt),
            Token::Lte => next_op!(Token::Lte),
            Token::Gt => next_op!(Token::Gt),
            Token::Gte => next_op!(Token::Gte),
            Token::Eq => next_op!(Token::Eq),
            Token::Ne => next_op!(Token::Ne),
            Token::Lparen => {
                match self.output_queue.pop() {
                    // A "(" preceded by an identifier means that it is a function call.
                    Some(Ast::Identifier(fn_name)) => self.open_function(fn_name),
                    _ => Err(self.err(None, &format!("Unexpected parenthesis"))),
                }
            },
            Token::Rparen => self.closing_token(token, |p: &mut Self, op: Operator| {
                match op {
                    Operator::Function(name, mut args) => {
                        args.push(p.output_queue.pop().unwrap());
                        p.output_queue.push(Ast::Function(name, args));
                        Some(Ok(p.advance()))
                    },
                    // TODO: Implement simple precedence parens? Needs a JEP.
                    _ => None
                }
            }),
            Token::Rbracket => self.closing_token(token, |p: &mut Self, op: Operator| {
                match op {
                    Operator::MultiList(mut args) => {
                        args.push(p.output_queue.pop().unwrap());
                        p.output_queue.push(Ast::MultiList(args));
                        Some(Ok(p.advance()))
                    },
                    Operator::FilterProjection(None) => {
                        let filter_expr = p.output_queue.pop().unwrap();
                        let next_token = p.advance();
                        Some(p.projection_rhs(next_token,
                                              Operator::FilterProjection(Some(filter_expr))))
                    },
                    _ => None
                }
            }),
            Token::Rbrace => self.closing_token(token, |p: &mut Self, op: Operator| {
                match op {
                    Operator::MultiHash(mut args) => {
                        args.push(p.output_queue.pop().unwrap());
                        // If there are an odd number of values, then there is a mis-match.
                        if args.len() % 2 == 1 {
                            Some(Err(p.err(Some(&Token::Rbrace), "Unbalanced key value pairs")))
                        } else {
                            let mut kvps: Vec<KeyValuePair> = vec!();
                            while args.len() > 0 {
                                kvps.insert(0, KeyValuePair {value: args.pop().unwrap(),
                                                             key: args.pop().unwrap()});
                            }
                            p.output_queue.push(Ast::MultiHash(kvps));
                            Some(Ok(p.advance()))
                        }
                    },
                    _ => None
                }
            }),
            Token::Comma => self.separator_token(Token::Comma, |p: &mut Self, op: Operator| {
                match op {
                    Operator::Function(fn_name, mut args) => {
                        args.push(p.output_queue.pop().unwrap());
                        p.operator_stack.push(Operator::Function(fn_name, args));
                        Some(Ok(p.advance()))
                    },
                    Operator::MultiList(mut args) => {
                        args.push(p.output_queue.pop().unwrap());
                        p.operator_stack.push(Operator::MultiList(args));
                        Some(Ok(p.advance()))
                    },
                    Operator::MultiHash(mut args) => {
                        args.push(p.output_queue.pop().unwrap());
                        p.operator_stack.push(Operator::MultiHash(args));
                        let next_token = p.advance();
                        Some(p.open_multi_hash_key(next_token))
                    },
                    // Error when a comma is inside a precedence parens: e.g., "(a || b, c) | d"
                    _ => None
                }
            }),
            Token::Colon => self.separator_token(Token::Colon, |p: &mut Self, op: Operator| {
                match op {
                    Operator::MultiHash(mut args) => {
                        // Cannot have an uneven number when adding a key.
                        if args.len() % 1 == 1 {
                            None
                        } else {
                            args.push(p.output_queue.pop().unwrap());
                            p.operator_stack.push(Operator::MultiHash(args));
                            Some(Ok(p.advance()))
                        }
                    },
                    _ => None
                }
            }),
            _ => Err(self.err(Some(&token), &format!("Unexpected postfix token: {}",
                                                     token.token_name()))),
        }
    }

    // Starts a filter projection and ensures that the next token is a valid nud token.
    #[inline]
    fn open_filter(&mut self) -> ParseStep {
        let next_token = self.advance();
        self.parser_state = ParserState::NeedOperand;
        self.push_operator(next_token, Operator::FilterProjection(None))
    }

    // Opens a function expression, ensuring that functions that are immediately closed are not
    // pushed onto the operator stack as it would try to consume an argument token which would
    // not exist.
    #[inline]
    fn open_function(&mut self, fn_name: String) -> ParseStep {
        match self.advance() {
            Token::Rparen => {
                self.output_queue.push(Ast::Function(fn_name, vec!()));
                self.parser_state = ParserState::HasOperand;
                Ok(self.advance())
            },
            next_token @ _ => {
                self.parser_state = ParserState::NeedOperand;
                self.push_operator(next_token, Operator::Function(fn_name, vec!()))
            }
        }
    }

    // Advances to the next token and ensures that the multi-hash key is an identifier.
    #[inline]
    fn open_multi_hash_key(&mut self, next_token: Token) -> ParseStep {
        match next_token {
            Token::Identifier(_) | Token::QuotedIdentifier(_) => Ok(next_token),
            _ => Err(self.err(Some(&next_token), &"Expected identifier for multi-hash key"))
        }
    }

    // Opens a "[", accounting for [*], [a, b], [1], [:]
    #[inline]
    fn open_lbracket(&mut self, is_nud: bool) -> ParseStep {
        // Skip the bracket "["
        let token = self.advance();
        match self.stream.peek() {
            // If the next token is a closing "]", then we know exactly what it is.
            Some(&(_, Token::Rbracket)) => {
                match token {
                    // Example: [1]
                    Token::Number(value) => {
                        if is_nud {
                            self.output_queue.push(Ast::Index(value));
                        } else {
                            let lhs = self.output_queue.pop().unwrap();
                            self.output_queue.push(
                                Ast::Subexpr(Box::new(lhs), Box::new(Ast::Index(value))));
                        }
                        // Skip the "]" token.
                        self.advance();
                        self.parser_state = ParserState::HasOperand;
                        Ok(self.advance())
                    },
                    // Example: [*]
                    Token::Star => {
                        if is_nud {
                            self.output_queue.push(Ast::CurrentNode);
                        }
                        // Skip the "]" token.
                        self.advance();
                        let next_token = self.advance();
                        self.projection_rhs(next_token, Operator::ArrayProjection)
                    },
                    // Example: [:]
                    Token::Colon => self.parse_slice(!is_nud, token),
                    // Everything else is invalid
                    _ => Err(self.err(Some(&token), "Expected number, ':', or '*'")),
                }
            },
            // Example: [1:], [::-1]
            Some(&(_, Token::Number(_))) | Some(&(_, Token::Colon)) =>
                self.parse_slice(!is_nud, token),
            // Everything else is a multi-list.
            _ => {
                self.parser_state = ParserState::NeedOperand;
                self.push_operator(token, Operator::MultiList(vec!()))
            }
        }
    }

    // Parse slices. e.g., [:::], [::-1], [0:10], [0:10:-2], etc...
    fn parse_slice(&mut self, is_binary: bool, mut current_token: Token) -> ParseStep {
        let mut pos = 0;
        let mut parts = [None, None, None];
        loop {
            match current_token {
                Token::Rbracket => break,
                Token::Colon if pos == 2 =>
                    return Err(self.err(None, "Found too many colons in slice expression")),
                Token::Colon => { pos += 1; current_token = self.advance(); },
                Token::Number(value) => {
                    parts[pos] = Some(value);
                    current_token = self.advance();
                    if let Token::Number(_) = current_token {
                        return Err(self.err(Some(&current_token), "Expected ':', or ']'"))
                    }
                },
                ref t @ _ => return Err(self.err(Some(t), "Expected number, ':', or ']'"))
            }
        }
        let next_token = self.advance();
        self.projection_rhs(
            next_token, Operator::SliceProjection(is_binary, parts[0], parts[1], parts[2]))
    }

    // Prepares the parser for the right hand side of a projection.
    #[inline]
    fn projection_rhs(&mut self, token: Token, parent_operator: Operator) -> ParseStep {
        match token {
            // Skip the dot token and parse with a dot precedence (e.g.., foo.*.bar)
            Token::Dot => self.parse_dot(parent_operator),
            // Multilist and filter are valid tokens that have a precedence >= 10
            Token::Lbracket | Token::Filter => {
                self.parser_state = ParserState::NeedOperand;
                self.push_operator(token, parent_operator)
            },
            // Precedence < 10 are just parsed as. E.g., * | baz
            _ if token.precedence() < 10 => {
                self.output_queue.push(Ast::CurrentNode);
                self.parser_state = ParserState::HasOperand;
                self.push_operator(token, parent_operator)
            },
            _ => Err(self.token_err(&token))
        }
    }

    // Prepares the parser for the right hand side of a "." token.
    #[inline]
    fn parse_dot(&mut self, parent_operator: Operator) -> ParseStep {
        // Skip the "." token.
        let token = self.advance();
        match token {
            // Parse multi-list when it follows a dot. e.g., foo.[a, b]
            Token::Lbracket => {
                // Push the operator that triggered this onto the operator stack.
                self.push_operator(token, parent_operator).and_then(|_| {
                    // Parse a multi-list. Skip the "[" and push the multi-list operator.
                    let next_token = self.advance();
                    self.parser_state = ParserState::NeedOperand;
                    self.push_operator(next_token, Operator::MultiList(vec![]))
                })
            },
            // Ensure the next character is valid after the "." token.
            Token::Identifier(_)
                | Token::Star
                | Token::Lbrace
                | Token::Ampersand
                | Token::Filter =>
            {
                self.parser_state = ParserState::NeedOperand;
                self.push_operator(token, parent_operator)
            },
            _ => Err(self.err(Some(&token), &format!("Expected an identifier, '*', '{{', '[', \
                                                     '@', or '[?', found {}", token.token_name())))
        }
    }

    // Returns true if the last operator has a greater precedence than the provided token.
    // Note: This is only its own method to play nice with the borrow checker.
    #[inline]
    fn does_last_have_greater_precedence(&self, op: &Operator) -> bool {
        match self.operator_stack.last() {
            Some(operator) if op.has_lower_precedence(operator.precedence()) => true,
            _ => false
        }
    }

    // Returns true if the last operator is "closed by" the provided token.
    // Note: This is only its own method to play nice with the borrow checker.
    #[inline]
    fn does_last_operator_close(&self, token: &Token) -> bool {
        match self.operator_stack.last() {
            Some(operator) if operator.closes(token) => true,
            _ => false
        }
    }

    // Returns true if the last operator is enumerated and supported token concatenation.
    // Note: This is only its own method to play nice with the borrow checker.
    #[inline]
    fn does_last_support(&self, token: &Token) -> bool {
        match self.operator_stack.last() {
            Some(operator) if operator.supports_token_separator(token) => true,
            _ => false
        }
    }

    // Adds an operator to the operator_stack.
    //
    // Any operators that have a greater precedence than the provided operator are popped from
    // the operator stack and processed, meaningbinary operators pop two output_queue values and
    // unary tokens pop one.
    #[inline]
    fn push_operator(&mut self, token: Token, operator: Operator) -> ParseStep {
        // Pop things from the top of the operator stack that have a higher precedence.
        while self.does_last_have_greater_precedence(&operator) {
            try!(self.pop_token());
        }
        self.operator_stack.push(operator);
        Ok(token)
    }

    // Pops operators until the operator at the top of the stack is closed by the provided token.
    //
    // If no matching opened operator is found, it means the token is misplaced. If an opened
    // operator is found but not what we were expecting (enforced with the closure), then the
    // closing character is unbalanced in relation to other closing characters.
    #[inline]
    fn closing_token<F>(&mut self, token: Token, on_match: F) -> ParseStep
        where F: Fn(&mut Self, Operator) -> Option<ParseStep>
    {
        while !self.operator_stack.is_empty() {
            if !self.does_last_operator_close(&token) {
                // Keep popping operators off the operator stack and onto output stack.
                try!(self.pop_token());
            } else {
                let last_operator = self.operator_stack.pop().unwrap();
                // Ensure that the operator that was popped is our desired match.
                if let Some(t) = on_match(self, last_operator) {
                    return t;
                }
                break;
            }
        }
        Err(self.err(Some(&token), &format!("Unbalanced {:?}", token)))
    }

    /// When a comma/colon/etc. is encountered, we pop from the operator stack until the operator
    /// at the top of the stack is an operator that accepts the token (e.g., function or
    /// multi-list). We then add the value at the top of the output stack to the operator that
    /// accepts mutliple values. This value is popped and then added back to the operator stack
    /// after pushing the value.
    #[inline]
    fn separator_token<F>(&mut self, token: Token, on_match: F) -> ParseStep
        where F: Fn(&mut Self, Operator) -> Option<ParseStep>
    {
        self.parser_state = ParserState::NeedOperand;
        while !self.operator_stack.is_empty() {
            if !self.does_last_support(&token) {
                try!(self.pop_token());
            } else {
                let last_operator = self.operator_stack.pop().unwrap();
                // Ensure that the operator that was popped is our desired match.
                if let Some(t) = on_match(self, last_operator) {
                    return t;
                }
                break;
            }
        }
        Err(self.err(Some(&token), &format!("Misplaced {:?}", token)))
    }

    // Ensures that the operator is something that can be popped otherwise it's unclosed.
    #[inline]
    fn assert_not_unclosed(&mut self, operator: &Operator) -> Result<(), ParseError> {
        match operator {
            &Operator::Function(_, _) => Err(self.err(None, "Unclosed function")),
            &Operator::MultiHash(_) => Err(self.err(None, "Unclosed multi-hash '{'")),
            &Operator::MultiList(_) => Err(self.err(None, "Unclosed multi-list '['")),
            &Operator::FilterProjection(None) => Err(self.err(None, "Unclosed filter")),
            _ => Ok(())
        }
    }

    #[inline]
    fn pop_token(&mut self) -> Result<(), ParseError> {
        let operator = self.operator_stack.pop().unwrap();
        try!(self.assert_not_unclosed(&operator));
        if operator.is_binary() {
            if self.output_queue.len() < 2 {
                return Err(self.err(None, &format!("Missing right side of '{}'", operator)));
            }
            let rhs = self.output_queue.pop().unwrap();
            let lhs = self.output_queue.pop().unwrap();
            match operator {
                Operator::Basic(ref tok) => try!(self.pop_basic_binary(tok, lhs, rhs)),
                Operator::ArrayProjection => self.output_queue.push(
                    Ast::Projection(Box::new(lhs), Box::new(rhs))),
                Operator::FilterProjection(Some(expr)) => self.output_queue.push(
                    Ast::Projection(Box::new(lhs),
                                    Box::new(Ast::Condition(
                                        Box::new(expr),
                                        Box::new(rhs))))),
                Operator::SliceProjection(_, start, stop, step) => self.output_queue.push(
                    Ast::Subexpr(Box::new(lhs),
                                 Box::new(Ast::Projection(
                                     Box::new(Ast::Slice(start, stop, step)), Box::new(rhs))))),
                _ => return Err(self.err(None, &format!("Unexpected binary operator: {:?}",
                                                        operator)))
            };
        } else {
            let node = self.output_queue.pop().unwrap();
            match operator {
                Operator::Basic(ref tok) if tok == &Token::Ampersand =>
                    self.output_queue.push(Ast::Expref(Box::new(node))),
                Operator::SliceProjection(_, start, stop, step) => self.output_queue.push(
                    Ast::Projection(Box::new(Ast::Slice(start, stop, step)), Box::new(node))),
                _ => return Err(self.err(None, &"Unexpected unary operator"))
            }
        }
        Ok(())
    }

    #[inline]
    fn pop_basic_binary(&mut self, tok: &Token, lhs: Ast, rhs: Ast) -> Result<(), ParseError> {
        match tok {
            &Token::Dot => self.output_queue.push(
                Ast::Subexpr(Box::new(lhs), Box::new(rhs))),
            &Token::Or => self.output_queue.push(
                Ast::Or(Box::new(lhs), Box::new(rhs))),
            &Token::Pipe => self.output_queue.push(
                Ast::Subexpr(Box::new(lhs), Box::new(rhs))),
            &Token::Star => self.output_queue.push(
                Ast::Projection(Box::new(Ast::ObjectValues(Box::new(lhs))),
                                Box::new(rhs))),
            &Token::Flatten => self.output_queue.push(
                Ast::Projection(Box::new(Ast::Flatten(Box::new(lhs))),
                                Box::new(rhs))),
            &Token::Eq => self.output_queue.push(
                Ast::Comparison(Comparator::Eq, Box::new(lhs), Box::new(rhs))),
            &Token::Ne => self.output_queue.push(
                Ast::Comparison(Comparator::Ne, Box::new(lhs), Box::new(rhs))),
            &Token::Gt => self.output_queue.push(
                Ast::Comparison(Comparator::Gt, Box::new(lhs), Box::new(rhs))),
            &Token::Gte => self.output_queue.push(
                Ast::Comparison(Comparator::Gte, Box::new(lhs), Box::new(rhs))),
            &Token::Lt => self.output_queue.push(
                Ast::Comparison(Comparator::Lt, Box::new(lhs), Box::new(rhs))),
            &Token::Lte => self.output_queue.push(
                Ast::Comparison(Comparator::Lte, Box::new(lhs), Box::new(rhs))),
            _ => return Err(self.err(None, &"Unexpected binary operator"))
        };
        Ok(())
    }

    /// Advances the cursor position of the parser and returns the next token or Token::Eof.
    #[inline]
    fn advance(&mut self) -> Token {
        match self.stream.next() {
            Some((pos, tok)) => { self.pos = pos; tok },
            None => Token::Eof
        }
    }

    /// Returns a formatted ParseError with the given message.
    fn err(&self, current_token: Option<&Token>, error_msg: &str) -> ParseError {
        let mut buff = error_msg.to_string();
        if let Some(&Token::Error { ref msg, .. }) = current_token {
            buff.push_str(&format!(" -- {}", msg));
        }
        ParseError::new(&self.expr, self.pos, buff)
    }

    /// Generates a formatted parse error for an out of place token.
    fn token_err(&self, current_token: &Token) -> ParseError {
        let error_message = &format!("Unexpected token: {}", current_token.token_name());
        self.err(Some(current_token), error_message)
    }
}


#[cfg(test)]
mod test {
    use super::*;
    use super::Operator;
    use ast::Ast;
    use lexer::Token;

    #[test] fn test_parse_nud() {
        let ast = parse("foo").unwrap();
        assert_eq!("Identifier(\"foo\")", format!("{:?}", ast));
    }

    #[test] fn test_parse_subexpr() {
        let ast = parse("foo.bar.baz").unwrap();
        assert_eq!("Subexpr(Subexpr(Identifier(\"foo\"), Identifier(\"bar\")), \
                            Identifier(\"baz\"))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_or() {
        let ast = parse("foo || bar").unwrap();
        assert_eq!("Or(Identifier(\"foo\"), Identifier(\"bar\"))", format!("{:?}", ast));
    }

    #[test] fn test_parse_or_and_subexpr_with_precedence() {
        let ast = parse("foo.baz || bar.bam").unwrap();
        assert_eq!("Or(Subexpr(Identifier(\"foo\"), Identifier(\"baz\")), \
                       Subexpr(Identifier(\"bar\"), Identifier(\"bam\")))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_or_and_pipe_with_precedence() {
        let ast = parse("foo || bar | baz").unwrap();
        assert_eq!("Subexpr(Or(Identifier(\"foo\"), Identifier(\"bar\")), \
                            Identifier(\"baz\"))", format!("{:?}", ast));
    }

    #[test] fn test_comparator() {
        let ast = parse("foo.bar == baz || bam").unwrap();
        assert_eq!(
            "Comparison(Eq, Subexpr(Identifier(\"foo\"), Identifier(\"bar\")), \
                                    Or(Identifier(\"baz\"), Identifier(\"bam\")))",
            format!("{:?}", ast));
    }

    #[test] fn test_parse_wildcard_values() {
        let ast = parse("foo.*.baz").unwrap();
        assert_eq!("Projection(ObjectValues(Identifier(\"foo\")), Identifier(\"baz\"))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_nud_wildcard_values() {
        let ast = parse("*.baz").unwrap();
        assert_eq!("Projection(ObjectValues(CurrentNode), Identifier(\"baz\"))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_nud_wildcard_values_with_no_rhs() {
        let ast = parse("* | baz").unwrap();
        assert_eq!("Subexpr(Projection(ObjectValues(CurrentNode), CurrentNode), \
                            Identifier(\"baz\"))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_flatten() {
        let ast = parse("foo[].baz").unwrap();
        assert_eq!("Projection(Flatten(Identifier(\"foo\")), Identifier(\"baz\"))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_nud_flatten() {
        let ast = parse("[].baz").unwrap();
        assert_eq!("Projection(Flatten(CurrentNode), Identifier(\"baz\"))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_wildcard_index() {
        let ast = parse("foo[*].baz").unwrap();
        assert_eq!("Projection(Identifier(\"foo\"), Identifier(\"baz\"))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_number() {
        let ast = parse("foo[0]").unwrap();
        assert_eq!("Subexpr(Identifier(\"foo\"), Index(0))", format!("{:?}", ast));
    }

    #[test] fn test_parse_number_with_subexpr() {
        let ast = parse("foo[0].bar").unwrap();
        assert_eq!("Subexpr(Subexpr(Identifier(\"foo\"), Index(0)), Identifier(\"bar\"))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_number_in_projection() {
        let ast = parse("foo.*[0]").unwrap();
        assert_eq!("Projection(ObjectValues(Identifier(\"foo\")), Index(0))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_complex_expression() {
        let ast = parse("foo.*.bar[*][0].baz || bam | boo").unwrap();
        assert_eq!("Subexpr(Or(Projection(ObjectValues(Identifier(\"foo\")), \
                                Projection(Identifier(\"bar\"), \
                                           Subexpr(Index(0), Identifier(\"baz\")))), \
                       Identifier(\"bam\")), Identifier(\"boo\"))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_expression_reference() {
        let ast = parse("&foo.bar | [0]").unwrap();
        assert_eq!("Expref(Subexpr(Subexpr(Identifier(\"foo\"), Identifier(\"bar\")), Index(0)))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_empty_functions() {
        let ast = parse("foo()").unwrap();
        assert_eq!("Function(\"foo\", [])", format!("{:?}", ast));
    }

    #[test] fn test_parse_functions_at_end() {
        let ast = parse("foo(bar)").unwrap();
        assert_eq!("Function(\"foo\", [Identifier(\"bar\")])", format!("{:?}", ast));
    }

    #[test] fn test_ensures_functions_are_closed() {
        let result = parse("foo(bar");
        assert_eq!("Err(ParseError { msg: \"Parse error at line 0, col 7; Unclosed function\\n\
                   foo(bar\\n       ^\\n\", line: 0, col: 7 })",
                   format!("{:?}", result));
    }

    #[test] fn test_ensures_functions_with_no_args_are_closed() {
        let result = parse("foo(");
        assert_eq!("Err(ParseError { msg: \"Parse error at line 0, col 4; Unclosed function\\n\
                   foo(\\n    ^\\n\", line: 0, col: 4 })",
                   format!("{:?}", result));
    }

    #[test] fn test_parse_functions_with_multiple_args() {
        let ast = parse("foo(bar, baz.boo, bam || qux)").unwrap();
        assert_eq!("Function(\"foo\", [Identifier(\"bar\"), \
                                       Subexpr(Identifier(\"baz\"), Identifier(\"boo\")), \
                                       Or(Identifier(\"bam\"), Identifier(\"qux\"))])",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_multi_list() {
        let ast = parse("foo.[bar, baz]").unwrap();
        assert_eq!("Subexpr(Identifier(\"foo\"), \
                            MultiList([Identifier(\"bar\"), Identifier(\"baz\")]))",
                   format!("{:?}", ast));
    }

    #[test] fn test_ensures_multi_list_are_closed() {
        let result = parse("foo.[bar, baz");
        assert_eq!("Err(ParseError { msg: \"Parse error at line 0, col 13; \
                   Unclosed multi-list \\\'[\\\'\\n\
                   foo.[bar, baz\\n             ^\\n\", line: 0, col: 13 })",
                   format!("{:?}", result));
    }

    #[test] fn test_parse_postfix_slice_projections() {
        let ast = parse("foo[0::-1].bar").unwrap();
        assert_eq!("Subexpr(Identifier(\"foo\"), \
                            Projection(Slice(Some(0), None, Some(-1)), \
                                       Identifier(\"bar\")))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_prefix_slice_projections() {
        let ast = parse("[0::-1].bar").unwrap();
        assert_eq!("Projection(Slice(Some(0), None, Some(-1)), Identifier(\"bar\"))",
                   format!("{:?}", ast));
    }

    #[test] fn test_ensures_slices_are_closed() {
        let result = parse("[0::1");
        assert_eq!("Err(ParseError { msg: \"Parse error at line 0, col 5; \
                   Expected number, \\\':\\\', or \\\']\\\'\\n\
                   [0::1\\n     ^\\n\", line: 0, col: 5 })",
                   format!("{:?}", result));
    }

    #[test] fn test_parses_nud_filter_projections() {
        let ast = parse("[?foo == bar].baz").unwrap();
        assert_eq!("Projection(CurrentNode, \
                               Condition(\
                                   Comparison(Eq, Identifier(\"foo\"), Identifier(\"bar\")), \
                                   Identifier(\"baz\")))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parses_led_filter_projections() {
        let ast = parse("prefix[?foo == bar].baz.boo").unwrap();
        assert_eq!("Projection(Identifier(\"prefix\"), \
                               Condition(\
                                   Comparison(Eq, Identifier(\"foo\"), Identifier(\"bar\")), \
                                   Subexpr(Identifier(\"baz\"), Identifier(\"boo\"))))",
                   format!("{:?}", ast));
    }

    #[test] fn test_ensures_filters_are_not_empty() {
        let result = parse("prefix[?].bar");
        assert_eq!("Err(ParseError { msg: \"Parse error at line 0, col 8; Unexpected prefix \
                   token: Rbracket\\nprefix[?].bar\\n        ^\\n\", line: 0, col: 8 })",
                   format!("{:?}", result));
    }

    #[test] fn test_ensures_filters_are_closed() {
        let result = parse("prefix[?baz");
        assert_eq!("Err(ParseError { msg: \"Parse error at line 0, col 11; Unclosed filter\\n\
                   prefix[?baz\\n           ^\\n\", line: 0, col: 11 })",
                   format!("{:?}", result));
    }

    #[test] fn test_parse_multi_hash() {
        let ast = parse("foo.{bar: baz, bam: boo}.bam").unwrap();
        assert_eq!("Subexpr(Subexpr(Identifier(\"foo\"), \
                            MultiHash([KeyValuePair { key: Identifier(\"bar\"), \
                                                      value: Identifier(\"baz\") }, \
                                       KeyValuePair { key: Identifier(\"bam\"), \
                                                      value: Identifier(\"boo\") }])), \
                            Identifier(\"bam\"))",
                   format!("{:?}", ast));
    }

    #[test] fn test_parse_nud_multi_hash() {
        let ast = parse("{bar: baz}").unwrap();
        assert_eq!("MultiHash([KeyValuePair { key: Identifier(\"bar\"), \
                                              value: Identifier(\"baz\") }])",
                   format!("{:?}", ast));
    }

    #[test] fn test_ensures_multi_hash_are_closed() {
        let result = parse("foo.{bar: baz");
        assert_eq!("Err(ParseError { msg: \"Parse error at line 0, col 13; Unclosed multi-hash \
                   \\\'{\\\'\\nfoo.{bar: baz\\n             ^\\n\", line: 0, col: 13 })",
                   format!("{:?}", result));
    }

    #[test] fn test_ensures_multi_hash_colon_has_value() {
        let result = parse("foo.{bar:}");
        assert_eq!("Err(ParseError { msg: \"Parse error at line 0, col 9; Unexpected prefix \
                   token: Rbrace\\nfoo.{bar:}\\n         ^\\n\", line: 0, col: 9 })",
                   format!("{:?}", result));
    }

    #[test] fn test_ensures_multi_hash_comma_followed_by_expr() {
        let result = parse("foo.{bar: baz, }");
        assert_eq!("Err(ParseError { msg: \"Parse error at line 0, col 15; Expected identifier \
                   for multi-hash key\\nfoo.{bar: baz, }\\n               ^\\n\", \
                   line: 0, col: 15 })",
                   format!("{:?}", result));
    }

    #[test] fn test_ensures_multi_hash_comma_followed_by_key() {
        let result = parse("{&bar: bam}");
        assert_eq!("Err(ParseError { msg: \"Parse error at line 0, col 1; Expected identifier \
                   for multi-hash key\\n{&bar: bam}\\n ^\\n\", line: 0, col: 1 })",
                   format!("{:?}", result));
    }

    #[test] fn test_does_not_blow_up_on_bad_binary() {
        let result = parse("foo |");
        assert_eq!("Err(ParseError { msg: \"Parse error at line 0, col 5; Missing right side \
                   of \\\'|\\\'\\nfoo |\\n     ^\\n\", line: 0, col: 5 })",
                   format!("{:?}", result));
    }

    #[test] fn test_displays_operators() {
        assert_eq!(".".to_string(), format!("{}", Operator::Basic(Token::Dot)));
        assert_eq!("foo function".to_string(),
                   format!("{}", Operator::Function("foo".to_string(), vec!())));
        assert_eq!("multi-list".to_string(), format!("{}", Operator::MultiList(vec!())));
        assert_eq!("multi-hash".to_string(), format!("{}", Operator::MultiHash(vec!())));
        assert_eq!("[*]".to_string(), format!("{}", Operator::ArrayProjection));
        assert_eq!("filter-projection".to_string(),
                   format!("{}", Operator::FilterProjection(Some(Ast::CurrentNode))));
        assert_eq!("slice-projection".to_string(),
                   format!("{}", Operator::SliceProjection(true, None, None, None)));
    }
}
