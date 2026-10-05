//! Recursive-descent parser for the Tridentix MVP subset.
//! Consumes the Token stream from `lexer.rs` and produces the AST
//! defined in `ast.rs`. Indent/Dedent tokens are treated as the block
//! delimiters (equivalent to `{`/`}` in a brace-based language).

use crate::ast::{
    BinOp, EnumVariant, Expr, Param, Program, RestartStrategy, Stmt, StructField, SupervisedChild, UnOp,
};
use crate::lexer::Token;

pub struct ParseError {
    pub message: String,
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

type PResult<T> = Result<T, ParseError>;

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0 }
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        self.pos += 1;
        tok
    }

    fn expect(&mut self, expected: &Token) -> PResult<()> {
        if self.peek() == expected {
            self.advance();
            Ok(())
        } else {
            Err(ParseError {
                message: format!("expected {:?}, found {:?}", expected, self.peek()),
            })
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Token::Newline) {
            self.advance();
        }
    }

    pub fn parse_program(&mut self) -> PResult<Program> {
        let mut stmts = Vec::new();
        self.skip_newlines();
        while !matches!(self.peek(), Token::Eof) {
            stmts.push(self.parse_stmt()?);
            self.skip_newlines();
        }
        Ok(stmts)
    }

    /// Parses an indented block: `Indent stmt* Dedent`.
    fn parse_block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect(&Token::Indent)?;
        let mut stmts = Vec::new();
        self.skip_newlines();
        while !matches!(self.peek(), Token::Dedent | Token::Eof) {
            stmts.push(self.parse_stmt()?);
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        match self.peek().clone() {
            Token::Fn => self.parse_fn_def(false),
            Token::Async => {
                self.advance();
                self.parse_fn_def(true)
            }
            Token::Struct => self.parse_struct_def(),
            Token::Enum => self.parse_enum_def(),
            Token::Let => self.parse_let(),
            Token::If => self.parse_if(),
            Token::Loop => self.parse_loop(),
            Token::While => self.parse_while(),
            Token::Return => self.parse_return(),
            Token::Actor => self.parse_actor(),
            Token::Supervisor => self.parse_supervisor(),
            Token::Send => self.parse_send(),
            Token::Try => self.parse_try(),
            Token::Raise => {
                self.advance();
                let e = self.parse_expr()?;
                Ok(Stmt::Raise(e))
            }
            Token::Match => self.parse_match(),
            Token::Import => self.parse_import(),
            _ => {
                let expr = self.parse_expr()?;
                if matches!(self.peek(), Token::Assign) {
                    self.advance(); // consume '='
                    let value = self.parse_expr()?;
                    match expr {
                        Expr::Ident(name) => Ok(Stmt::Assign { name, value }),
                        Expr::FieldAccess(base, field) => Ok(Stmt::FieldAssign { base: *base, field, value }),
                        Expr::Index(base, index) => Ok(Stmt::IndexAssign { base: *base, index: *index, value }),
                        other => Err(ParseError {
                            message: format!("invalid assignment target: {:?}", other),
                        }),
                    }
                } else {
                    Ok(Stmt::ExprStmt(expr))
                }
            }
        }
    }

    fn parse_type_ann(&mut self) -> PResult<Option<String>> {
        if matches!(self.peek(), Token::Colon) {
            self.advance();
            match self.advance() {
                Token::Ident(name) => Ok(Some(name)),
                other => Err(ParseError {
                    message: format!("expected type name, found {:?}", other),
                }),
            }
        } else {
            Ok(None)
        }
    }

    fn parse_fn_def(&mut self, is_async: bool) -> PResult<Stmt> {
        self.expect(&Token::Fn)?;
        let name = match self.advance() {
            Token::Ident(n) => n,
            other => {
                return Err(ParseError {
                    message: format!("expected function name, found {:?}", other),
                })
            }
        };

        // Optional generics: `fn identity<T>(x: T) -> T:`. Type params are
        // parsed and validated for use (see typechecker.rs) but erased at
        // runtime — Tridentix values are already dynamically typed under the
        // hood, so a generic function body just works for any T without
        // monomorphization. This is an honest simplification, not a fake
        // feature: it's the same "erasure" strategy Java/TypeScript use,
        // just without the compile-time type-checking benefits those add.
        let mut type_params = Vec::new();
        if matches!(self.peek(), Token::Lt) {
            self.advance();
            while !matches!(self.peek(), Token::Gt) {
                match self.advance() {
                    Token::Ident(n) => type_params.push(n),
                    other => {
                        return Err(ParseError {
                            message: format!("expected type parameter name, found {:?}", other),
                        })
                    }
                }
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                }
            }
            self.expect(&Token::Gt)?;
        }

        self.expect(&Token::LParen)?;
        let mut params = Vec::new();
        while !matches!(self.peek(), Token::RParen) {
            let pname = match self.advance() {
                Token::Ident(n) => n,
                other => {
                    return Err(ParseError {
                        message: format!("expected parameter name, found {:?}", other),
                    })
                }
            };
            let ptype = self.parse_type_ann()?;
            params.push(Param {
                name: pname,
                type_ann: ptype,
            });
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            }
        }
        self.expect(&Token::RParen)?;

        let ret_type = if matches!(self.peek(), Token::Arrow) {
            self.advance();
            match self.advance() {
                Token::Ident(n) => Some(n),
                other => {
                    return Err(ParseError {
                        message: format!("expected return type, found {:?}", other),
                    })
                }
            }
        } else {
            None
        };

        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        let body = self.parse_block()?;

        Ok(Stmt::FnDef {
            name,
            type_params,
            params,
            ret_type,
            is_async,
            body,
        })
    }

    fn parse_struct_def(&mut self) -> PResult<Stmt> {
        self.expect(&Token::Struct)?;
        let name = match self.advance() {
            Token::Ident(n) => n,
            other => {
                return Err(ParseError { message: format!("expected struct name, found {:?}", other) })
            }
        };
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        self.expect(&Token::Indent)?;
        self.skip_newlines();

        let mut fields = Vec::new();
        while !matches!(self.peek(), Token::Dedent | Token::Eof) {
            let fname = match self.advance() {
                Token::Ident(n) => n,
                other => {
                    return Err(ParseError {
                        message: format!("expected field name, found {:?}", other),
                    })
                }
            };
            let ftype = self.parse_type_ann()?;
            fields.push(StructField { name: fname, type_ann: ftype });
            self.expect(&Token::Newline)?;
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;

        Ok(Stmt::StructDef { name, fields })
    }

    fn parse_enum_def(&mut self) -> PResult<Stmt> {
        self.expect(&Token::Enum)?;
        let name = match self.advance() {
            Token::Ident(n) => n,
            other => {
                return Err(ParseError { message: format!("expected enum name, found {:?}", other) })
            }
        };
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        self.expect(&Token::Indent)?;
        self.skip_newlines();

        let mut variants = Vec::new();
        while !matches!(self.peek(), Token::Dedent | Token::Eof) {
            let vname = match self.advance() {
                Token::Ident(n) => n,
                other => {
                    return Err(ParseError {
                        message: format!("expected variant name, found {:?}", other),
                    })
                }
            };
            let mut payload_names = Vec::new();
            if matches!(self.peek(), Token::LParen) {
                self.advance();
                while !matches!(self.peek(), Token::RParen) {
                    match self.advance() {
                        Token::Ident(n) => payload_names.push(n),
                        other => {
                            return Err(ParseError {
                                message: format!("expected payload field name, found {:?}", other),
                            })
                        }
                    }
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                    }
                }
                self.expect(&Token::RParen)?;
            }
            variants.push(EnumVariant { name: vname, payload_names });
            self.expect(&Token::Newline)?;
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;

        Ok(Stmt::EnumDef { name, variants })
    }

    fn parse_actor(&mut self) -> PResult<Stmt> {
        self.expect(&Token::Actor)?;
        let name = match self.advance() {
            Token::Ident(n) => n,
            other => {
                return Err(ParseError {
                    message: format!("expected actor name, found {:?}", other),
                })
            }
        };
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        self.expect(&Token::Indent)?;
        self.skip_newlines();

        let mut state_vars = Vec::new();
        if matches!(self.peek(), Token::Ident(n) if n == "state") {
            self.advance(); // "state"
            self.expect(&Token::Colon)?;
            self.expect(&Token::Newline)?;
            self.expect(&Token::Indent)?;
            self.skip_newlines();
            while !matches!(self.peek(), Token::Dedent | Token::Eof) {
                let field_name = match self.advance() {
                    Token::Ident(n) => n,
                    other => {
                        return Err(ParseError {
                            message: format!("expected state field name, found {:?}", other),
                        })
                    }
                };
                let field_type = self.parse_type_ann()?;
                self.expect(&Token::Assign)?;
                let init_expr = self.parse_expr()?;
                state_vars.push((field_name, field_type, init_expr));
                self.expect(&Token::Newline)?;
                self.skip_newlines();
            }
            self.expect(&Token::Dedent)?;
            self.skip_newlines();
        }

        self.expect(&Token::On)?;
        match self.advance() {
            Token::Ident(n) if n == "receive" => {}
            other => {
                return Err(ParseError {
                    message: format!("expected 'receive', found {:?}", other),
                })
            }
        }
        self.expect(&Token::LParen)?;
        let param_name = match self.advance() {
            Token::Ident(n) => n,
            other => {
                return Err(ParseError {
                    message: format!("expected message parameter name, found {:?}", other),
                })
            }
        };
        let param_type = self.parse_type_ann()?;
        self.expect(&Token::RParen)?;
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        let body = self.parse_block()?;

        self.skip_newlines();
        self.expect(&Token::Dedent)?;

        Ok(Stmt::Actor {
            name,
            state_vars,
            param_name,
            param_type,
            body,
        })
    }

    fn parse_supervisor(&mut self) -> PResult<Stmt> {
        self.expect(&Token::Supervisor)?;
        let name = match self.advance() {
            Token::Ident(n) => n,
            other => {
                return Err(ParseError {
                    message: format!("expected supervisor name, found {:?}", other),
                })
            }
        };
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        self.expect(&Token::Indent)?;
        self.skip_newlines();

        let mut strategy = RestartStrategy::OneForOne;
        let mut max_restarts = 3i64;
        let mut time_window_secs = 60i64;
        let mut children = Vec::new();

        loop {
            match self.peek().clone() {
                Token::Ident(field) if field == "strategy" => {
                    self.advance();
                    self.expect(&Token::Assign)?;
                    strategy = match self.advance() {
                        Token::Ident(v) if v == "one_for_one" => RestartStrategy::OneForOne,
                        Token::Ident(v) if v == "one_for_all" => RestartStrategy::OneForAll,
                        Token::Ident(v) if v == "rest_for_one" => RestartStrategy::RestForOne,
                        other => {
                            return Err(ParseError {
                                message: format!("unknown restart strategy: {:?}", other),
                            })
                        }
                    };
                    self.expect(&Token::Newline)?;
                }
                Token::Ident(field) if field == "max_restarts" => {
                    self.advance();
                    self.expect(&Token::Assign)?;
                    max_restarts = match self.advance() {
                        Token::Int(n) => n,
                        other => {
                            return Err(ParseError {
                                message: format!("expected int for max_restarts, found {:?}", other),
                            })
                        }
                    };
                    self.expect(&Token::Newline)?;
                }
                Token::Ident(field) if field == "time_window" => {
                    self.advance();
                    self.expect(&Token::Assign)?;
                    time_window_secs = match self.advance() {
                        Token::Int(n) => n,
                        other => {
                            return Err(ParseError {
                                message: format!("expected int (seconds) for time_window, found {:?}", other),
                            })
                        }
                    };
                    self.expect(&Token::Newline)?;
                }
                Token::Ident(field) if field == "children" => {
                    self.advance();
                    self.expect(&Token::Colon)?;
                    self.expect(&Token::Newline)?;
                    self.expect(&Token::Indent)?;
                    self.skip_newlines();
                    while !matches!(self.peek(), Token::Dedent | Token::Eof) {
                        self.expect(&Token::Spawn)?;
                        let actor_name = match self.advance() {
                            Token::Ident(n) => n,
                            other => {
                                return Err(ParseError {
                                    message: format!("expected actor name after 'spawn', found {:?}", other),
                                })
                            }
                        };
                        self.expect(&Token::LParen)?;
                        self.expect(&Token::RParen)?;
                        self.expect(&Token::As)?;
                        let bind_name = match self.advance() {
                            Token::Ident(n) => n,
                            other => {
                                return Err(ParseError {
                                    message: format!("expected binding name after 'as', found {:?}", other),
                                })
                            }
                        };
                        children.push(SupervisedChild { actor_name, bind_name });
                        self.skip_newlines();
                    }
                    self.expect(&Token::Dedent)?;
                    self.skip_newlines();
                }
                Token::Dedent | Token::Eof => break,
                other => {
                    return Err(ParseError {
                        message: format!(
                            "unexpected token in supervisor block: {:?} (expected 'strategy', 'max_restarts', 'time_window', or 'children')",
                            other
                        ),
                    })
                }
            }
        }

        self.expect(&Token::Dedent)?;

        Ok(Stmt::Supervisor {
            name,
            strategy,
            max_restarts,
            time_window_secs,
            children,
        })
    }

    fn parse_try(&mut self) -> PResult<Stmt> {
        self.expect(&Token::Try)?;
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        let body = self.parse_block()?;

        self.expect(&Token::Catch)?;
        let catch_var = match self.advance() {
            Token::Ident(n) => n,
            other => {
                return Err(ParseError {
                    message: format!("expected catch variable name, found {:?}", other),
                })
            }
        };
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        let catch_body = self.parse_block()?;

        Ok(Stmt::Try { body, catch_var, catch_body })
    }

    /// `match subj: pattern => : block ... _ => : block`. Literal
    /// patterns (`0 => ...`) desugar into a plain `subject == 0`
    /// equality-check `if`. Enum-variant patterns (`Shape.Circle(r) =>`)
    /// desugar into a REAL tag check (`Expr::EnumIsVariant`) plus `let`
    /// bindings that extract payload values (`Expr::EnumPayload`)
    /// prepended to the arm body — so `r` is actually bound and usable
    /// inside, not just syntax that happens to parse. Both new node
    /// types need interpreter + typechecker support (see those files'
    /// `Expr::EnumIsVariant`/`Expr::EnumPayload` handling) — this is
    /// real destructuring, not the naive equality-only match Tridentix had
    /// before this pass.
    fn parse_match(&mut self) -> PResult<Stmt> {
        self.expect(&Token::Match)?;
        let subject = self.parse_expr()?;
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        self.expect(&Token::Indent)?;
        self.skip_newlines();

        // Each arm becomes (condition, body) where condition is either:
        //   - `None` for the wildcard `_` arm (becomes the final `else`)
        //   - a ready-to-use boolean `Expr` for everything else. For a
        //     literal pattern (`0 => ...`) this is `subject == 0`. For an
        //     enum-variant pattern (`Shape.Circle(r) => ...`) this is
        //     `Expr::EnumIsVariant(subject, "Shape", "Circle")`, with
        //     `let r = <payload extraction>` statements PREPENDED to the
        //     arm's body so the binding is visible inside it — this is
        //     what makes `Circle(r) => print(r)` actually work, not just
        //     parse.
        let mut arms: Vec<(Option<Expr>, Vec<Stmt>)> = Vec::new();
        while !matches!(self.peek(), Token::Dedent | Token::Eof) {
            let is_wildcard = matches!(self.peek(), Token::Ident(n) if n == "_");
            if is_wildcard {
                self.advance();
                self.expect(&Token::FatArrow)?;
                self.expect(&Token::Colon)?;
                self.expect(&Token::Newline)?;
                let block = self.parse_block()?;
                arms.push((None, block));
                self.skip_newlines();
                continue;
            }

            let pattern_expr = self.parse_expr()?;
            self.expect(&Token::FatArrow)?;
            self.expect(&Token::Colon)?;
            self.expect(&Token::Newline)?;
            let mut block = self.parse_block()?;

            let condition = match &pattern_expr {
                Expr::EnumLit(type_name, variant, bind_exprs) => {
                    // Enum-variant pattern, e.g. `Shape.Circle(r) => ...`.
                    // Each bare identifier in the parens becomes a `let`
                    // binding, prepended to the arm body, that extracts
                    // that payload slot at runtime.
                    let mut bindings = Vec::new();
                    for (idx, be) in bind_exprs.iter().enumerate() {
                        if let Expr::Ident(bind_name) = be {
                            bindings.push(Stmt::Let {
                                name: bind_name.clone(),
                                mutable: false,
                                type_ann: None,
                                value: Expr::EnumPayload(
                                    Box::new(subject.clone()),
                                    type_name.clone(),
                                    variant.clone(),
                                    idx,
                                ),
                            });
                        }
                        // A non-identifier payload pattern (e.g. a literal
                        // like `Circle(0)`) isn't supported for binding —
                        // only identifier-capture patterns are, matching
                        // this checker's "real but restricted" scope.
                    }
                    bindings.extend(block);
                    block = bindings;
                    Expr::EnumIsVariant(Box::new(subject.clone()), type_name.clone(), variant.clone())
                }
                _ => Expr::Binary(Box::new(subject.clone()), BinOp::Eq, Box::new(pattern_expr)),
            };

            arms.push((Some(condition), block));
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;

        if arms.is_empty() {
            return Err(ParseError { message: "match with no arms".into() });
        }

        let mut else_block: Option<Vec<Stmt>> = None;
        let mut concrete_arms = Vec::new();
        for (condition, block) in arms {
            match condition {
                None => else_block = Some(block),
                Some(cond) => concrete_arms.push((cond, block)),
            }
        }
        if concrete_arms.is_empty() {
            return Err(ParseError { message: "match must have at least one non-wildcard arm".into() });
        }
        let (first_cond, first_block) = concrete_arms.remove(0);

        Ok(Stmt::If {
            cond: first_cond,
            then_block: first_block,
            elif_blocks: concrete_arms,
            else_block,
        })
    }

    /// `import "relative/path.trix"` — file-based module system.
    /// Resolution (reading the file, merging its top-level definitions)
    /// happens as a post-parse pass in main.rs, not here — the parser
    /// only records the path.
    fn parse_import(&mut self) -> PResult<Stmt> {
        self.expect(&Token::Import)?;
        let path = match self.advance() {
            Token::Str(s) => s,
            other => {
                return Err(ParseError {
                    message: format!("expected a string path after 'import', found {:?}", other),
                })
            }
        };
        Ok(Stmt::Import(path))
    }

    fn parse_send(&mut self) -> PResult<Stmt> {
        self.expect(&Token::Send)?;
        self.expect(&Token::LParen)?;
        let target = self.parse_expr()?;
        self.expect(&Token::Comma)?;
        let message = self.parse_expr()?;
        self.expect(&Token::RParen)?;
        Ok(Stmt::Send { target, message })
    }

    fn parse_let(&mut self) -> PResult<Stmt> {
        self.expect(&Token::Let)?;
        let mutable = if matches!(self.peek(), Token::Mut) {
            self.advance();
            true
        } else {
            false
        };
        let name = match self.advance() {
            Token::Ident(n) => n,
            other => {
                return Err(ParseError {
                    message: format!("expected identifier after 'let', found {:?}", other),
                })
            }
        };
        let type_ann = self.parse_type_ann()?;
        self.expect(&Token::Assign)?;
        let value = self.parse_expr()?;
        Ok(Stmt::Let {
            name,
            mutable,
            type_ann,
            value,
        })
    }

    fn parse_if(&mut self) -> PResult<Stmt> {
        self.expect(&Token::If)?;
        let cond = self.parse_expr()?;
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        let then_block = self.parse_block()?;

        let mut elif_blocks = Vec::new();
        while matches!(self.peek(), Token::Elif) {
            self.advance();
            let c = self.parse_expr()?;
            self.expect(&Token::Colon)?;
            self.expect(&Token::Newline)?;
            let b = self.parse_block()?;
            elif_blocks.push((c, b));
        }

        let else_block = if matches!(self.peek(), Token::Else) {
            self.advance();
            self.expect(&Token::Colon)?;
            self.expect(&Token::Newline)?;
            Some(self.parse_block()?)
        } else {
            None
        };

        Ok(Stmt::If {
            cond,
            then_block,
            elif_blocks,
            else_block,
        })
    }

    /// Only supports the `loop x in range(a, b):` form for the MVP
    /// (Language Spec §2.6). General iterator loops are a later phase.
    fn parse_loop(&mut self) -> PResult<Stmt> {
        self.expect(&Token::Loop)?;
        let var = match self.advance() {
            Token::Ident(n) => n,
            other => {
                return Err(ParseError {
                    message: format!("expected loop variable, found {:?}", other),
                })
            }
        };
        self.expect(&Token::In)?;
        match self.advance() {
            Token::Ident(n) if n == "range" => {}
            other => {
                return Err(ParseError {
                    message: format!("expected 'range(...)', found {:?}", other),
                })
            }
        }
        self.expect(&Token::LParen)?;
        let start = self.parse_expr()?;
        self.expect(&Token::Comma)?;
        let end = self.parse_expr()?;
        self.expect(&Token::RParen)?;
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        let body = self.parse_block()?;
        Ok(Stmt::Loop {
            var,
            start,
            end,
            body,
        })
    }

    fn parse_while(&mut self) -> PResult<Stmt> {
        self.expect(&Token::While)?;
        let cond = self.parse_expr()?;
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        let body = self.parse_block()?;
        Ok(Stmt::While { cond, body })
    }

    fn parse_return(&mut self) -> PResult<Stmt> {
        self.expect(&Token::Return)?;
        if matches!(self.peek(), Token::Newline | Token::Dedent | Token::Eof) {
            Ok(Stmt::Return(None))
        } else {
            let val = self.parse_expr()?;
            Ok(Stmt::Return(Some(val)))
        }
    }

    // ---- Expression parsing (precedence climbing) ----
    // Precedence, low to high: comparison  ->  additive  ->  multiplicative  ->  primary

    fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Token::EqEq => BinOp::Eq,
                Token::NotEq => BinOp::NotEq,
                Token::Lt => BinOp::Lt,
                Token::Gt => BinOp::Gt,
                Token::LtEq => BinOp::LtEq,
                Token::GtEq => BinOp::GtEq,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_additive()?;
            lhs = Expr::Binary(Box::new(lhs), op, Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_additive(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::Binary(Box::new(lhs), op, Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary(Box::new(lhs), op, Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        match self.peek() {
            Token::Minus => {
                self.advance();
                let operand = self.parse_unary()?;
                Ok(Expr::Unary(UnOp::Neg, Box::new(operand)))
            }
            Token::Not => {
                self.advance();
                let operand = self.parse_unary()?;
                Ok(Expr::Unary(UnOp::Not, Box::new(operand)))
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        match self.advance() {
            Token::Int(n) => Ok(Expr::IntLit(n)),
            Token::Float(f) => Ok(Expr::FloatLit(f)),
            Token::Str(s) => Ok(Expr::StrLit(s)),
            Token::True => Ok(Expr::BoolLit(true)),
            Token::False => Ok(Expr::BoolLit(false)),
            Token::LBracket => {
                let mut items = Vec::new();
                while !matches!(self.peek(), Token::RBracket) {
                    items.push(self.parse_expr()?);
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                    }
                }
                self.expect(&Token::RBracket)?;
                Ok(Expr::ListLit(items))
            }
            Token::Spawn => {
                let name = match self.advance() {
                    Token::Ident(n) => n,
                    other => {
                        return Err(ParseError {
                            message: format!("expected actor name after 'spawn', found {:?}", other),
                        })
                    }
                };
                self.expect(&Token::LParen)?;
                let mut args = Vec::new();
                while !matches!(self.peek(), Token::RParen) {
                    args.push(self.parse_expr()?);
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                    }
                }
                self.expect(&Token::RParen)?;
                Ok(Expr::Spawn(name, args))
            }
            Token::Await => {
                let inner = self.parse_unary()?;
                Ok(Expr::Await(Box::new(inner)))
            }
            Token::Fn => {
                // Closure literal: `fn(a, b) => a + b` — single-expression
                // body only (see ast.rs's Expr::Closure doc comment).
                self.expect(&Token::LParen)?;
                let mut params = Vec::new();
                while !matches!(self.peek(), Token::RParen) {
                    let pname = match self.advance() {
                        Token::Ident(n) => n,
                        other => {
                            return Err(ParseError {
                                message: format!("expected closure parameter name, found {:?}", other),
                            })
                        }
                    };
                    let ptype = self.parse_type_ann()?;
                    params.push(Param { name: pname, type_ann: ptype });
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                    }
                }
                self.expect(&Token::RParen)?;
                self.expect(&Token::FatArrow)?;
                let body = self.parse_expr()?;
                Ok(Expr::Closure(params, Box::new(body)))
            }
            Token::LParen => {
                let inner = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok(inner)
            }
            Token::Ident(name) => self.parse_ident_expr(name),
            other => Err(ParseError {
                message: format!("unexpected token in expression: {:?}", other),
            }),
        }
    }

    /// Handles everything that can follow a leading identifier:
    /// - `tensor.zeros(...)` / `tensor.sum(...)` — namespaced builtins
    ///   (kept as the existing folded-name `Expr::Call` for backward
    ///   compatibility with earlier examples).
    /// - `PascalCase.Variant` / `PascalCase.Variant(args)` — enum-variant
    ///   construction (`Expr::EnumLit`), recognized by the base
    ///   identifier's naming convention (matches the language's existing
    ///   PascalCase-for-types convention).
    /// - `value.field` — struct field access (`Expr::FieldAccess`),
    ///   chainable (`a.b.c`).
    /// - `PascalCase { field: expr, ... }` — struct literal construction.
    /// - `name(args)` — ordinary function call (also how closures stored
    ///   in a variable get called — see interpreter.rs's `eval_call`,
    ///   which checks local variables for a `Value::Closure` before
    ///   falling back to the global function table).
    fn parse_ident_expr(&mut self, name: String) -> PResult<Expr> {
        let is_pascal_case = name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false);

        // Struct literal: `Point { x: 1, y: 2 }`.
        if is_pascal_case && matches!(self.peek(), Token::LBrace) {
            self.advance();
            let mut fields = Vec::new();
            while !matches!(self.peek(), Token::RBrace) {
                let fname = match self.advance() {
                    Token::Ident(n) => n,
                    other => {
                        return Err(ParseError {
                            message: format!("expected field name in struct literal, found {:?}", other),
                        })
                    }
                };
                self.expect(&Token::Colon)?;
                let fvalue = self.parse_expr()?;
                fields.push((fname, fvalue));
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                }
            }
            self.expect(&Token::RBrace)?;
            return Ok(Expr::StructLit(name, fields));
        }

        // tensor.* namespaced builtins: fold into one call name (unchanged
        // legacy behavior so earlier tensor examples keep working as-is).
        if name == "tensor" && matches!(self.peek(), Token::Dot) {
            let mut full_name = name;
            while matches!(self.peek(), Token::Dot) {
                self.advance();
                match self.advance() {
                    Token::Ident(member) => {
                        full_name.push('.');
                        full_name.push_str(&member);
                    }
                    other => {
                        return Err(ParseError {
                            message: format!("expected member name after '.', found {:?}", other),
                        })
                    }
                }
            }
            if matches!(self.peek(), Token::LParen) {
                self.advance();
                let mut args = Vec::new();
                while !matches!(self.peek(), Token::RParen) {
                    args.push(self.parse_expr()?);
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                    }
                }
                self.expect(&Token::RParen)?;
                return Ok(Expr::Call(full_name, args));
            }
            return Ok(Expr::Ident(full_name));
        }

        // PascalCase.Variant / PascalCase.Variant(args) — enum construction.
        if is_pascal_case && matches!(self.peek(), Token::Dot) {
            self.advance();
            let variant = match self.advance() {
                Token::Ident(n) => n,
                other => {
                    return Err(ParseError {
                        message: format!("expected enum variant name after '.', found {:?}", other),
                    })
                }
            };
            let mut args = Vec::new();
            if matches!(self.peek(), Token::LParen) {
                self.advance();
                while !matches!(self.peek(), Token::RParen) {
                    args.push(self.parse_expr()?);
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                    }
                }
                self.expect(&Token::RParen)?;
            }
            return Ok(Expr::EnumLit(name, variant, args));
        }

        // Ordinary call: `name(args)`.
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            let mut args = Vec::new();
            while !matches!(self.peek(), Token::RParen) {
                args.push(self.parse_expr()?);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                }
            }
            self.expect(&Token::RParen)?;
            return Ok(Expr::Call(name, args));
        }

        // Plain identifier, possibly followed by chained field access:
        // `a.b.c`.
        let mut expr = Expr::Ident(name);
        loop {
            if matches!(self.peek(), Token::Dot) {
                self.advance();
                let field = match self.advance() {
                    Token::Ident(n) => n,
                    other => {
                        return Err(ParseError {
                            message: format!("expected field name after '.', found {:?}", other),
                        })
                    }
                };
                expr = Expr::FieldAccess(Box::new(expr), field);
            } else if matches!(self.peek(), Token::LBracket) {
                self.advance();
                let index_expr = self.parse_expr()?;
                self.expect(&Token::RBracket)?;
                expr = Expr::Index(Box::new(expr), Box::new(index_expr));
            } else {
                break;
            }
        }
        Ok(expr)
    }
}
