use crate::config::{AlignCmtStyle, BraceStyle, Config, ExternCBrace, IndentStyle, PointerAlign, SpaceOption};
use crate::error::FunkyError;
use crate::token::{Span, Token, TokenKind};

// ── Brace-injection pre-pass ──────────────────────────────────────────────────
//
// When add_braces_to_if / _while / _for are enabled, we do a single pre-pass
// over the token list and inject synthetic `{` / `}` tokens around braceless
// single-statement bodies.  The resulting token slice is then fed to the main
// formatter, which sees only already-braced code.

fn inj_synthetic(kind: TokenKind, lexeme: &'static str) -> Token<'static> {
    Token {
        kind,
        lexeme,
        span: Span {
            start_byte: 0,
            end_byte: 0,
            line: 0,
            col: 0,
        },
    }
}

fn inj_copy_ws<'src>(tokens: &[Token<'src>], i: &mut usize, out: &mut Vec<Token<'src>>) {
    while *i < tokens.len() && matches!(tokens[*i].kind, TokenKind::Whitespace | TokenKind::Newline)
    {
        out.push(tokens[*i].clone());
        *i += 1;
    }
}

fn inj_peek_non_ws(tokens: &[Token<'_>], from: usize) -> usize {
    let mut j = from;
    while j < tokens.len() && matches!(tokens[j].kind, TokenKind::Whitespace | TokenKind::Newline) {
        j += 1;
    }
    j
}

/// Like `inj_peek_non_ws` but also skips block/line comments, so a `/* note */ {`
/// pattern is correctly recognized as an already-braced body.
fn inj_peek_non_ws_or_cmt(tokens: &[Token<'_>], from: usize) -> usize {
    let mut j = from;
    while j < tokens.len()
        && matches!(
            tokens[j].kind,
            TokenKind::Whitespace
                | TokenKind::Newline
                | TokenKind::CommentBlock
                | TokenKind::CommentLine
        )
    {
        j += 1;
    }
    j
}

/// Like `inj_copy_ws` but also copies block/line comments verbatim.
fn inj_copy_ws_or_cmt<'src>(tokens: &[Token<'src>], i: &mut usize, out: &mut Vec<Token<'src>>) {
    while *i < tokens.len()
        && matches!(
            tokens[*i].kind,
            TokenKind::Whitespace
                | TokenKind::Newline
                | TokenKind::CommentBlock
                | TokenKind::CommentLine
        )
    {
        out.push(tokens[*i].clone());
        *i += 1;
    }
}

/// Copy a balanced `(…)` group.  `tokens[*i]` must be `(`.
fn inj_copy_paren<'src>(tokens: &[Token<'src>], i: &mut usize, out: &mut Vec<Token<'src>>) {
    out.push(tokens[*i].clone());
    *i += 1;
    let mut depth = 1u32;
    while *i < tokens.len() {
        let t = tokens[*i].clone();
        out.push(t.clone());
        *i += 1;
        match t.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
}

/// Copy a balanced `{ … }` block, recursively applying brace injection inside.
fn inj_copy_block<'src>(
    tokens: &[Token<'src>],
    i: &mut usize,
    out: &mut Vec<Token<'src>>,
    config: &crate::config::Config,
) {
    out.push(tokens[*i].clone()); // `{`
    *i += 1;
    while *i < tokens.len() {
        if tokens[*i].kind == TokenKind::RBrace {
            out.push(tokens[*i].clone());
            *i += 1;
            break;
        }
        inj_item(tokens, i, out, config);
    }
}

/// Copy exactly one statement: a `{ }` block, a control-flow statement, or
/// tokens through the next `;` at brace-depth 0.
fn inj_copy_stmt<'src>(
    tokens: &[Token<'src>],
    i: &mut usize,
    out: &mut Vec<Token<'src>>,
    config: &crate::config::Config,
) {
    inj_copy_ws(tokens, i, out);
    if *i >= tokens.len() {
        return;
    }
    match tokens[*i].kind {
        TokenKind::LBrace => inj_copy_block(tokens, i, out, config),
        TokenKind::KwIf | TokenKind::KwFor | TokenKind::KwWhile | TokenKind::KwElse => {
            inj_item(tokens, i, out, config);
        }
        _ => {
            let mut depth = 0u32;
            while *i < tokens.len() {
                let t = tokens[*i].clone();
                match t.kind {
                    TokenKind::LBrace => depth += 1,
                    TokenKind::RBrace => {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                    }
                    TokenKind::Semi if depth == 0 => {
                        out.push(t);
                        *i += 1;
                        // Carry a trailing inline comment along with the statement
                        // so that `return -1; /* note */` stays together when
                        // braces are injected around the statement.
                        let j = {
                            let mut k = *i;
                            while k < tokens.len() && tokens[k].kind == TokenKind::Whitespace {
                                k += 1;
                            }
                            k
                        };
                        if j < tokens.len()
                            && matches!(
                                tokens[j].kind,
                                TokenKind::CommentBlock | TokenKind::CommentLine
                            )
                        {
                            while *i <= j {
                                out.push(tokens[*i].clone());
                                *i += 1;
                            }
                        }
                        return;
                    }
                    _ => {}
                }
                out.push(t);
                *i += 1;
            }
        }
    }
}

fn inj_handle_if<'src>(
    tokens: &[Token<'src>],
    i: &mut usize,
    out: &mut Vec<Token<'src>>,
    config: &crate::config::Config,
) {
    out.push(tokens[*i].clone()); // `if`
    *i += 1;
    inj_copy_ws(tokens, i, out);
    if *i < tokens.len() && tokens[*i].kind == TokenKind::LParen {
        inj_copy_paren(tokens, i, out);
    }
    let j = inj_peek_non_ws_or_cmt(tokens, *i);
    if j < tokens.len() && tokens[j].kind == TokenKind::LBrace {
        // Already braced — still recurse inside so nested bodies are also handled.
        // Use comment-aware copy so trailing comments before `{` are preserved.
        inj_copy_ws_or_cmt(tokens, i, out);
        inj_copy_block(tokens, i, out, config);
    } else if j >= tokens.len() || tokens[j].kind == TokenKind::Semi {
        // Degenerate `if (cond);` — copy as-is.
        inj_copy_ws(tokens, i, out);
        if *i < tokens.len() {
            out.push(tokens[*i].clone());
            *i += 1;
        }
    } else {
        inj_copy_ws(tokens, i, out);
        out.push(inj_synthetic(TokenKind::LBrace, "{"));
        inj_copy_stmt(tokens, i, out, config);
        out.push(inj_synthetic(TokenKind::RBrace, "}"));
    }
    // Handle optional else.
    let j = inj_peek_non_ws(tokens, *i);
    if j < tokens.len() && tokens[j].kind == TokenKind::KwElse {
        inj_copy_ws(tokens, i, out);
        inj_handle_else(tokens, i, out, config);
    }
}

fn inj_handle_else<'src>(
    tokens: &[Token<'src>],
    i: &mut usize,
    out: &mut Vec<Token<'src>>,
    config: &crate::config::Config,
) {
    out.push(tokens[*i].clone()); // `else`
    *i += 1;
    inj_copy_ws(tokens, i, out);
    if *i >= tokens.len() {
        return;
    }
    match tokens[*i].kind {
        TokenKind::KwIf => inj_handle_if(tokens, i, out, config),
        TokenKind::LBrace => inj_copy_block(tokens, i, out, config),
        TokenKind::Semi => {
            out.push(tokens[*i].clone());
            *i += 1;
        }
        _ => {
            out.push(inj_synthetic(TokenKind::LBrace, "{"));
            inj_copy_stmt(tokens, i, out, config);
            out.push(inj_synthetic(TokenKind::RBrace, "}"));
        }
    }
}

/// Handle `for (…) body` or `while (…) body` (not do-while terminator).
fn inj_handle_ctrl<'src>(
    tokens: &[Token<'src>],
    i: &mut usize,
    out: &mut Vec<Token<'src>>,
    config: &crate::config::Config,
) {
    out.push(tokens[*i].clone()); // keyword
    *i += 1;
    inj_copy_ws(tokens, i, out);
    if *i < tokens.len() && tokens[*i].kind == TokenKind::LParen {
        inj_copy_paren(tokens, i, out);
    }
    let j = inj_peek_non_ws_or_cmt(tokens, *i);
    if j >= tokens.len() || tokens[j].kind == TokenKind::LBrace || tokens[j].kind == TokenKind::Semi
    {
        // Already braced, or `;` (do-while terminator / empty loop) — copy as-is.
        inj_copy_ws_or_cmt(tokens, i, out);
        if *i < tokens.len() {
            inj_item(tokens, i, out, config);
        }
    } else {
        inj_copy_ws(tokens, i, out);
        out.push(inj_synthetic(TokenKind::LBrace, "{"));
        inj_copy_stmt(tokens, i, out, config);
        out.push(inj_synthetic(TokenKind::RBrace, "}"));
    }
}

/// Dispatch one logical item from the token stream.
fn inj_item<'src>(
    tokens: &[Token<'src>],
    i: &mut usize,
    out: &mut Vec<Token<'src>>,
    config: &crate::config::Config,
) {
    if *i >= tokens.len() {
        return;
    }
    match tokens[*i].kind {
        TokenKind::KwIf if config.braces.add_braces_to_if => inj_handle_if(tokens, i, out, config),
        TokenKind::KwFor if config.braces.add_braces_to_for => {
            inj_handle_ctrl(tokens, i, out, config)
        }
        TokenKind::KwWhile if config.braces.add_braces_to_while => {
            inj_handle_ctrl(tokens, i, out, config)
        }
        TokenKind::KwElse if config.braces.add_braces_to_if => {
            inj_handle_else(tokens, i, out, config)
        }
        TokenKind::LBrace => inj_copy_block(tokens, i, out, config),
        _ => {
            out.push(tokens[*i].clone());
            *i += 1;
        }
    }
}

fn inject_braces_pass<'src>(
    tokens: &[Token<'src>],
    config: &crate::config::Config,
) -> Vec<Token<'src>> {
    let mut out = Vec::with_capacity(tokens.len() + 32);
    let mut i = 0;
    while i < tokens.len() {
        inj_item(tokens, &mut i, &mut out, config);
    }
    out
}

// ── Context ───────────────────────────────────────────────────────────────────

/// What opened the most recent `{`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BraceCtx {
    Block, // if/for/while/do/else/try/catch
    Type,  // struct/class/union/enum
    Namespace,
    Function, // function definition body
    Switch,   // switch statement body
    ExternC,  // extern "C" { } — no extra indentation
    Other,    // initializer list, lambda capture, etc.
}

struct Fmt<'src> {
    config: &'src Config,
    tokens: &'src [Token<'src>],
    pos: usize,

    output: String,
    /// True when the last character written was a newline.
    at_line_start: bool,
    indent_level: u32,
    /// Stack tracking what each `{` opened.
    brace_stack: Vec<BraceCtx>,
    /// Parallel to `brace_stack`: true when the corresponding `{` opened a
    /// flat large initializer that should be expanded one element per line.
    large_init_stack: Vec<bool>,
    /// Depth inside `(…)` — used to suppress newlines after `;` in for-headers.
    paren_depth: u32,
    /// Depth inside `[…]`.
    bracket_depth: u32,
    /// Pending blank lines to emit before the next meaningful token.
    blank_lines: u32,
    /// When true, the next Newline token seen in skip_ws was already emitted
    /// by the formatter (e.g. after `;` or `}`), so it must not be re-counted.
    skip_next_newline: bool,
    /// Set by `skip_ws()` to true when at least one `Whitespace` token was
    /// consumed between the previous token and the upcoming one.  Used by
    /// `SpaceOption::Preserve` to reproduce the source spacing.
    src_had_inline_ws: bool,
    /// The last non-whitespace, non-newline token kind we emitted.
    prev: Option<TokenKind>,
    /// Number of switch bodies we are currently inside — used to dedent case/default.
    switch_depth: u32,
    /// Per-switch stack tracking whether the current case body has added an
    /// extra indent level (only used when config.indent.indent_switch_case).
    case_body_stack: Vec<bool>,
    /// Current preprocessor #if nesting depth (used for pp_indent).
    pp_depth: u32,
    /// Stack of saved `indent_level` values at each `#if`/`#ifdef`/`#ifndef` entry.
    /// On `#else`/`#elif` we restore to the saved level so both branches start from
    /// the same code-brace depth; on `#endif` we pop without restoring (the last
    /// branch's accumulated depth is correct).
    pp_brace_stack: Vec<u32>,
    /// Number of open ternary `?` operators; used to detect ternary `:`.
    ternary_depth: u32,
    /// Set when `operator` keyword is emitted; cleared on the next non-ws token
    /// so that the overloaded operator symbol gets no surrounding space.
    after_operator_kw: bool,
    /// Set when an operator-overload symbol is emitted (i.e. `after_operator_kw`
    /// was true), so the following `(` is treated as a call paren (no space).
    last_was_operator_overload: bool,
    /// Number of class/struct/union bodies we are currently inside — used to dedent access specifiers.
    class_depth: u32,
    /// Set when `switch` keyword is emitted; cleared once its `{` is consumed.
    pending_switch: bool,
    /// Set when a type keyword (class/struct/union/enum) is emitted; cleared
    /// once its `{` is consumed. Needed because the `{` is often preceded by
    /// the type's name (an Ident), not the keyword itself.
    pending_type: bool,
    /// Set when `extern` is seen, kept through the following `LitStr` (`"C"`),
    /// consumed when `{` is reached to classify the block as `ExternC`.
    pending_extern_c: bool,
    /// Set when `case`/`default` keyword is emitted so the following `:` gets
    /// a newline after it instead of continuing on the same line.
    in_case_label: bool,
    /// Set after emitting a case/default label colon (`case X:` or `default:`).
    /// Used so that a `{` on the next line is treated as a block, not an
    /// initializer list.
    last_was_case_colon: bool,
    /// Set when `public`/`private`/`protected` is emitted so the following `:`
    /// gets a newline after it.
    in_access_label: bool,
    /// Set when a goto label identifier is emitted at column 0; causes the
    /// following `:` to emit a newline without applying normal spacing rules.
    in_goto_label: bool,
    /// When true, the next call to `space()` is suppressed and the flag is
    /// cleared. Used to suppress the space between a pointer `*`/`&` and the
    /// following identifier in `name` pointer-alignment mode.
    suppress_next_space: bool,
    /// Nesting depth inside template angle brackets `<…>`. Zero outside any
    /// template argument list.
    template_depth: u32,
    /// Set when the last non-whitespace token emitted was a template-closing `>`.
    /// Used to treat `>` like an identifier for call-paren spacing purposes.
    last_was_template_close: bool,
    /// Stack parallel to paren_depth: `true` if the corresponding `(` opened a
    /// C-style cast (next non-whitespace token inside was a type keyword).
    cast_paren_stack: Vec<bool>,
    /// Set when the last `)` closed a cast paren. Cleared by `set_prev`.
    last_was_cast_close: bool,
    /// Current output column (chars since last newline). Used to record
    /// opening-paren column so continuation params can be aligned.
    current_col: usize,
    /// Stack parallel to paren_depth: the column to align continuation lines
    /// to (i.e. the column right after the `(` was written).
    paren_col_stack: Vec<usize>,
    /// Parallel to paren_col_stack: when the `(` was the last non-whitespace
    /// on its line, stores `(true, col)` where `col` is the precomputed column
    /// that all continuation lines should start at.  `(false, 0)` means the
    /// `(` was not at EOL and column-alignment is used instead.
    paren_eol_stack: Vec<(bool, usize)>,
    /// Indentation column of the first non-whitespace token on the current
    /// line (set at the end of every `indent()` call, reset to 0 by `nl()`).
    line_indent_col: usize,
    /// Value of `paren_depth` at the time `indent()` was last called — used
    /// to count how many `(`s were opened on the current line.
    line_start_paren_depth: u32,
    /// Column to align continuation lines to after an `=` assignment operator.
    /// None when not inside an assignment RHS at statement level.
    assign_col: Option<usize>,
    /// True when the `=` that opened `assign_col` was the last non-whitespace
    /// on its line, meaning continuations use a regular indent.
    assign_eol: bool,

    // ── blank_line_after_var_decl_block state ─────────────────────────────────
    /// True while we are in the leading declaration run of a function body.
    in_var_decl_block: bool,
    /// True after a `;` at function scope; cleared when the next statement's
    /// first token is processed.
    at_func_stmt_start: bool,
    /// True once we have seen at least one declaration in the current function's
    /// leading block (prevents a spurious blank when the function opens with
    /// statements rather than declarations).
    saw_func_decl: bool,
    /// Set when the declaration run ends; causes flush_blank_lines to inject a
    /// blank line before the first non-declaration statement.
    force_blank_after_decls: bool,
}

impl<'src> Fmt<'src> {
    fn new(config: &'src Config, tokens: &'src [Token<'src>]) -> Self {
        Self {
            config,
            tokens,
            pos: 0,
            output: String::with_capacity(4096),
            at_line_start: true,
            indent_level: 0,
            brace_stack: Vec::new(),
            large_init_stack: Vec::new(),
            paren_depth: 0,
            bracket_depth: 0,
            blank_lines: 0,
            skip_next_newline: false,
            src_had_inline_ws: false,
            prev: None,
            switch_depth: 0,
            case_body_stack: Vec::new(),
            pp_depth: 0,
            pp_brace_stack: Vec::new(),
            ternary_depth: 0,
            after_operator_kw: false,
            last_was_operator_overload: false,
            class_depth: 0,
            pending_switch: false,
            pending_type: false,
            pending_extern_c: false,
            in_case_label: false,
            last_was_case_colon: false,
            in_access_label: false,
            in_goto_label: false,
            suppress_next_space: false,
            template_depth: 0,
            last_was_template_close: false,
            cast_paren_stack: Vec::new(),
            last_was_cast_close: false,
            current_col: 0,
            paren_col_stack: Vec::new(),
            paren_eol_stack: Vec::new(),
            line_indent_col: 0,
            line_start_paren_depth: 0,
            assign_col: None,
            assign_eol: false,
            in_var_decl_block: false,
            at_func_stmt_start: false,
            saw_func_decl: false,
            force_blank_after_decls: false,
        }
    }

    // ── Navigation ───────────────────────────────────────────────────────────

    fn advance(&mut self) -> Option<&'src Token<'src>> {
        let t = self.tokens.get(self.pos)?;
        self.pos += 1;
        Some(t)
    }

    /// Skip whitespace/newline tokens, counting blank lines.
    ///
    /// When `skip_next_newline` is set, the first Newline token is silently
    /// dropped because the formatter already emitted a newline for it (e.g.
    /// the `\n` the formatter writes after `;` or `}`).
    fn skip_ws(&mut self) {
        self.src_had_inline_ws = false;
        let mut synthetic_consumed = false;
        while let Some(t) = self.tokens.get(self.pos) {
            match t.kind {
                TokenKind::Whitespace => {
                    self.src_had_inline_ws = true;
                    self.pos += 1;
                }
                TokenKind::Newline => {
                    self.pos += 1;
                    if self.skip_next_newline && !synthetic_consumed {
                        synthetic_consumed = true;
                    } else {
                        self.blank_lines += 1;
                    }
                }
                _ => break,
            }
        }
        self.skip_next_newline = false;
    }

    /// Return the kind of the next non-whitespace/newline token without consuming it.
    fn peek_non_ws_kind(&self) -> Option<TokenKind> {
        let mut j = self.pos;
        while j < self.tokens.len() {
            match self.tokens[j].kind {
                TokenKind::Whitespace | TokenKind::Newline => j += 1,
                k => return Some(k),
            }
        }
        None
    }

    // ── Output helpers ────────────────────────────────────────────────────────

    fn write(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.output.push_str(s);
        self.at_line_start = s.ends_with('\n');
        if let Some(pos) = s.rfind('\n') {
            self.current_col = s[pos + 1..].chars().count();
        } else {
            self.current_col += s.chars().count();
        }
    }

    fn nl(&mut self) {
        // Strip trailing spaces from the current line before emitting the newline.
        while self.output.ends_with(' ') {
            self.output.pop();
        }
        self.output.push_str(self.config.newline_str());
        self.at_line_start = true;
        self.suppress_next_space = false;
        self.current_col = 0;
        self.line_indent_col = 0;
    }

    fn indent(&mut self) {
        self.line_start_paren_depth = self.paren_depth;
        if self.paren_depth > 0 {
            self.align_to_paren();
        } else if self.assign_col.is_some() {
            self.align_to_assign();
        } else {
            let unit = self.config.indent_str();
            for _ in 0..self.indent_level {
                self.output.push_str(&unit);
                self.current_col += unit.len();
            }
            if self.indent_level > 0 {
                self.at_line_start = false;
            }
        }
        self.line_indent_col = self.current_col;
    }

    fn align_to_paren(&mut self) {
        // When `(` was the last non-whitespace before the newline, aligning to
        // its column would push continuation far right.  Instead we use the
        // precomputed `eol_col` stored in `paren_eol_stack`: that column is
        // `line_indent_col + parens_opened_on_that_line * indent_width`, which
        // matches uncrustify's behaviour for both simple and nested call sites.
        if let Some((eol, eol_col)) = self.paren_eol_stack.last_mut() {
            if !*eol && self.output.trim_end().ends_with('(') {
                *eol = true;
            }
            if *eol {
                let col = *eol_col;
                for _ in 0..col {
                    self.output.push(' ');
                }
                self.current_col = col;
                self.at_line_start = false;
                return;
            }
        }
        if let Some(&col) = self.paren_col_stack.last() {
            for _ in 0..col {
                self.output.push(' ');
            }
            self.current_col = col;
            if col > 0 {
                self.at_line_start = false;
            }
        }
    }

    fn align_to_assign(&mut self) {
        // When `=` was the last non-whitespace before the newline, aligning to
        // its column would leave no room for content. Use a regular indent instead.
        if !self.assign_eol && self.output.trim_end().ends_with('=') {
            self.assign_eol = true;
        }
        if self.assign_eol {
            let unit = self.config.indent_str();
            for _ in 0..=self.indent_level {
                self.output.push_str(&unit);
                self.current_col += unit.len();
            }
            self.at_line_start = false;
            return;
        }
        if let Some(col) = self.assign_col {
            for _ in 0..col {
                self.output.push(' ');
            }
            self.current_col = col;
            if col > 0 {
                self.at_line_start = false;
            }
        }
    }

    /// Update `self.prev` and clear the template-close flag in one step.
    /// Template-close arms must NOT use this — they set the two fields directly.
    fn set_prev(&mut self, kind: TokenKind) {
        self.prev = Some(kind);
        self.last_was_template_close = false;
        self.last_was_cast_close = false;
        // When `after_operator_kw` is true, we just emitted the overload symbol;
        // record that so the following `(` gets call-paren spacing.
        // For `operator[]` and `operator()`, the closing `]`/`)` must keep the
        // flag alive so that the parameter-list `(` is also treated as a call paren.
        self.last_was_operator_overload = self.after_operator_kw
            || (self.last_was_operator_overload
                && matches!(kind, TokenKind::RBracket | TokenKind::RParen));
        self.after_operator_kw = false;
        self.last_was_case_colon = false;
    }

    fn space(&mut self) {
        if self.suppress_next_space {
            self.suppress_next_space = false;
            return;
        }
        if !self.at_line_start && !self.output.ends_with(' ') {
            self.output.push(' ');
            self.current_col += 1;
        }
    }

    /// Emit pending blank lines, capped to `max_blank_lines`.
    fn flush_blank_lines(&mut self) {
        if self.force_blank_after_decls {
            self.force_blank_after_decls = false;
            // When not at line start (e.g. a trailing /* comment */ follows the
            // last decl), one nl() only terminates the current line; we need a
            // second nl() to produce an actual blank line.
            let min_lines = if self.at_line_start { 1 } else { 2 };
            if self.blank_lines < min_lines {
                self.blank_lines = min_lines;
            }
        }
        let max = self.config.newlines.max_blank_lines as u32;
        if max > 0 {
            let emit = self.blank_lines.min(max);
            for _ in 0..emit {
                self.nl();
            }
        }
        self.blank_lines = 0;
    }

    /// True for token kinds that can open a variable/type declaration at
    /// function scope. Used by blank_line_after_var_decl_block.
    fn is_decl_start(kind: TokenKind) -> bool {
        matches!(
            kind,
            TokenKind::Keyword
                | TokenKind::KwStruct
                | TokenKind::KwClass
                | TokenKind::KwUnion
                | TokenKind::KwEnum
                | TokenKind::KwTypename
                | TokenKind::KwTypedef
        )
    }

    /// True when we're at statement start and the current token is an Ident that
    /// begins a user-defined-type declaration (`TypeName varName;`).  In C/C++,
    /// `ident ident` at statement scope is always a declaration — there is no
    /// non-declaration statement in that form.  Scans ahead skipping WS and any
    /// leading `*`/`&` (pointer/reference declarators) to find the variable name.
    fn ident_starts_decl(&self) -> bool {
        // Scan forward to decide if this Ident is the start of a declaration.
        // Patterns we accept:
        //   TypeName varName         — user typedef: Ident Ident
        //   ATTR_MACRO const T* var  — attribute macro + qualifiers: Ident Keyword+ Ident
        // We stop at newlines because declarations are always on one line.
        let mut i = self.pos;
        loop {
            let Some(tk) = self.tokens.get(i) else {
                return false;
            };
            match tk.kind {
                // Stop at newlines: `TypeName varName` declarations are always
                // on one line. Skipping newlines would misidentify adjacent macro
                // calls (e.g. `EXPECT_ABORT_BEGIN\n    TEST_ASSERT(...)`) as decls.
                TokenKind::Newline => return false,
                TokenKind::Whitespace | TokenKind::Star => i += 1,
                // A bare Ident following is the variable name — declaration confirmed.
                TokenKind::Ident => return true,
                // A Keyword (const, volatile, unsigned, etc.) after the leading Ident
                // means this is an attribute-macro + qualifier pattern like
                // `UNITY_PTR_ATTRIBUTE const float* p` — keep scanning.
                TokenKind::Keyword
                | TokenKind::KwStruct
                | TokenKind::KwClass
                | TokenKind::KwUnion
                | TokenKind::KwEnum
                | TokenKind::KwTypename => i += 1,
                _ => return false,
            }
        }
    }

    /// Emit the newline/space after a `}` based on what follows.
    /// Called from both the RBrace arm and the LBrace empty-body collapse path.
    fn emit_post_brace_spacing(
        &mut self,
        ctx: BraceCtx,
        next_kind: Option<TokenKind>,
        source_line: u32,
    ) {
        let semi_follows = next_kind == Some(TokenKind::Semi);
        let typedef_name =
            matches!(ctx, BraceCtx::Type) && matches!(next_kind, Some(TokenKind::Ident));
        let cuddle = match next_kind {
            Some(TokenKind::KwElse) => {
                self.config.braces.cuddle_else && !self.config.newlines.nl_brace_else
            }
            Some(TokenKind::KwCatch) => self.config.braces.cuddle_catch,
            Some(TokenKind::KwWhile) => matches!(ctx, BraceCtx::Block),
            _ => false,
        };

        if semi_follows {
            // `;` will be written by the Semi arm directly.
        } else if typedef_name || (cuddle && matches!(self.config.braces.style, BraceStyle::Kr)) {
            self.space();
        } else if cuddle
            && matches!(self.config.braces.style, BraceStyle::Stroustrup)
            && next_kind == Some(TokenKind::KwElse)
        {
            self.nl();
            self.skip_next_newline = true;
        } else if self.peek_inline_comment(source_line) {
            // inline comment — let CommentLine close the line
        } else {
            self.nl();
            self.skip_next_newline = true;
        }
    }

    /// Called once per token, before the token is written, to advance the
    /// blank_line_after_var_decl_block state machine.
    fn check_var_decl_transition(&mut self, kind: TokenKind) {
        if !self.config.newlines.blank_line_after_var_decl_block {
            return;
        }
        if !self.at_func_stmt_start || !self.in_var_decl_block {
            return;
        }
        // Comments are handled by two rules:
        //   1. Inline trailing comments (not at line start) are always transparent.
        //   2. Standalone comments (at line start) are transparent when they are
        //      *between* declarations — i.e. more declarations follow — so that
        //      section comments like `/* WHEN ... */` don't split the block.
        //      A standalone comment ends the block only when it precedes code.
        if matches!(kind, TokenKind::CommentLine | TokenKind::CommentBlock) {
            if !self.at_line_start {
                return; // inline trailing comment — never ends the block
            }
            if !self.saw_func_decl {
                return; // before any declaration — preamble comment is transparent
            }
            if self.comment_precedes_decl() {
                return; // comment sits between declarations — transparent
            }
            // Otherwise fall through: standalone comment before code ends the block.
        }
        self.at_func_stmt_start = false;
        let is_decl =
            Self::is_decl_start(kind) || (kind == TokenKind::Ident && self.ident_starts_decl());
        if is_decl {
            self.saw_func_decl = true;
        } else {
            self.in_var_decl_block = false;
            if self.saw_func_decl {
                self.force_blank_after_decls = true;
            }
        }
    }

    /// Ensure we're at the start of a fresh line (emit newline + indent if not).
    fn ensure_own_line(&mut self) {
        if !self.at_line_start {
            self.nl();
        }
        self.indent();
    }

    /// For KR/Stroustrup enforcement: trim all trailing whitespace from the
    /// output so the next token can be appended to the end of the previous
    /// content line (e.g. `if (cond)\n    {` → `if (cond) {`).
    fn trim_to_prev_line_end(&mut self) {
        let new_len = self
            .output
            .trim_end_matches(|c: char| c.is_ascii_whitespace())
            .len();
        self.output.truncate(new_len);
        if let Some(pos) = self.output.rfind('\n') {
            self.current_col = self.output[pos + 1..].chars().count();
        } else {
            self.current_col = self.output.chars().count();
        }
        self.at_line_start = false;
    }

    // ── Cast detection ────────────────────────────────────────────────────────

    /// True when the token immediately before the current `(` is a sizeof-like
    /// operator keyword — meaning the `(` is NOT a cast paren.
    fn prev_is_sizeof_like(&self) -> bool {
        // self.pos is one past `(`; self.pos-1 is `(` itself; scan from self.pos-2.
        if self.pos < 2 {
            return false;
        }
        let mut i = self.pos - 2;
        loop {
            match self.tokens[i].kind {
                TokenKind::Whitespace | TokenKind::Newline => {
                    if i == 0 {
                        return false;
                    }
                    i -= 1;
                }
                _ => {
                    let tok = &self.tokens[i];
                    // Any identifier before `(` is a function call, never a cast.
                    if tok.kind == TokenKind::Ident {
                        return true;
                    }
                    return matches!(
                        tok.lexeme,
                        "sizeof" | "alignof" | "alignas" | "__alignof__" | "decltype" | "typeid"
                    );
                }
            }
        }
    }

    /// True if the tokens from `self.pos` up to and including a matching `)`
    /// look like a C-style cast type: optional cv/elaborated-type keywords,
    /// followed by exactly one type keyword or identifier, followed by zero or
    /// more `*`/`&`, followed by `)`.  Also accepts user-defined type names
    /// (bare `Ident`), not just built-in keywords.
    fn next_is_type_kw(&self) -> bool {
        let mut i = self.pos;
        let skip_ws = |mut j: usize| -> usize {
            while j < self.tokens.len()
                && matches!(
                    self.tokens[j].kind,
                    TokenKind::Whitespace | TokenKind::Newline
                )
            {
                j += 1;
            }
            j
        };
        i = skip_ws(i);

        // Accept any number of qualifier/struct/class/… keywords before the
        // core type name — but we need at least one type-like token overall.
        let mut saw_type = false;
        while i < self.tokens.len() {
            let k = self.tokens[i].kind;
            if Self::is_decl_start(k) {
                saw_type = true;
                i += 1;
                i = skip_ws(i);
            } else if k == TokenKind::Ident {
                // User-defined type name (e.g. MyStruct, size_t, uint32_t).
                saw_type = true;
                i += 1;
                i = skip_ws(i);
                break; // only one ident in a cast type
            } else {
                break;
            }
        }

        if !saw_type {
            return false;
        }

        // Optional pointer / reference decorators
        while i < self.tokens.len()
            && matches!(self.tokens[i].kind, TokenKind::Star | TokenKind::Amp)
        {
            i += 1;
            i = skip_ws(i);
        }

        // Must end with `)`
        matches!(self.tokens.get(i).map(|t| t.kind), Some(TokenKind::RParen))
    }

    // ── Inline-comment detection ──────────────────────────────────────────────

    /// True if the tokens from `self.pos` match the function-pointer declarator
    /// pattern `(*Name)` or `(&Name)` — i.e. `*`/`&` then an identifier then `)`.
    ///
    /// This distinguishes `void (*Fn)(int)` from `memset(&data, 0, n)` where the
    /// `(` is merely followed by an address-of expression, not a declarator.
    fn next_is_fn_ptr_declarator(&self) -> bool {
        let skip_ws = |mut j: usize| -> usize {
            while j < self.tokens.len()
                && matches!(
                    self.tokens[j].kind,
                    TokenKind::Whitespace | TokenKind::Newline
                )
            {
                j += 1;
            }
            j
        };
        let mut i = skip_ws(self.pos);
        // Must start with * or & (function-pointer or C++ reference-to-function).
        if !matches!(
            self.tokens.get(i).map(|t| t.kind),
            Some(TokenKind::Star | TokenKind::Amp)
        ) {
            return false;
        }
        i += 1;
        i = skip_ws(i);
        // Then an identifier (the function-pointer/reference name)
        if !matches!(self.tokens.get(i).map(|t| t.kind), Some(TokenKind::Ident)) {
            return false;
        }
        i += 1;
        i = skip_ws(i);
        // Then `)` …
        if !matches!(self.tokens.get(i).map(|t| t.kind), Some(TokenKind::RParen)) {
            return false;
        }
        i += 1;
        i = skip_ws(i);
        // … immediately followed by `(` (the parameter list).
        // This distinguishes `void (*fp)(int)` from a call like `foo(&x)` where
        // `)` closes the argument list and is followed by `;`, `,`, `)`, etc.
        matches!(self.tokens.get(i).map(|t| t.kind), Some(TokenKind::LParen))
    }

    /// True if the next token (skipping only `Whitespace`, not `Newline`) is a
    /// `CommentLine` or `CommentBlock` whose source line matches `source_line`.
    fn peek_inline_comment(&self, source_line: u32) -> bool {
        let mut i = self.pos;
        while i < self.tokens.len() && self.tokens[i].kind == TokenKind::Whitespace {
            i += 1;
        }
        matches!(
            self.tokens.get(i),
            Some(t) if matches!(t.kind, TokenKind::CommentLine | TokenKind::CommentBlock)
                && t.span.line == source_line
        )
    }

    /// True when the next non-whitespace token is a `CommentLine` on `source_line`.
    fn peek_inline_line_comment(&self, source_line: u32) -> bool {
        let mut i = self.pos;
        while i < self.tokens.len() && self.tokens[i].kind == TokenKind::Whitespace {
            i += 1;
        }
        matches!(
            self.tokens.get(i),
            Some(t) if t.kind == TokenKind::CommentLine && t.span.line == source_line
        )
    }

    /// Scans forward from `self.pos` skipping whitespace, newlines, and any
    /// comments, then returns true when the first real token is a declaration
    /// start.  Used to decide whether a standalone comment between declarations
    /// is transparent (followed by another declaration) or ends the var-decl
    /// block (followed by code).
    fn comment_precedes_decl(&self) -> bool {
        let mut i = self.pos;
        loop {
            let Some(tk) = self.tokens.get(i) else {
                return false;
            };
            match tk.kind {
                TokenKind::Whitespace | TokenKind::Newline => i += 1,
                TokenKind::CommentLine | TokenKind::CommentBlock => i += 1,
                kind => {
                    return Self::is_decl_start(kind)
                        || (kind == TokenKind::Ident && {
                            // Temporarily advance past this ident to run ident_starts_decl
                            // logic inline (we can't call self.ident_starts_decl() since it
                            // reads from self.pos).
                            let mut j = i + 1;
                            loop {
                                let Some(t2) = self.tokens.get(j) else {
                                    break false;
                                };
                                match t2.kind {
                                    TokenKind::Newline => break false,
                                    TokenKind::Whitespace | TokenKind::Star => j += 1,
                                    TokenKind::Ident => break true,
                                    TokenKind::Keyword
                                    | TokenKind::KwStruct
                                    | TokenKind::KwClass
                                    | TokenKind::KwUnion
                                    | TokenKind::KwEnum
                                    | TokenKind::KwTypename => j += 1,
                                    _ => break false,
                                }
                            }
                        });
                }
            }
        }
    }

    // ── Small initializer detection ───────────────────────────────────────────

    /// Scans forward from `self.pos` (the token immediately after `{`) looking
    /// for the matching `}`.  Returns `Some(rbrace_index)` when the initializer
    /// has no nested braces and contains at most 16 non-whitespace tokens, so
    /// it can safely be kept on a single line.  Returns `None` otherwise.
    fn small_initializer_end(&self) -> Option<usize> {
        const MAX_TOKENS: usize = 16;
        let mut count = 0;
        for (offset, tk) in self.tokens[self.pos..].iter().enumerate() {
            match tk.kind {
                // Nested brace or source newline: not a small single-line init.
                TokenKind::LBrace | TokenKind::Newline => return None,
                TokenKind::RBrace => return Some(self.pos + offset),
                TokenKind::Whitespace => {}
                _ => {
                    count += 1;
                    if count > MAX_TOKENS {
                        return None;
                    }
                }
            }
        }
        None
    }

    /// Scans forward from `self.pos` looking for the matching `}`.
    /// Returns `Some(rbrace_index)` only when the initializer is flat (no
    /// nested `{`) and written on a single source line (no `Newline` tokens).
    /// Returns `None` for multi-line or nested initializers so that the source
    /// grouping is preserved instead of being blown out one-element-per-line.
    fn large_flat_initializer_end(&self) -> Option<usize> {
        for (offset, tk) in self.tokens[self.pos..].iter().enumerate() {
            match tk.kind {
                TokenKind::LBrace | TokenKind::Newline => return None,
                TokenKind::RBrace => return Some(self.pos + offset),
                _ => {}
            }
        }
        None
    }

    // ── Brace context inference ───────────────────────────────────────────────

    /// Returns the effective previous token kind for brace-context inference,
    /// looking through any trailing comments so that `if (cond) /* note */ {`
    /// is classified the same as `if (cond) {`.
    fn prev_through_comments(&self) -> Option<TokenKind> {
        if !matches!(
            self.prev,
            Some(TokenKind::CommentBlock | TokenKind::CommentLine | TokenKind::PreprocLine)
        ) {
            return self.prev;
        }
        // self.pos is one past the LBrace; self.pos-1 is LBrace.  Scan backward.
        if self.pos < 2 {
            return None;
        }
        let mut i = self.pos - 2;
        loop {
            match self.tokens[i].kind {
                TokenKind::Whitespace
                | TokenKind::Newline
                | TokenKind::CommentBlock
                | TokenKind::CommentLine
                | TokenKind::PreprocLine => {
                    if i == 0 {
                        return None;
                    }
                    i -= 1;
                }
                k => return Some(k),
            }
        }
    }

    /// Returns `true` when the `RParen` immediately before `{` looks like it
    /// closes a macro/function *call* at statement level rather than a function
    /// *definition* parameter list.
    ///
    /// Heuristic: find the `(` that matches the `)`, then the identifier before
    /// it, then look one step further.  If what precedes the identifier is a
    /// statement boundary (`{`, `}`, `;`, preprocline) we are at the start of a
    /// statement with no return-type — this is a macro call, not a definition.
    /// If it is a type keyword, another identifier (return type), `*`, `&`, or
    /// `>` (template close), it is a function definition.
    ///
    /// The check is skipped when we are inside a class body (`class_depth > 0`)
    /// because constructors and inline methods have no return type yet still
    /// qualify as definitions.
    fn rparen_looks_like_call(&self) -> bool {
        if self.class_depth > 0 {
            return false; // inside a class: always a definition
        }
        if self.pos < 2 {
            return false;
        }
        // Start one token before the current LBrace.
        let mut i = self.pos - 2;
        // Skip whitespace / comments between ) and {.
        while matches!(
            self.tokens[i].kind,
            TokenKind::Whitespace
                | TokenKind::Newline
                | TokenKind::CommentBlock
                | TokenKind::CommentLine
        ) {
            if i == 0 {
                return false;
            }
            i -= 1;
        }
        if self.tokens[i].kind != TokenKind::RParen {
            return false;
        }
        // Find the matching `(`.
        let mut depth = 1usize;
        loop {
            if i == 0 {
                return false;
            }
            i -= 1;
            match self.tokens[i].kind {
                TokenKind::RParen => depth += 1,
                TokenKind::LParen => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        // i = the `(`.  Find the name token immediately before it.
        while i > 0 {
            i -= 1;
            if !matches!(
                self.tokens[i].kind,
                TokenKind::Whitespace | TokenKind::Newline
            ) {
                break;
            }
        }
        // i = the function/macro name token.
        // Now scan backward past the qualified name (ColonColon, Tilde) to find
        // the token that precedes the entire name.
        loop {
            if i == 0 {
                return false; // start of file without return type: treat as fn def
            }
            i -= 1;
            match self.tokens[i].kind {
                TokenKind::Whitespace | TokenKind::Newline => continue,
                // Parts of a qualified/scoped name — keep scanning.
                TokenKind::ColonColon | TokenKind::Tilde => continue,
                // A return type — this is a function definition.
                TokenKind::Ident
                | TokenKind::Keyword
                | TokenKind::KwStruct
                | TokenKind::KwClass
                | TokenKind::KwUnion
                | TokenKind::KwEnum
                | TokenKind::KwTypename
                | TokenKind::Star
                | TokenKind::Amp
                | TokenKind::Gt
                | TokenKind::RParen => return false,
                // Statement boundary with no preceding return type — macro call.
                TokenKind::Semi
                | TokenKind::LBrace
                | TokenKind::RBrace
                | TokenKind::PreprocLine => {
                    return true;
                }
                _ => return false, // conservative: treat unknown context as fn def
            }
        }
    }

    /// Returns `true` when the `RParen` immediately before `{` closes a
    /// control-flow construct (`if`, `while`, `for`, `switch`) rather than a
    /// function parameter list.  Scans backward past the `)…(` group and checks
    /// the keyword before the `(`.
    fn rparen_closes_ctrl_flow(&self) -> bool {
        if self.pos < 2 {
            return false;
        }
        let mut i = self.pos - 2; // token before LBrace
                                  // skip whitespace / comments between ) and {
        while matches!(
            self.tokens[i].kind,
            TokenKind::Whitespace
                | TokenKind::Newline
                | TokenKind::CommentBlock
                | TokenKind::CommentLine
        ) {
            if i == 0 {
                return false;
            }
            i -= 1;
        }
        if self.tokens[i].kind != TokenKind::RParen {
            return false;
        }
        // find the matching '('
        let mut depth = 1usize;
        loop {
            if i == 0 {
                return false;
            }
            i -= 1;
            match self.tokens[i].kind {
                TokenKind::RParen => depth += 1,
                TokenKind::LParen => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        // skip whitespace before '('
        while i > 0 {
            i -= 1;
            if !matches!(
                self.tokens[i].kind,
                TokenKind::Whitespace | TokenKind::Newline
            ) {
                break;
            }
        }
        matches!(
            self.tokens[i].kind,
            TokenKind::KwIf | TokenKind::KwWhile | TokenKind::KwFor | TokenKind::KwSwitch
        )
    }

    fn infer_brace_ctx(&self) -> BraceCtx {
        let prev = match self.prev_through_comments() {
            Some(k) => k,
            None => return BraceCtx::Other,
        };
        match prev {
            TokenKind::LitStr if self.pending_extern_c => BraceCtx::ExternC,
            TokenKind::KwNamespace => BraceCtx::Namespace,
            TokenKind::KwStruct | TokenKind::KwClass | TokenKind::KwUnion | TokenKind::KwEnum => {
                BraceCtx::Type
            }
            TokenKind::RParen => {
                if self.pending_switch {
                    BraceCtx::Switch
                } else if self.rparen_closes_ctrl_flow() || self.rparen_looks_like_call() {
                    BraceCtx::Block
                } else {
                    BraceCtx::Function
                }
            }
            TokenKind::KwElse | TokenKind::KwDo | TokenKind::KwTry => BraceCtx::Block,
            TokenKind::Ident | TokenKind::Gt => {
                // Ident: could be a named type `class Foo {` or a function body.
                // Gt: template specialization `class Foo<T> {`.
                if self.pending_type {
                    BraceCtx::Type
                } else {
                    BraceCtx::Function
                }
            }
            TokenKind::Colon => {
                // `class Foo : public Bar {` — colon ends the base-class list.
                if self.pending_type {
                    BraceCtx::Type
                } else if self.last_was_case_colon {
                    // `case X: {` or `default: {` — block following a case label.
                    BraceCtx::Block
                } else {
                    BraceCtx::Other
                }
            }
            // After `=`, `(`, `,`, `{` → initializer-list style
            TokenKind::Eq
            | TokenKind::PlusEq
            | TokenKind::MinusEq
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Comma => BraceCtx::Other,
            _ => BraceCtx::Other,
        }
    }

    // ── Pointer/reference declarator detection ────────────────────────────────

    /// Heuristic: a `*` or `&` is a declarator (not multiplication/address-of)
    /// when the preceding non-whitespace token is a definite type-introducing
    /// token: a type keyword, another `*`/`&` (chained pointers), `)` (cast or
    /// function-pointer return type), `>` (template instantiation), or
    /// `typename`/`struct`/`class`/`union`/`enum`.
    fn is_ptr_decl_context(&self) -> bool {
        if matches!(
            self.prev,
            Some(
                TokenKind::Keyword
                    | TokenKind::KwStruct
                    | TokenKind::KwClass
                    | TokenKind::KwUnion
                    | TokenKind::KwEnum
                    | TokenKind::KwTypename
                    | TokenKind::Star
                    | TokenKind::Amp
                    | TokenKind::Gt
            )
        ) {
            return true;
        }
        // `)` followed by `*`/`&`: never a pointer declarator after a cast.
        // `(type)*ptr` is cast + dereference; `(type*)` has the `*` inside.
        // Only non-cast `)` (e.g. function return type in a fn-ptr context)
        // could introduce a declarator, but those are handled by the
        // fn-ptr-declarator check in the LParen arm, not here.
        if self.prev == Some(TokenKind::RParen) {
            return false;
        }
        // An identifier (user-defined type) followed by `*`/`&` is a pointer
        // declarator only when the tokens after the operator look like a name,
        // not an expression.  Heuristic: skip consecutive `*`/`&`/whitespace,
        // then require an identifier or keyword followed by a declaration-ending
        // token (`;`, `,`, `)`, `=`, `[`, `{`).
        if self.prev == Some(TokenKind::Ident) {
            return self.star_after_ident_is_decl();
        }
        false
    }

    fn star_after_ident_is_decl(&self) -> bool {
        // ── backward check ───────────────────────────────────────────────────
        // Scan back past the `*` and the preceding Ident to find the token
        // that appeared before the type-name.  If that token unambiguously
        // belongs to an expression (assignment, arithmetic, comparison, …)
        // then `*` is multiplication, not a pointer declarator.
        //
        // self.pos is one past the `*` token.
        if self.pos >= 1 {
            let mut b = self.pos - 1; // index of the `*`
                                      // skip the `*` itself and any whitespace/newlines before the Ident
            while b > 0
                && matches!(
                    self.tokens[b].kind,
                    TokenKind::Whitespace | TokenKind::Newline | TokenKind::Star | TokenKind::Amp
                )
            {
                b -= 1;
            }
            // b should now be the Ident; step past it
            if b > 0 && self.tokens[b].kind == TokenKind::Ident {
                b -= 1;
            }
            // skip whitespace before the Ident
            while b > 0
                && matches!(
                    self.tokens[b].kind,
                    TokenKind::Whitespace | TokenKind::Newline
                )
            {
                b -= 1;
            }
            // If the token before the Ident is an expression operator, this is
            // multiplication, not a declaration.
            let before = self.tokens[b].kind;
            let is_expr_op = matches!(
                before,
                TokenKind::Eq          // assignment: r = a * b
                    | TokenKind::Plus
                    | TokenKind::Minus
                    | TokenKind::Slash
                    | TokenKind::Percent
                    | TokenKind::Pipe
                    | TokenKind::Caret
                    | TokenKind::LtLt
                    | TokenKind::GtGt
                    | TokenKind::EqEq
                    | TokenKind::BangEq
                    | TokenKind::Lt
                    | TokenKind::LtEq
                    | TokenKind::GtEq
                    | TokenKind::AmpAmp
                    | TokenKind::PipePipe
                    | TokenKind::Question
                    | TokenKind::PlusEq   // compound assignments
                    | TokenKind::MinusEq
                    | TokenKind::StarEq
                    | TokenKind::SlashEq
                    | TokenKind::PercentEq
                    | TokenKind::AmpEq
                    | TokenKind::PipeEq
                    | TokenKind::CaretEq
                    | TokenKind::LtLtEq
                    | TokenKind::GtGtEq
                    | TokenKind::Arrow    // member access: p->field & MASK
                    | TokenKind::Dot      // member access: s.field & MASK
                    | TokenKind::ArrowStar
                    | TokenKind::DotStar
            ) || matches!(
                before,
                TokenKind::KwReturn | TokenKind::KwCase | TokenKind::KwThrow
            )
                // `(` preceded by an expression op means we're in an expression subgroup
                || (before == TokenKind::LParen && b > 0 && {
                    let mut bb = b - 1;
                    while bb > 0
                        && matches!(
                            self.tokens[bb].kind,
                            TokenKind::Whitespace | TokenKind::Newline
                        )
                    {
                        bb -= 1;
                    }
                    let outer = self.tokens[bb].kind;
                    matches!(
                        outer,
                        TokenKind::Eq
                            | TokenKind::Plus
                            | TokenKind::Minus
                            | TokenKind::Slash
                            | TokenKind::Percent
                            | TokenKind::Pipe
                            | TokenKind::Caret
                            | TokenKind::LtLt
                            | TokenKind::GtGt
                            | TokenKind::EqEq
                            | TokenKind::BangEq
                            | TokenKind::Lt
                            | TokenKind::LtEq
                            | TokenKind::GtEq
                            | TokenKind::AmpAmp
                            | TokenKind::PipePipe
                            | TokenKind::PlusEq
                            | TokenKind::MinusEq
                            | TokenKind::StarEq
                            | TokenKind::SlashEq
                            | TokenKind::PercentEq
                            | TokenKind::AmpEq
                            | TokenKind::PipeEq
                            | TokenKind::CaretEq
                            | TokenKind::LtLtEq
                            | TokenKind::GtGtEq
                            | TokenKind::Comma
                            | TokenKind::LParen
                            | TokenKind::Bang   // !(key_flag & MASK)
                            | TokenKind::Tilde  // ~(key_flag & MASK)
                            | TokenKind::KwIf
                            | TokenKind::KwWhile
                            | TokenKind::KwFor
                            | TokenKind::KwSwitch
                            | TokenKind::KwReturn
                            | TokenKind::KwCase
                            | TokenKind::KwThrow
                    )
                });
            if is_expr_op {
                return false;
            }
        }

        // ── forward check ────────────────────────────────────────────────────
        // After the `*`, the tokens must look like a declarator name, not an
        // expression operand.
        let mut i = self.pos;
        // skip additional pointer/ref operators and whitespace
        while i < self.tokens.len() {
            match self.tokens[i].kind {
                TokenKind::Star | TokenKind::Amp | TokenKind::Whitespace | TokenKind::Newline => {
                    i += 1;
                }
                _ => break,
            }
        }
        // skip optional `const`/`volatile`/`restrict` after the stars
        while i < self.tokens.len()
            && matches!(self.tokens[i].kind, TokenKind::Keyword)
            && matches!(
                self.tokens[i].lexeme,
                "const" | "volatile" | "restrict" | "__restrict" | "__restrict__"
            )
        {
            i += 1;
            while i < self.tokens.len()
                && matches!(
                    self.tokens[i].kind,
                    TokenKind::Whitespace | TokenKind::Newline
                )
            {
                i += 1;
            }
        }
        // skip a qualified name (Ident :: Ident ...)
        let mut found_name = false;
        let mut last_was_operator_kw = false;
        while i < self.tokens.len() {
            match self.tokens[i].kind {
                TokenKind::Ident => {
                    last_was_operator_kw = false;
                    found_name = true;
                    i += 1;
                }
                TokenKind::Keyword => {
                    // Only `operator` can appear as (part of) a declarator name;
                    // all other keywords (sizeof, return, …) indicate an expression.
                    if self.tokens[i].lexeme != "operator" {
                        break;
                    }
                    last_was_operator_kw = true;
                    found_name = true;
                    i += 1;
                }
                TokenKind::ColonColon => {
                    last_was_operator_kw = false;
                    i += 1;
                }
                TokenKind::Whitespace | TokenKind::Newline | TokenKind::PreprocLine => {
                    i += 1;
                }
                _ => break,
            }
        }
        if !found_name {
            return false;
        }
        // If the name ended with `operator`, skip the overloaded symbol so the
        // following `(` is seen as the declaration terminator.
        if last_was_operator_kw {
            while i < self.tokens.len()
                && matches!(
                    self.tokens[i].kind,
                    TokenKind::Whitespace | TokenKind::Newline
                )
            {
                i += 1;
            }
            // Skip one operator-symbol token (=, +=, ==, [], (), etc.)
            let op_kind = self.tokens.get(i).map(|t| t.kind);
            if op_kind.is_some_and(|k| {
                k.is_binary_op()
                    || matches!(
                        k,
                        TokenKind::PlusPlus
                            | TokenKind::MinusMinus
                            | TokenKind::Bang
                            | TokenKind::Tilde
                            | TokenKind::LParen
                            | TokenKind::LBracket
                    )
            }) {
                i += 1;
                // For `operator()` and `operator[]` also skip the closing bracket.
                let closing = self.tokens.get(i).map(|t| t.kind);
                if matches!(closing, Some(TokenKind::RParen | TokenKind::RBracket)) {
                    i += 1;
                }
            }
        }
        while i < self.tokens.len()
            && matches!(
                self.tokens[i].kind,
                TokenKind::Whitespace
                    | TokenKind::Newline
                    | TokenKind::PreprocLine
                    | TokenKind::CommentBlock
                    | TokenKind::CommentLine
            )
        {
            i += 1;
        }
        // declaration-terminating tokens
        matches!(
            self.tokens.get(i).map(|t| t.kind),
            Some(
                TokenKind::Semi
                    | TokenKind::Comma
                    | TokenKind::RParen
                    | TokenKind::Eq
                    | TokenKind::LBracket
                    | TokenKind::LBrace
                    | TokenKind::LParen // function-pointer: Type (*fn)(...)
            )
        )
    }

    // ── Template angle-bracket detection ─────────────────────────────────────

    /// Returns true when the `<` just consumed looks like the opening of a
    /// template argument list rather than a less-than comparison.
    ///
    /// Scans forward from `self.pos` (the token immediately after `<`).
    /// Only tokens that can legally appear in a template argument list are
    /// permitted; the first unexpected token causes an early `false` return.
    fn looks_like_template_open(&self) -> bool {
        let mut i = self.pos;
        let mut depth: u32 = 1;
        let mut scanned = 0u32;
        while i < self.tokens.len() && scanned < 256 {
            scanned += 1;
            match self.tokens[i].kind {
                // Whitespace is irrelevant to the heuristic.
                TokenKind::Whitespace | TokenKind::Newline => {}
                // Type-like content: names, scoping, pointer/ref modifiers,
                // separators, and non-type literal parameters.
                TokenKind::Ident
                | TokenKind::Keyword
                | TokenKind::KwStruct
                | TokenKind::KwClass
                | TokenKind::KwUnion
                | TokenKind::KwEnum
                | TokenKind::KwTemplate
                | TokenKind::KwTypename
                | TokenKind::KwUsing
                | TokenKind::ColonColon
                | TokenKind::Star
                | TokenKind::Amp
                | TokenKind::Comma
                | TokenKind::LitInt
                | TokenKind::LitFloat
                | TokenKind::DotDotDot => {}
                // Nested `<`: bump depth.
                TokenKind::Lt => {
                    depth += 1;
                }
                // `>`: pop one level; if we've returned to zero it's the match.
                TokenKind::Gt => {
                    if depth == 0 {
                        return false;
                    }
                    depth -= 1;
                    if depth == 0 {
                        return true;
                    }
                }
                // `>>` closes two nesting levels (C++11 `vector<vector<int>>`).
                // When depth == 1 the second `>` belongs to the outer context,
                // but this `<` is still a valid template open.
                TokenKind::GtGt => {
                    if depth <= 2 {
                        return true;
                    }
                    depth -= 2;
                }
                // Anything else (operators, parens, braces, …) means this is
                // an expression context, not a template argument list.
                _ => return false,
            }
            i += 1;
        }
        false
    }

    // ── Spacing decision ──────────────────────────────────────────────────────

    /// Should a space be emitted before `next`, given the last emitted token `prev`?
    fn needs_space(&self, next: TokenKind) -> bool {
        // The token immediately following `operator` is the overloaded symbol —
        // never add space between `operator` and `=`, `+=`, `==`, `[]`, etc.
        // Exception: `operator new` and `operator delete` are keyword operators
        // that do need a space.
        if self.after_operator_kw && !matches!(next, TokenKind::KwNew | TokenKind::KwDelete) {
            return false;
        }

        let prev = match self.prev {
            Some(k) => k,
            None => return false,
        };

        // Inside a template argument list: spacing after `<` and before `>`
        // is controlled solely by space_inside_angle_brackets.
        if prev == TokenKind::Lt && self.template_depth > 0 {
            return self.config.spacing.space_inside_angle_brackets;
        }

        // Never space before these closers / punctuation
        if matches!(
            next,
            TokenKind::Semi
                | TokenKind::Comma
                | TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::RBrace
                | TokenKind::DotDotDot
                | TokenKind::PlusPlus
                | TokenKind::MinusMinus
        ) {
            // RBrace handled separately; RParen/RBracket respect space_inside_* config
            if next == TokenKind::RParen {
                return match self.config.spacing.space_inside_parens {
                    SpaceOption::Add => true,
                    SpaceOption::Remove => false,
                    SpaceOption::Preserve => self.src_had_inline_ws,
                };
            }
            if next == TokenKind::RBracket {
                return match self.config.spacing.space_inside_brackets {
                    SpaceOption::Add => true,
                    SpaceOption::Remove => false,
                    SpaceOption::Preserve => self.src_had_inline_ws,
                };
            }
            if next == TokenKind::RBrace {
                return false; // newline handled by the RBrace arm
            }
            // After `;` in a for-loop header (e.g. `; ++i`), space is required.
            if matches!(next, TokenKind::PlusPlus | TokenKind::MinusMinus)
                && prev == TokenKind::Semi
            {
                return true;
            }
            // `foo(int x, ...)` — space after comma applies before `...`.
            if next == TokenKind::DotDotDot && prev == TokenKind::Comma {
                return self.config.spacing.space_after_comma;
            }
            return false;
        }

        // Never space after these openers
        if matches!(
            prev,
            TokenKind::LParen | TokenKind::LBracket | TokenKind::Tilde | TokenKind::Bang
        ) {
            if prev == TokenKind::LParen {
                return match self.config.spacing.space_inside_parens {
                    SpaceOption::Add => true,
                    SpaceOption::Remove => false,
                    SpaceOption::Preserve => self.src_had_inline_ws,
                };
            }
            if prev == TokenKind::LBracket {
                return match self.config.spacing.space_inside_brackets {
                    SpaceOption::Add => true,
                    SpaceOption::Remove => false,
                    SpaceOption::Preserve => self.src_had_inline_ws,
                };
            }
            return false;
        }

        // No space around member access / scope
        if matches!(
            prev,
            TokenKind::Dot
                | TokenKind::Arrow
                | TokenKind::DotStar
                | TokenKind::ArrowStar
                | TokenKind::ColonColon
        ) {
            return false;
        }
        if matches!(
            next,
            TokenKind::Dot
                | TokenKind::Arrow
                | TokenKind::DotStar
                | TokenKind::ArrowStar
                | TokenKind::ColonColon
        ) {
            return false;
        }

        // No space between unary prefix op and its operand (e.g. `++i`).
        // But if next is a binary op (e.g. `*p++ = x`), fall through to
        // the binary-op spacing rules below.
        if matches!(prev, TokenKind::PlusPlus | TokenKind::MinusMinus) && !next.is_binary_op() {
            return false;
        }

        // Space before `(` depends on context
        if next == TokenKind::LParen {
            // After an operator overload symbol (`operator=`, `operator+=`, `operator[]`,
            // `operator new`, etc.) the `(` opens the parameter list — treat it as a
            // call paren. Check before is_control_kw so `operator new(` doesn't get
            // keyword-paren spacing.
            if self.last_was_operator_overload {
                return self.config.spacing.space_before_call_paren;
            }
            if prev.is_control_kw() {
                return self.config.spacing.space_before_keyword_paren;
            }
            // Template close `>` behaves like an identifier: `vector<int>()`
            // uses call-paren spacing, not the default "always space" path.
            if matches!(prev, TokenKind::Ident | TokenKind::Keyword) || self.last_was_template_close
            {
                return self.config.spacing.space_before_call_paren;
            }
            if matches!(prev, TokenKind::RParen) {
                // Cast: honour space_after_cast; function-pointer call: no space.
                if self.last_was_cast_close {
                    return match self.config.spacing.space_after_cast {
                        SpaceOption::Add => true,
                        SpaceOption::Remove => false,
                        SpaceOption::Preserve => self.src_had_inline_ws,
                    };
                }
                return false;
            }
            return true;
        }

        // Space inside `[`
        if next == TokenKind::LBracket {
            return false;
        }

        // Unary operators: if the previous token cannot end an expression,
        // the next `+`, `-`, `*`, `&`, `!`, `~` is unary → no space before operand.
        // We handle the "before" part: no space inserted when op is unary.
        // (The op itself was already emitted with whatever spacing it got.)

        // After `;` inside a for-loop header, always emit a space regardless of
        // what follows (unary *, &, +, -, !, ~, ++, --).
        if prev == TokenKind::Semi && self.paren_depth > 0 {
            return true;
        }

        // After comma
        if prev == TokenKind::Comma {
            return self.config.spacing.space_after_comma;
        }

        // After a cast-close `)`, `*` and `&` are always unary (dereference /
        // address-of). Check this before the binary-op path which would otherwise
        // see RParen.ends_expr() == true and add a space.
        if self.last_was_cast_close
            && prev == TokenKind::RParen
            && matches!(next, TokenKind::Star | TokenKind::Amp)
        {
            return match self.config.spacing.space_after_cast {
                SpaceOption::Add => true,
                SpaceOption::Remove => false,
                SpaceOption::Preserve => self.src_had_inline_ws,
            };
        }

        // Binary operators — space on both sides if configured
        if next.is_binary_op() {
            if prev.ends_expr() {
                return self.config.spacing.space_around_binary_ops;
            }
            // next is in unary context, but prev (a binary op or keyword like
            // `return`/`throw`) still needs a trailing space: `= -1`, `return &x`.
            if prev.is_binary_op() || prev.is_any_kw() {
                return self.config.spacing.space_around_binary_ops;
            }
            // purely unary context (e.g. after `(`) — no space
            return false;
        }
        if prev.is_binary_op() {
            return self.config.spacing.space_around_binary_ops;
        }

        // Colon: ternary gets space on both sides; case/label/base-class does not.
        // Bitfield colons are handled directly in the Colon arm, not here.
        if next == TokenKind::Colon {
            return self.ternary_depth > 0;
        }
        if prev == TokenKind::Colon {
            return true;
        }

        // After keywords that aren't followed by `(`
        if prev.is_any_kw() {
            return true;
        }
        // Before a keyword
        if next.is_any_kw() {
            return true;
        }

        // After a cast-closing `)`, honour space_after_cast config.
        // The `next == LParen && prev == RParen` case already returned false above,
        // so this only fires when the next token is not `(`.
        if prev == TokenKind::RParen && self.last_was_cast_close {
            return match self.config.spacing.space_after_cast {
                SpaceOption::Add => true,
                SpaceOption::Remove => false,
                SpaceOption::Preserve => self.src_had_inline_ws,
            };
        }

        // Default: space between two identifier-like tokens
        true
    }

    // ── Main format loop ──────────────────────────────────────────────────────

    fn format(mut self) -> Result<String, FunkyError> {
        loop {
            self.skip_ws();

            let tok = match self.advance() {
                None => break,
                Some(t) => t.clone(),
            };

            self.check_var_decl_transition(tok.kind);

            match tok.kind {
                TokenKind::Eof => {
                    if self.config.newlines.final_newline && !self.at_line_start {
                        self.nl();
                    }
                    break;
                }

                // ── Preprocessor — pass through verbatim, normalized newlines ─
                TokenKind::PreprocLine => {
                    self.flush_blank_lines();
                    if !self.at_line_start {
                        self.nl();
                    }
                    // Normalize line endings in the directive.
                    let normalized = tok.lexeme.replace("\r\n", "\n").replace('\r', "\n");
                    let nl = self.config.newline_str();
                    let normalized = normalized.replace('\n', nl);

                    // Classify the directive (always, not just when pp_indent is on).
                    let trimmed = normalized.trim_start();
                    let directive = trimmed
                        .strip_prefix('#')
                        .map(|s| s.split_whitespace().next().unwrap_or(""))
                        .unwrap_or("");
                    let is_open = matches!(directive, "if" | "ifdef" | "ifndef");
                    let is_close = directive == "endif";
                    let is_reopen = matches!(directive, "elif" | "else");

                    // Track code brace depth across #ifdef/#else/#endif so both
                    // branches start indenting from the same level.
                    if is_open {
                        self.pp_brace_stack.push(self.indent_level);
                    } else if is_reopen {
                        // Restore to the depth recorded at the opening #if so the
                        // else-branch starts with the same baseline.
                        if let Some(&saved) = self.pp_brace_stack.last() {
                            self.indent_level = saved;
                        }
                    } else if is_close {
                        // Pop without restoring — the last branch's accumulated
                        // depth is the correct post-#endif depth.
                        self.pp_brace_stack.pop();
                    }

                    // Normalize the number of spaces between `#endif` and a
                    // trailing `/*` comment.
                    let normalized = if is_close {
                        normalize_endif_spacing(
                            &normalized,
                            self.config.preprocessor.endif_comment_space,
                        )
                    } else {
                        normalized
                    };

                    if self.config.preprocessor.pp_indent {
                        // #endif and #elif/#else dedent before emit.
                        if is_close || is_reopen {
                            self.pp_depth = self.pp_depth.saturating_sub(1);
                        }
                        let indent_str = self.config.indent_str().repeat(self.pp_depth as usize);
                        // Write depth-prefix before the `#`.
                        self.write(&indent_str);
                        self.write(normalized.trim_start());
                        // #if and #elif/#else increase depth after emit.
                        if is_open || is_reopen {
                            self.pp_depth += 1;
                        }
                    } else {
                        self.write(&normalized);
                    }

                    if !self.at_line_start {
                        self.nl();
                    }
                    self.set_prev(TokenKind::PreprocLine);
                }

                // ── Line comment ──────────────────────────────────────────────
                TokenKind::CommentLine => {
                    // Merge: a standalone comment with no blank lines before it
                    // can be hoisted to the end of the preceding brace/statement
                    // line when the config flag is set.
                    let can_merge = self.config.newlines.merge_line_comment
                        && self.at_line_start
                        && self.blank_lines == 0
                        && matches!(
                            self.prev,
                            Some(TokenKind::LBrace | TokenKind::RBrace | TokenKind::Semi)
                        );
                    self.flush_blank_lines();
                    if can_merge {
                        self.trim_to_prev_line_end();
                    }
                    if !self.at_line_start {
                        self.space();
                    } else {
                        self.indent();
                    }
                    // Emit comment with normalized line ending at the end.
                    let body = tok.lexeme.trim_end_matches(['\n', '\r']);
                    self.write(body);
                    self.nl();
                    self.set_prev(TokenKind::CommentLine);
                }

                // ── Block comment ─────────────────────────────────────────────
                TokenKind::CommentBlock => {
                    self.flush_blank_lines();
                    if !self.at_line_start {
                        self.space();
                    } else {
                        self.indent();
                    }
                    // Normalize newlines in the block comment body.
                    let nl = self.config.newline_str();
                    let normalized = tok
                        .lexeme
                        .replace("\r\n", "\n")
                        .replace('\r', "\n")
                        .replace('\n', nl);
                    // Ensure the closing `*/` has a leading space when it sits
                    // flush at the start of a line (e.g. `\n*/` → `\n */`).
                    // When the config option is enabled, rewrite a bare `*/`
                    // closing line to ` */` to match ` *`-continuation style.
                    // Exception: SQLite-style `**`-continuation comments already
                    // have `*/` at column 0 — don't add a spurious space there.
                    let uses_double_star = normalized
                        .split(nl)
                        .skip(1)
                        .any(|line| line.starts_with("**"));
                    let normalized = if self.config.comments.normalize_block_comment_closing
                        && !uses_double_star
                        && normalized.contains(&format!("{nl}*/"))
                    {
                        normalized.replace(&format!("{nl}*/"), &format!("{nl} */"))
                    } else {
                        normalized
                    };
                    // When indent style is spaces, expand leading tabs in
                    // continuation lines to spaces.  The leading whitespace
                    // before the `*` on each continuation line is indentation,
                    // not comment content, and should follow the indent style.
                    let normalized = if self.config.indent.style == IndentStyle::Spaces {
                        let tab_w = self.config.indent.width as usize;
                        let mut out = String::with_capacity(normalized.len());
                        let mut first = true;
                        for line in normalized.split(nl) {
                            if !first {
                                out.push_str(nl);
                            }
                            first = false;
                            // Expand each leading tab to tab_w spaces.
                            let non_tab = line.trim_start_matches('\t');
                            let n_tabs = line.len() - non_tab.len();
                            for _ in 0..n_tabs * tab_w {
                                out.push(' ');
                            }
                            out.push_str(non_tab);
                        }
                        out
                    } else {
                        normalized
                    };
                    // Strip trailing spaces from each line inside the comment.
                    let normalized = if normalized.contains(' ') || normalized.contains('\t') {
                        let mut out = String::with_capacity(normalized.len());
                        let mut first = true;
                        for line in normalized.split(nl) {
                            if !first {
                                out.push_str(nl);
                            }
                            first = false;
                            out.push_str(line.trim_end());
                        }
                        out
                    } else {
                        normalized
                    };
                    self.write(&normalized);
                    // If the comment ends the line (next source token is a
                    // Newline), emit the newline now and mark it consumed so
                    // skip_ws() doesn't double-count it as a blank line.
                    // CommentLine does this implicitly because its lexeme
                    // includes the trailing \n; CommentBlock does not.
                    if matches!(self.tokens.get(self.pos), Some(t) if t.kind == TokenKind::Newline) {
                        self.nl();
                        self.skip_next_newline = true;
                    }
                    self.set_prev(TokenKind::CommentBlock);
                }

                // ── Opening brace ─────────────────────────────────────────────
                TokenKind::LBrace => {
                    let ctx = self.infer_brace_ctx();
                    self.flush_blank_lines();

                    match ctx {
                        BraceCtx::Other => {
                            // Initializer list — stay on same line with a space
                            if self.needs_space(TokenKind::LBrace) {
                                self.space();
                            }
                            self.write("{");

                            // Small initializer: keep entirely on one line.
                            if let Some(end) = self.small_initializer_end() {
                                let content: Vec<(&str, TokenKind)> = self.tokens[self.pos..end]
                                    .iter()
                                    .filter(|t| {
                                        !matches!(
                                            t.kind,
                                            TokenKind::Whitespace | TokenKind::Newline
                                        )
                                    })
                                    .map(|t| (t.lexeme, t.kind))
                                    .collect();

                                if content.is_empty() {
                                    self.write("}");
                                } else {
                                    self.write(" ");
                                    let mut prev_kind = TokenKind::LBrace;
                                    let mut suppress = false;
                                    for (lex, kind) in content.iter() {
                                        // No space before . or -> only when it's member
                                        // access (prev ends an expression). Designated
                                        // initializer .field comes after , or { and
                                        // does need a space.
                                        let need_space = !(suppress
                                            || matches!(kind, TokenKind::Comma)
                                            || matches!(
                                                prev_kind,
                                                TokenKind::LBrace
                                                    | TokenKind::Dot
                                                    | TokenKind::Arrow
                                                    | TokenKind::LParen
                                            )
                                            || matches!(kind, TokenKind::Dot | TokenKind::Arrow)
                                                && prev_kind.ends_expr());
                                        if need_space {
                                            self.write(" ");
                                        }
                                        suppress = false;
                                        self.write(lex);
                                        // Unary context: after comma, LBrace, or LParen,
                                        // the next - + * & is unary — suppress space after it.
                                        if matches!(
                                            kind,
                                            TokenKind::Minus
                                                | TokenKind::Plus
                                                | TokenKind::Star
                                                | TokenKind::Amp
                                        ) && matches!(
                                            prev_kind,
                                            TokenKind::Comma
                                                | TokenKind::LBrace
                                                | TokenKind::LParen
                                        ) {
                                            suppress = true;
                                        }
                                        prev_kind = *kind;
                                    }
                                    self.write(" }");
                                }
                                self.pos = end + 1;
                                self.set_prev(TokenKind::RBrace);
                                continue;
                            }
                        }
                        // extern "C" { } is a linkage specification. Placement is
                        // controlled by braces.extern_c_brace:
                        //   force_same_line — always K&R (Google/LLVM style)
                        //   preserve        — leave brace where source has it
                        BraceCtx::ExternC => {
                            match self.config.braces.extern_c_brace {
                                ExternCBrace::ForceSameLine => {
                                    if self.at_line_start {
                                        self.trim_to_prev_line_end();
                                    }
                                    self.space();
                                }
                                ExternCBrace::Preserve => {
                                    if self.at_line_start {
                                        // brace was already on its own line in source — keep it
                                    } else {
                                        self.space();
                                    }
                                }
                            }
                            self.write("{");
                        }
                        _ => match self.config.braces.style {
                            BraceStyle::Allman => {
                                self.ensure_own_line();
                                self.write("{");
                            }
                            BraceStyle::Kr | BraceStyle::Stroustrup => {
                                // fn_brace_newline: function-definition braces go on
                                // their own line even in KR mode.  Control-flow
                                // constructs (if/for/while/switch) always stay on the
                                // same line.
                                let fn_newline = ctx == BraceCtx::Function
                                    && self.config.braces.fn_brace_newline
                                    && !self.rparen_closes_ctrl_flow();
                                if fn_newline {
                                    self.ensure_own_line();
                                } else {
                                    if self.at_line_start {
                                        // Source had Allman-style brace; enforce KR.
                                        self.trim_to_prev_line_end();
                                    }
                                    self.space();
                                }
                                self.write("{");
                            }
                        },
                    }

                    // ── Empty-body collapse ───────────────────────────────────
                    // When collapse_empty_body is set and the only content between
                    // `{` and `}` is whitespace, emit `{}` on the same line.
                    if self.config.braces.collapse_empty_body {
                        let mut look = self.pos;
                        while look < self.tokens.len()
                            && matches!(
                                self.tokens[look].kind,
                                TokenKind::Whitespace | TokenKind::Newline
                            )
                        {
                            look += 1;
                        }
                        if self.tokens.get(look).map(|t| t.kind) == Some(TokenKind::RBrace) {
                            // Consume whitespace + the `}` token.
                            self.pos = look + 1;
                            self.write("}");

                            // Replicate the post-`}` newline/spacing decisions from
                            // the RBrace arm so callers see the same output shape.
                            let mut after = self.pos;
                            while after < self.tokens.len()
                                && matches!(
                                    self.tokens[after].kind,
                                    TokenKind::Whitespace | TokenKind::Newline
                                )
                            {
                                after += 1;
                            }
                            let next_kind = self.tokens.get(after).map(|t| t.kind);

                            self.emit_post_brace_spacing(ctx, next_kind, tok.span.line);

                            self.pending_switch = false;
                            self.pending_type = false;
                            self.pending_extern_c = false;
                            self.set_prev(TokenKind::RBrace);
                            continue;
                        }
                    }

                    if ctx == BraceCtx::Switch {
                        self.switch_depth += 1;
                        self.case_body_stack.push(false);
                    }
                    if ctx == BraceCtx::Type {
                        self.class_depth += 1;
                    }
                    // A Block at global scope (brace_stack currently empty) is a
                    // macro-defined function body (e.g. SM_STATE(...) { ... }).
                    // Treat it like a Function for the var-decl blank-line rule.
                    let is_func_like = ctx == BraceCtx::Function
                        || (ctx == BraceCtx::Block && self.brace_stack.is_empty());
                    if is_func_like && self.config.newlines.blank_line_after_var_decl_block {
                        self.in_var_decl_block = true;
                        self.at_func_stmt_start = true;
                        self.saw_func_decl = false;
                    }
                    self.pending_switch = false;
                    self.pending_type = false;
                    self.pending_extern_c = false;
                    // `{` takes over indentation; = alignment no longer applies.
                    self.assign_col = None;
                    let is_large_init = ctx == BraceCtx::Other
                        && self.config.braces.expand_large_initializers
                        && self.large_flat_initializer_end().is_some();
                    self.brace_stack.push(ctx);
                    self.large_init_stack.push(is_large_init);
                    // `case X: {` — the case-label colon already incremented
                    // indent_level (via indent_switch_case). The block `{` should
                    // take over that indent slot rather than adding a new one, so the
                    // body sits at case_body_level + 1 (not + 2).
                    if ctx == BraceCtx::Block
                        && self.last_was_case_colon
                        && self.config.indent.indent_switch_case
                    {
                        // Undo the case-body indent; the block's own +1 below
                        // restores the same net level.
                        self.indent_level = self.indent_level.saturating_sub(1);
                        if let Some(active) = self.case_body_stack.last_mut() {
                            *active = false;
                        }
                    }
                    if ctx != BraceCtx::ExternC {
                        self.indent_level += 1;
                    }
                    self.nl();
                    self.skip_next_newline = true;
                    if self.config.newlines.blank_line_after_open_brace
                        && matches!(ctx, BraceCtx::Function | BraceCtx::Block)
                    {
                        self.blank_lines = self.blank_lines.max(1);
                    }
                    self.set_prev(TokenKind::LBrace);
                }

                // ── Closing brace ─────────────────────────────────────────────
                TokenKind::RBrace => {
                    let closing_ctx = self.brace_stack.last().copied().unwrap_or(BraceCtx::Other);
                    // When indent_switch_case is on, an active case body adds an
                    // extra indent level that must be unwound before the `}`.
                    if closing_ctx == BraceCtx::Switch && self.config.indent.indent_switch_case {
                        if let Some(true) = self.case_body_stack.last() {
                            self.indent_level = self.indent_level.saturating_sub(1);
                        }
                        self.case_body_stack.pop();
                    }
                    if closing_ctx != BraceCtx::ExternC && self.indent_level > 0 {
                        self.indent_level -= 1;
                    }
                    // An `=` inside the brace body (e.g. the last enum value without a
                    // trailing comma) leaves assign_col set. Clear it so `indent()` uses
                    // normal indentation rather than aligning to the `=` column.
                    self.assign_col = None;
                    self.flush_blank_lines();
                    self.ensure_own_line();
                    self.write("}");

                    let ctx = self.brace_stack.pop().unwrap_or(BraceCtx::Other);
                    self.large_init_stack.pop();

                    if ctx == BraceCtx::Switch {
                        self.switch_depth = self.switch_depth.saturating_sub(1);
                    }
                    if ctx == BraceCtx::Type {
                        self.class_depth = self.class_depth.saturating_sub(1);
                    }
                    // brace_stack was already popped; a Block that left the stack
                    // empty was a top-level macro-function body (see LBrace logic).
                    let was_func_like = ctx == BraceCtx::Function
                        || (ctx == BraceCtx::Block && self.brace_stack.is_empty());
                    if was_func_like {
                        self.in_var_decl_block = false;
                        self.at_func_stmt_start = false;
                        self.force_blank_after_decls = false;
                    }

                    // Semicolon required after type definitions and namespace
                    let needs_semi = matches!(ctx, BraceCtx::Type);

                    // Peek: is the next token `;`?
                    let mut look = self.pos;
                    while look < self.tokens.len()
                        && matches!(
                            self.tokens[look].kind,
                            TokenKind::Whitespace | TokenKind::Newline
                        )
                    {
                        look += 1;
                    }
                    let next_kind = self.tokens.get(look).map(|t| t.kind);

                    if needs_semi && next_kind != Some(TokenKind::Semi) {
                        // The struct/class/enum definition has no trailing `;` —
                        // we must not add one ourselves (the source might be a forward
                        // decl without one, which is fine). Just emit the brace.
                    }

                    self.emit_post_brace_spacing(ctx, next_kind, tok.span.line);

                    self.set_prev(TokenKind::RBrace);
                }

                // ── Semicolon ─────────────────────────────────────────────────
                TokenKind::Semi => {
                    self.flush_blank_lines();
                    self.pending_type = false;
                    self.pending_extern_c = false;
                    self.write(";");
                    // Don't emit newline if we're inside parens (for-loop header).
                    if self.paren_depth == 0 {
                        self.assign_col = None;
                        // If a trailing inline comment follows on the same source
                        // line, let the CommentLine handler close the line instead.
                        if self.peek_inline_comment(tok.span.line) {
                            // nothing — CommentLine will emit the trailing \n
                        } else {
                            self.nl();
                            self.skip_next_newline = true;
                        }
                        // Signal that the next token starts a new statement so the
                        // var-decl-block state machine can evaluate it.
                        if self.in_var_decl_block {
                            let top = self.brace_stack.last();
                            let is_func_top = top == Some(&BraceCtx::Function)
                                || (top == Some(&BraceCtx::Block) && self.brace_stack.len() == 1);
                            if is_func_top {
                                self.at_func_stmt_start = true;
                            }
                        }
                    }
                    self.set_prev(TokenKind::Semi);
                }

                // ── Paren depth tracking ──────────────────────────────────────
                TokenKind::LParen => {
                    self.flush_blank_lines();
                    let is_cast = self.next_is_type_kw()
                        && !self.prev_is_sizeof_like()
                        && !self.prev.is_some_and(|p| p.is_control_kw());
                    self.cast_paren_stack.push(is_cast);
                    if self.at_line_start {
                        self.indent();
                    } else if self.needs_space(TokenKind::LParen) {
                        self.space();
                    } else if self.next_is_fn_ptr_declarator()
                        && matches!(
                            self.prev,
                            Some(TokenKind::Keyword | TokenKind::Ident | TokenKind::RParen)
                        )
                    {
                        // function-pointer declarator: `void (*Fn)(...)` needs space before `(`
                        self.space();
                    }
                    self.write("(");
                    self.paren_depth += 1;
                    // When space_inside_parens is Add (or Preserve with source space),
                    // the first argument starts one column later; include that offset
                    // so continuation lines align with it rather than the bare `(`.
                    let extra = match self.config.spacing.space_inside_parens {
                        SpaceOption::Add => 1,
                        SpaceOption::Remove => 0,
                        // Peek at the raw next token to see if source has a space.
                        SpaceOption::Preserve => usize::from(matches!(
                            self.tokens.get(self.pos),
                            Some(t) if t.kind == TokenKind::Whitespace
                        )),
                    };
                    self.paren_col_stack.push(self.current_col + extra);
                    // Precompute the EOL continuation column: base indent of
                    // this line plus one indent width per paren opened on it.
                    let indent_width = self.config.indent.width as usize;
                    let parens_on_line =
                        (self.paren_depth - self.line_start_paren_depth) as usize;
                    let eol_col = self.line_indent_col + parens_on_line * indent_width;
                    self.paren_eol_stack.push((false, eol_col));
                    self.set_prev(TokenKind::LParen);
                }
                TokenKind::RParen => {
                    self.flush_blank_lines();
                    if self.at_line_start {
                        self.align_to_paren();
                    } else {
                        let want = match self.config.spacing.space_inside_parens {
                            SpaceOption::Add => true,
                            SpaceOption::Remove => false,
                            SpaceOption::Preserve => self.src_had_inline_ws,
                        };
                        if want {
                            self.space();
                        }
                    }
                    self.write(")");
                    self.paren_depth = self.paren_depth.saturating_sub(1);
                    self.paren_col_stack.pop();
                    self.paren_eol_stack.pop();
                    let is_cast_close = self.cast_paren_stack.pop().unwrap_or(false);
                    // A pointer declarator `*`/`&` immediately before `)` (e.g.
                    // `sizeof(char *)`) sets suppress_next_space, but that flag
                    // must not leak out of the closing paren into subsequent tokens.
                    self.suppress_next_space = false;
                    self.prev = Some(TokenKind::RParen);
                    self.last_was_template_close = false;
                    self.last_was_cast_close = is_cast_close;
                }

                // ── Bracket depth tracking ────────────────────────────────────
                TokenKind::LBracket => {
                    self.flush_blank_lines();
                    self.write("[");
                    self.bracket_depth += 1;
                    self.set_prev(TokenKind::LBracket);
                }
                TokenKind::RBracket => {
                    self.flush_blank_lines();
                    if !self.at_line_start {
                        let want = match self.config.spacing.space_inside_brackets {
                            SpaceOption::Add => true,
                            SpaceOption::Remove => false,
                            SpaceOption::Preserve => self.src_had_inline_ws,
                        };
                        if want {
                            self.space();
                        }
                    }
                    self.write("]");
                    self.bracket_depth = self.bracket_depth.saturating_sub(1);
                    self.set_prev(TokenKind::RBracket);
                }

                // ── Colon after case / default / access specifier / ternary ──
                TokenKind::Colon => {
                    self.flush_blank_lines();
                    // Ternary `:` gets a space before it; case/label/access/goto do not.
                    if self.ternary_depth > 0
                        && !self.in_case_label
                        && !self.in_access_label
                        && !self.in_goto_label
                    {
                        self.ternary_depth = self.ternary_depth.saturating_sub(1);
                        if !self.at_line_start {
                            self.space();
                        }
                    } else if !self.in_case_label
                        && !self.in_access_label
                        && !self.in_goto_label
                        && self.prev == Some(TokenKind::Ident)
                        && self.peek_non_ws_kind() == Some(TokenKind::LitInt)
                        && self
                            .brace_stack
                            .last()
                            .is_some_and(|ctx| *ctx == BraceCtx::Type)
                    {
                        // Bitfield colon: `field:N` → `field : N`
                        if !self.at_line_start {
                            self.space();
                        }
                    }
                    self.write(":");
                    let is_case_colon = self.in_case_label;
                    if self.in_case_label {
                        self.in_case_label = false;
                        self.nl();
                        self.skip_next_newline = true;
                        if self.config.indent.indent_switch_case {
                            self.indent_level += 1;
                            if let Some(active) = self.case_body_stack.last_mut() {
                                *active = true;
                            }
                        }
                    } else if self.in_access_label {
                        self.in_access_label = false;
                        self.nl();
                        self.skip_next_newline = true;
                    } else if self.in_goto_label {
                        self.in_goto_label = false;
                        self.nl();
                        self.skip_next_newline = true;
                    }
                    self.set_prev(TokenKind::Colon);
                    // Set AFTER set_prev() so set_prev() doesn't clear it.
                    self.last_was_case_colon = is_case_colon;
                }

                // ── switch keyword — arm to set pending_switch ────────────────
                TokenKind::KwSwitch => {
                    self.flush_blank_lines();
                    if self.at_line_start {
                        self.indent();
                    } else if self.needs_space(tok.kind) {
                        self.space();
                    }
                    self.pending_switch = true;
                    self.write(tok.lexeme);
                    self.set_prev(tok.kind);
                }

                // ── case / default labels ─────────────────────────────────────
                TokenKind::KwCase | TokenKind::KwDefault => {
                    self.flush_blank_lines();
                    if self.at_line_start {
                        if self.config.indent.indent_switch_case {
                            // Undo any prior case-body extra indent, then print
                            // at the switch-body level (no additional dedent).
                            if let Some(active) = self.case_body_stack.last_mut() {
                                if *active {
                                    self.indent_level = self.indent_level.saturating_sub(1);
                                    *active = false;
                                }
                            }
                            self.indent();
                        } else {
                            // Dedent one level relative to the switch body.
                            let saved = self.indent_level;
                            if self.switch_depth > 0 && self.indent_level > 0 {
                                self.indent_level -= 1;
                            }
                            self.indent();
                            self.indent_level = saved;
                        }
                    } else if self.needs_space(tok.kind) {
                        self.space();
                    }
                    self.in_case_label = true;
                    self.write(tok.lexeme);
                    self.set_prev(tok.kind);
                }

                // ── Access specifiers — dedented to class body level ──────────
                TokenKind::KwPublic | TokenKind::KwPrivate | TokenKind::KwProtected => {
                    self.flush_blank_lines();
                    if self.at_line_start && self.class_depth > 0 {
                        let saved = self.indent_level;
                        if self.indent_level > 0 {
                            self.indent_level -= 1;
                        }
                        self.indent();
                        self.indent_level = saved;
                        self.in_access_label = true;
                    } else if !self.at_line_start && self.needs_space(tok.kind) {
                        self.space();
                    }
                    self.write(tok.lexeme);
                    self.set_prev(tok.kind);
                }

                // ── Template angle brackets ───────────────────────────────────
                TokenKind::Lt
                    if matches!(
                        self.prev,
                        Some(TokenKind::Ident | TokenKind::KwTemplate | TokenKind::Gt)
                    ) && self.looks_like_template_open() =>
                {
                    self.flush_blank_lines();
                    // No space between the name and `<`: `vector<int>` not `vector <int>`.
                    if self.at_line_start {
                        self.indent();
                    }
                    self.write("<");
                    self.template_depth += 1;
                    if self.config.spacing.space_inside_angle_brackets {
                        self.space();
                    }
                    self.set_prev(TokenKind::Lt);
                }

                TokenKind::Gt if self.template_depth > 0 => {
                    self.flush_blank_lines();
                    if self.config.spacing.space_inside_angle_brackets && !self.at_line_start {
                        self.space();
                    } else if self.at_line_start {
                        self.indent();
                    }
                    self.write(">");
                    self.template_depth -= 1;
                    self.prev = Some(TokenKind::Gt);
                    self.last_was_template_close = true;
                }

                // `>>` closing two nested template levels: `vector<vector<int>>`
                TokenKind::GtGt if self.template_depth >= 2 => {
                    self.flush_blank_lines();
                    if self.config.spacing.space_inside_angle_brackets && !self.at_line_start {
                        self.space();
                    } else if self.at_line_start {
                        self.indent();
                    }
                    self.write(">>");
                    self.template_depth -= 2;
                    self.prev = Some(TokenKind::Gt);
                    self.last_was_template_close = true;
                }

                // ── Pointer / reference declarator ───────────────────────────
                TokenKind::Star | TokenKind::Amp if self.is_ptr_decl_context() => {
                    self.flush_blank_lines();
                    match self.config.spacing.pointer_align {
                        PointerAlign::Middle => {
                            // Same as binary-op: space on both sides.
                            if self.at_line_start {
                                self.indent();
                            } else if self.needs_space(tok.kind) {
                                self.space();
                            }
                        }
                        PointerAlign::Type => {
                            // Star/amp attached to the type — no space before.
                            if self.at_line_start {
                                self.indent();
                            }
                            // Deliberately no space() call here.
                        }
                        PointerAlign::Name => {
                            // Star/amp attached to the name — space before (only
                            // between type and first star; consecutive stars/amps
                            // stay together), suppress space after.
                            if self.at_line_start {
                                self.indent();
                            } else if !matches!(self.prev, Some(TokenKind::Star | TokenKind::Amp)) {
                                self.space();
                            }
                            self.suppress_next_space = true;
                        }
                    }
                    self.write(tok.lexeme);
                    self.set_prev(tok.kind);
                }

                // ── Unary / binary * & + - (non-declarator) ─────────────────
                // In unary context, suppress the space after the op so `*ptr`,
                // `&x`, `-1`, `+x` stay compact.
                TokenKind::Star | TokenKind::Amp | TokenKind::Plus | TokenKind::Minus => {
                    self.flush_blank_lines();
                    // At line start (e.g. `*ptr = ...` after a standalone block
                    // comment) the operator is always unary, never binary — even
                    // if the previous emitted token was a CommentBlock which
                    // satisfies ends_expr().
                    // After a cast-close `)`, * and & are always unary (dereference /
                    // address-of), never binary multiplication / bitwise-and.
                    let is_binary = !self.at_line_start
                        && !self.last_was_cast_close
                        && self.prev.is_some_and(|p| p.ends_expr());
                    if self.at_line_start {
                        self.indent();
                    } else if self.needs_space(tok.kind) {
                        self.space();
                    }
                    if !is_binary {
                        self.suppress_next_space = true;
                    }
                    self.write(tok.lexeme);
                    self.set_prev(tok.kind);
                }

                // ── Type keywords — mark pending_type for brace context ──────
                TokenKind::KwClass
                | TokenKind::KwStruct
                | TokenKind::KwUnion
                | TokenKind::KwEnum => {
                    self.flush_blank_lines();
                    if self.at_line_start {
                        self.indent();
                    } else if self.needs_space(tok.kind) {
                        self.space();
                    }
                    self.pending_type = true;
                    self.write(tok.lexeme);
                    self.set_prev(tok.kind);
                }

                // ── Comma — newline after each element in large initializers ──
                TokenKind::Comma => {
                    self.flush_blank_lines();
                    if self.at_line_start {
                        self.indent();
                    }
                    self.write(",");
                    // A comma at statement level ends the current assignment expression.
                    if self.paren_depth == 0 && self.bracket_depth == 0 {
                        self.assign_col = None;
                    }
                    if self.large_init_stack.last() == Some(&true) && self.paren_depth == 0 {
                        // If a trailing line comment follows on the same source line,
                        // let the CommentLine handler close the line instead.
                        if self.peek_inline_line_comment(tok.span.line) {
                            // nothing — CommentLine will emit the trailing \n
                        } else {
                            self.nl();
                            self.skip_next_newline = true;
                        }
                    }
                    self.set_prev(TokenKind::Comma);
                }

                // ── Assignment operators — track RHS column for continuation ──
                TokenKind::Eq
                | TokenKind::PlusEq
                | TokenKind::MinusEq
                | TokenKind::StarEq
                | TokenKind::SlashEq
                | TokenKind::PercentEq
                | TokenKind::AmpEq
                | TokenKind::PipeEq
                | TokenKind::CaretEq
                | TokenKind::LtLtEq
                | TokenKind::GtGtEq => {
                    self.flush_blank_lines();
                    if self.at_line_start {
                        self.indent();
                    } else if self.needs_space(tok.kind) {
                        self.space();
                    }
                    self.write(tok.lexeme);
                    if self.paren_depth == 0 && self.bracket_depth == 0 && self.assign_col.is_none()
                    {
                        let space = usize::from(self.config.spacing.space_around_binary_ops);
                        self.assign_col = Some(self.current_col + space);
                        self.assign_eol = false;
                    }
                    self.set_prev(tok.kind);
                }

                // ── Goto labels: `identifier:` at statement level ─────────────
                TokenKind::Ident
                    if !self.config.indent.indent_goto_labels
                        && self.at_line_start
                        && self.paren_depth == 0
                        && self.bracket_depth == 0
                        && self.template_depth == 0
                        && self.ternary_depth == 0
                        && !self.in_case_label
                        && !self.in_access_label
                        && self.peek_non_ws_kind() == Some(TokenKind::Colon) =>
                {
                    self.flush_blank_lines();
                    // Emit at column 0 — no indentation call.
                    self.write(tok.lexeme);
                    self.in_goto_label = true;
                    self.set_prev(tok.kind);
                }

                // ── Everything else ───────────────────────────────────────────
                _ => {
                    self.flush_blank_lines();

                    if self.at_line_start {
                        self.indent();
                    } else if self.needs_space(tok.kind) {
                        self.space();
                    }

                    // Track `extern "C"` sequence for ExternC brace context.
                    // Keep the flag alive across the LitStr (`"C"`); set it on `extern`;
                    // clear it on anything else that breaks the sequence.
                    if !(tok.kind == TokenKind::LitStr && self.pending_extern_c) {
                        self.pending_extern_c =
                            tok.kind == TokenKind::Keyword && tok.lexeme == "extern";
                    }

                    self.write(tok.lexeme);
                    self.set_prev(tok.kind);
                    // `operator` keyword — suppress spacing before the overloaded symbol.
                    if tok.kind == TokenKind::Keyword && tok.lexeme == "operator" {
                        self.after_operator_kw = true;
                    }
                    // Ternary `?` — track depth so the matching `:` gets spaces.
                    if tok.kind == TokenKind::Question {
                        self.ternary_depth += 1;
                    }
                }
            }
        }

        // Normalise any \r\n or \r remaining in the output to the configured style.
        let nl = self.config.newline_str();
        if nl != "\n" {
            let output = self.output.replace("\r\n", "\n").replace('\r', "\n");
            self.output = output.replace('\n', nl);
        }

        Ok(self.output)
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

pub fn format<'src>(tokens: &[Token<'src>], config: &Config) -> Result<String, FunkyError> {
    let injected;
    let tokens: &[Token<'src>] = if config.braces.add_braces_to_if
        || config.braces.add_braces_to_while
        || config.braces.add_braces_to_for
    {
        injected = inject_braces_pass(tokens, config);
        &injected
    } else {
        tokens
    };
    let output = Fmt::new(config, tokens).format()?;
    let nl = config.newline_str();
    let output = if config.spacing.align_right_cmt_span > 0 {
        let normalize_single = config.spacing.align_right_cmt_style == AlignCmtStyle::All;
        align_trailing_comments(
            &output,
            nl,
            config.spacing.align_right_cmt_gap.max(1),
            normalize_single,
            config.spacing.align_on_tabstop,
            config.indent.width as usize,
            config.spacing.align_right_cmt_span,
        )
    } else {
        output
    };
    let output = if config.spacing.align_enum_equ_span > 0 {
        align_enum_equals(
            &output,
            nl,
            config.spacing.align_on_tabstop,
            config.indent.width as usize,
        )
    } else {
        output
    };
    let output = if config.spacing.align_doxygen_cmt_span > 0 {
        align_doxygen_comments(&output, nl)
    } else {
        output
    };
    Ok(output)
}

/// Normalizes the whitespace between `#endif` and a trailing `/*` comment to
/// exactly `spaces` spaces. Lines with no `/*` are returned unchanged.
fn normalize_endif_spacing(line: &str, spaces: u32) -> String {
    if let Some(pos) = line.find("/*") {
        let before = line[..pos].trim_end();
        let gap = " ".repeat(spaces as usize);
        format!("{before}{gap}{}", &line[pos..])
    } else {
        line.to_string()
    }
}

/// Round `n` up to the next multiple of `step` (or `n` itself if already a multiple).
fn round_up_to_multiple(n: usize, step: usize) -> usize {
    if step == 0 {
        return n;
    }
    n.div_ceil(step) * step
}

/// Returns the byte index of the `//` or `/*` that starts a trailing inline
/// comment on `line`, or `None` if the line has no trailing comment (standalone
/// comment lines and blank lines also return `None`).
///
/// A `/* */` comment is only considered trailing when nothing non-whitespace
/// follows its closing `*/` — this prevents mid-expression block comments like
/// `2 /* two bytes */ +` from being treated as trailing comments.
fn trailing_comment_col(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("/*") {
        return None;
    }
    // Preprocessor lines (#endif, #else, #ifdef, etc.) have their own spacing
    // rules and must not be included in trailing-comment alignment groups.
    if trimmed.starts_with('#') {
        return None;
    }
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && (bytes[i + 1] == b'/' || bytes[i + 1] == b'*') {
            // Skip /**< — those are Doxygen member comments handled by their own pass.
            if bytes[i + 1] == b'*'
                && bytes.get(i + 2) == Some(&b'*')
                && bytes.get(i + 3) == Some(&b'<')
            {
                i += 1;
                continue;
            }
            // Skip `://` — part of a URL (e.g. `https://`), not a comment.
            if bytes[i + 1] == b'/' && i > 0 && bytes[i - 1] == b':' {
                i += 1;
                continue;
            }
            let before = &line[..i];
            if before.bytes().any(|b| b != b' ' && b != b'\t') {
                if bytes[i + 1] == b'/' {
                    // `//` extends to end of line — always trailing.
                    return Some(i);
                }
                // `/* */` — only trailing when nothing non-whitespace follows `*/`.
                let mut j = i + 2;
                while j + 1 < bytes.len() {
                    if bytes[j] == b'*' && bytes[j + 1] == b'/' {
                        let after = &line[j + 2..];
                        if !after.bytes().any(|b| b != b' ' && b != b'\t') {
                            return Some(i);
                        }
                        break; // code after `*/` — not a trailing comment
                    }
                    j += 1;
                }
            }
        }
        i += 1;
    }
    None
}

/// Align trailing `//` comments within groups of lines carrying trailing
/// comments.  Lines without a comment are allowed inside a group when they
/// are no more than `span` non-commented lines away from the next commented
/// line (matches uncrustify's `align_right_cmt_span` semantics).
/// `min_gap` is the minimum number of spaces between code end and comment.
fn align_trailing_comments(
    output: &str,
    nl: &str,
    min_gap: usize,
    normalize_single: bool,
    on_tabstop: bool,
    tab_width: usize,
    span: usize,
) -> String {
    let lines: Vec<&str> = output.split(nl).collect();
    let n = lines.len();
    let cols: Vec<Option<usize>> = lines.iter().map(|l| trailing_comment_col(l)).collect();
    let mut result: Vec<String> = lines.iter().map(|l| l.to_string()).collect();

    let mut i = 0;
    while i < n {
        if cols[i].is_some() {
            // Extend the group as long as the next commented line is within
            // `span` non-commented lines of the last commented line found.
            let mut last_cmt = i;
            let mut scan = i + 1;
            loop {
                // Find the next commented line from `scan`.
                let next = (scan..n).find(|&k| cols[k].is_some());
                match next {
                    Some(k) if k - last_cmt < span => {
                        last_cmt = k;
                        scan = k + 1;
                    }
                    _ => break,
                }
            }
            let j = last_cmt + 1; // exclusive end of group

            let commented_in_group = (i..j).filter(|&k| cols[k].is_some()).count();
            let is_single = commented_in_group == 1;
            // Skip single-line groups unless normalize_single is set.
            if is_single && !normalize_single {
                i = j;
                continue;
            }
            let max_code_len = (i..j)
                .filter(|&k| cols[k].is_some())
                .map(|k| lines[k][..cols[k].unwrap()].trim_end().len())
                .max()
                .unwrap();
            let raw_target = max_code_len + min_gap;
            let target = if on_tabstop && tab_width > 0 {
                round_up_to_multiple(raw_target, tab_width)
            } else {
                raw_target
            };
            for k in i..j {
                if let Some(col) = cols[k] {
                    let code = lines[k][..col].trim_end();
                    let comment = &lines[k][col..];
                    let pad = target.max(code.len() + 1) - code.len();
                    result[k] = format!("{}{}{}", code, " ".repeat(pad), comment);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }

    result.join(nl)
}

/// Returns the byte index of the `=` assignment operator in an enum value line
/// (e.g. `    FOO = 3,`), or `None` if the line isn't an enum value with `=`.
///
/// Requires the line to end with `,` to avoid false-positives inside function
/// bodies. Skips compound operators (`==`, `!=`, `<=`, `>=`, `+=`, …).
fn enum_eq_col(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        return None;
    }
    // Enum members end with `,` (non-last) or with an alphanumeric/`_`/`)` (last
    // member).  Reject anything that ends with `;`, `{`, `}`, etc. to avoid
    // false-positives on declarations or initialiser lines.
    let last = trimmed.trim_end().chars().last().unwrap_or(' ');
    if !matches!(last, ',' | ')') && !last.is_alphanumeric() && last != '_' {
        return None;
    }
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut in_char = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_string || in_char => {
                i += 2; // skip escaped character
                continue;
            }
            b'"' if !in_char => {
                in_string = !in_string;
            }
            b'\'' if !in_string => {
                in_char = !in_char;
            }
            b'=' if !in_string && !in_char => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    i += 1;
                    continue; // ==
                }
                if i > 0
                    && matches!(
                        bytes[i - 1],
                        b'!' | b'<' | b'>' | b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^'
                    )
                {
                    i += 1;
                    continue; // compound op
                }
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// True when `line` looks like a bare enum member with no explicit value
/// (e.g. `    RED,` or `    RED, // comment`).  Used to let bare members
/// act as transparent connectors within an alignment group so that
/// `RED, GREEN = 5, BLUE, YELLOW = 10` all align their `=` signs together.
fn is_bare_enum_member(line: &str) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        return false;
    }
    if !trimmed.contains(',') {
        return false;
    }
    // No bare `=` — those are captured by enum_eq_col instead.
    !trimmed.contains('=')
}

/// Align `=` signs within groups of consecutive enum value lines.
/// Bare enum members (no explicit value) are transparent within a group —
/// they don't break alignment but are left unchanged themselves.
fn align_enum_equals(output: &str, nl: &str, on_tabstop: bool, tab_width: usize) -> String {
    let lines: Vec<&str> = output.split(nl).collect();
    let n = lines.len();
    let cols: Vec<Option<usize>> = lines.iter().map(|l| enum_eq_col(l)).collect();
    let mut result: Vec<String> = lines.iter().map(|l| l.to_string()).collect();

    let mut i = 0;
    while i < n {
        if cols[i].is_some() {
            // Extend the group through bare members, blank lines, and preprocessor
            // directives (#ifdef/#endif/#else) as well as `=` members.  All of these
            // are transparent connectors — they don't break the alignment group.
            // This matches uncrustify's behavior of aligning enum members across
            // conditional-compilation blocks.
            let mut j = i + 1;
            while j < n
                && (cols[j].is_some()
                    || is_bare_enum_member(lines[j])
                    || lines[j].trim().is_empty()
                    || lines[j].trim_start().starts_with('#'))
            {
                j += 1;
            }
            // Trim trailing blank/bare lines so they don't become orphaned group members.
            while j > i + 1 && cols[j - 1].is_none() {
                j -= 1;
            }
            // Collect indices of only the `=`-bearing lines in this group.
            let eq_indices: Vec<usize> = (i..j).filter(|&k| cols[k].is_some()).collect();
            if eq_indices.len() > 1 {
                let max_name_len = eq_indices
                    .iter()
                    .map(|&k| lines[k][..cols[k].unwrap()].trim_end().len())
                    .max()
                    .unwrap();
                let raw_target = max_name_len + 1;
                let target = if on_tabstop && tab_width > 0 {
                    round_up_to_multiple(raw_target, tab_width)
                } else {
                    raw_target
                };
                for k in eq_indices {
                    let col = cols[k].unwrap();
                    let name = lines[k][..col].trim_end();
                    let rest = &lines[k][col..]; // starts with `= …`
                    let pad = target - name.len();
                    result[k] = format!("{}{}{}", name, " ".repeat(pad), rest);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }

    result.join(nl)
}

/// Returns the byte index of the `/**<` that starts a Doxygen member comment
/// on `line`, or `None` if there is no such trailing comment.  Standalone
/// comment lines (where `/**<` is the first non-whitespace) return `None`.
fn trailing_doxygen_col(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with("/**<") {
        return None;
    }
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'*' && bytes[i + 2] == b'*' && bytes[i + 3] == b'<'
        {
            let before = &line[..i];
            if before.bytes().any(|b| b != b' ' && b != b'\t') {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// True for a struct/class member line that has no `/**<` comment but should not
/// break an alignment group — blank lines and closing-brace lines do break it.
fn is_transparent_doxygen_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    !trimmed.is_empty() && !trimmed.starts_with('}')
}

/// Align trailing `/**<` Doxygen member comments within groups of consecutive
/// lines that all carry such a comment.  Comment-less member lines (e.g. a field
/// with no doc) are transparent: they extend the group without being rewritten
/// themselves.  Blank lines and closing-brace lines break the group.
fn align_doxygen_comments(output: &str, nl: &str) -> String {
    let lines: Vec<&str> = output.split(nl).collect();
    let n = lines.len();
    let cols: Vec<Option<usize>> = lines.iter().map(|l| trailing_doxygen_col(l)).collect();
    let mut result: Vec<String> = lines.iter().map(|l| l.to_string()).collect();

    let mut i = 0;
    while i < n {
        if cols[i].is_some() {
            // Extend the group through comment-less member lines as well.
            let mut j = i + 1;
            while j < n && (cols[j].is_some() || is_transparent_doxygen_line(lines[j])) {
                j += 1;
            }
            // Trim trailing transparent lines that have no /**< after them.
            while j > i + 1 && cols[j - 1].is_none() {
                j -= 1;
            }
            let commented: Vec<usize> = (i..j).filter(|&k| cols[k].is_some()).collect();
            if commented.len() > 1 {
                let max_code_len = commented
                    .iter()
                    .map(|&k| lines[k][..cols[k].unwrap()].trim_end().len())
                    .max()
                    .unwrap();
                let target = max_code_len + 1;
                for k in commented {
                    let col = cols[k].unwrap();
                    let code = lines[k][..col].trim_end();
                    let comment = &lines[k][col..];
                    let pad = target - code.len();
                    result[k] = format!("{}{}{}", code, " ".repeat(pad), comment);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }

    result.join(nl)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SpacingConfig;
    use crate::lexer::tokenize;

    fn fmt(src: &str) -> String {
        let (tokens, _) = tokenize(src, "<test>").unwrap();
        format(&tokens, &Config::default()).unwrap()
    }

    fn fmt_with(src: &str, config: &Config) -> String {
        let (tokens, _) = tokenize(src, "<test>").unwrap();
        format(&tokens, config).unwrap()
    }

    #[test]
    fn basic_function() {
        let src = "int main(){return 0;}";
        let out = fmt(src);
        assert!(out.contains("int main()"), "got: {out}");
        assert!(out.contains("return 0;"), "got: {out}");
        assert!(out.contains('}'), "got: {out}");
    }

    #[test]
    fn indentation() {
        let src = "void f(){int x=1;}";
        let out = fmt(src);
        // The body should be indented by 4 spaces
        assert!(out.contains("    int x = 1;"), "got:\n{out}");
    }

    #[test]
    fn preserves_chinese_comment() {
        let src = "int x; // 变量定义\n";
        let out = fmt(src);
        assert!(out.contains("// 变量定义"), "got:\n{out}");
    }

    #[test]
    fn allman_brace_style() {
        use crate::config::{BraceConfig, BraceStyle};
        let config = Config {
            braces: BraceConfig {
                style: BraceStyle::Allman,
                cuddle_else: false,
                cuddle_catch: false,
                collapse_empty_body: false,
                expand_large_initializers: true,
                fn_brace_newline: false,
                ..BraceConfig::default()
            },
            ..Config::default()
        };
        let src = "if(x){y=1;}";
        let out = fmt_with(src, &config);
        // In Allman style, `{` is on its own line
        let brace_line = out.lines().find(|l| l.trim() == "{");
        assert!(brace_line.is_some(), "no standalone brace line in:\n{out}");
    }

    #[test]
    fn collapse_empty_function_body() {
        // With fn_brace_newline=true (default), `{` goes to its own line.
        // collapse_empty_body then collapses `{\n}` to `{}` on that same new line.
        let src = "void f() {\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("void f()\n{}"),
            "empty function body should collapse to {{}} on new line: {out}"
        );
    }

    #[test]
    fn collapse_empty_struct_body() {
        let src = "struct Foo {\n};\n";
        let out = fmt(src);
        assert!(
            out.contains("struct Foo {};"),
            "empty struct body should collapse to {{}}; on one line: {out}"
        );
    }

    #[test]
    fn collapse_empty_body_off() {
        use crate::config::{BraceConfig, BraceStyle};
        let config = Config {
            braces: BraceConfig {
                style: BraceStyle::Kr,
                cuddle_else: false,
                cuddle_catch: false,
                collapse_empty_body: false,
                expand_large_initializers: true,
                fn_brace_newline: true,
                ..BraceConfig::default()
            },
            ..Config::default()
        };
        let src = "void f() {\n}\n";
        let out = fmt_with(src, &config);
        assert!(
            !out.contains("void f() {}"),
            "collapse_empty_body=false should not collapse: {out}"
        );
    }

    #[test]
    fn nl_brace_else_default_newline() {
        // Default: nl_brace_else=true → `}\nelse {` regardless of cuddle_else.
        let src = "void f() { if (x) { a(); } else { b(); } }\n";
        let out = fmt(src);
        assert!(
            !out.contains("} else {"),
            "default nl_brace_else=true must put else on its own line: {out}"
        );
        assert!(
            out.contains("else {"),
            "else block must still be present: {out}"
        );
    }

    #[test]
    fn nl_brace_else_false_cuddles_when_cuddle_else_true() {
        // With nl_brace_else=false and cuddle_else=true → `} else {` on one line.
        let config = Config {
            braces: crate::config::BraceConfig {
                cuddle_else: true,
                ..crate::config::BraceConfig::default()
            },
            newlines: crate::config::NewlineConfig {
                nl_brace_else: false,
                ..crate::config::NewlineConfig::default()
            },
            ..Config::default()
        };
        let src = "void f() { if (x) { a(); } else { b(); } }\n";
        let out = fmt_with(src, &config);
        assert!(
            out.contains("} else {"),
            "cuddle_else=true, nl_brace_else=false should cuddle: {out}"
        );
    }

    #[test]
    fn nl_brace_else_true_overrides_cuddle() {
        // nl_brace_else=true forces newline before else even when cuddle_else=true.
        let config = Config {
            braces: crate::config::BraceConfig {
                cuddle_else: true,
                ..crate::config::BraceConfig::default()
            },
            newlines: crate::config::NewlineConfig {
                nl_brace_else: true,
                ..crate::config::NewlineConfig::default()
            },
            ..Config::default()
        };
        let src = "void f() { if (x) { a(); } else { b(); } }\n";
        let out = fmt_with(src, &config);
        assert!(
            !out.contains("} else"),
            "nl_brace_else=true must break before else: {out}"
        );
        assert!(
            out.lines().any(|l| l.trim() == "else {"),
            "else must start on its own line: {out}"
        );
    }

    #[test]
    fn nl_brace_else_true_else_if() {
        // nl_brace_else=true also applies to `else if (`.
        let config = Config {
            braces: crate::config::BraceConfig {
                cuddle_else: true,
                ..crate::config::BraceConfig::default()
            },
            newlines: crate::config::NewlineConfig {
                nl_brace_else: true,
                ..crate::config::NewlineConfig::default()
            },
            ..Config::default()
        };
        let src = "void f() { if (x) { a(); } else if (y) { b(); } }\n";
        let out = fmt_with(src, &config);
        assert!(
            !out.contains("} else if"),
            "nl_brace_else=true must break before else if: {out}"
        );
        assert!(
            out.lines().any(|l| l.trim().starts_with("else if")),
            "else if must start on its own line: {out}"
        );
    }

    #[test]
    fn array_initializer_stays_inline() {
        let src = "uint8_t rx[] = { 0 };";
        let out = fmt(src);
        assert!(
            out.trim_end().ends_with("= { 0 };"),
            "expected inline initializer, got:\n{out}"
        );
    }

    #[test]
    fn multi_element_initializer_stays_inline() {
        let src = "int a[] = {1, 2, 3};";
        let out = fmt(src);
        assert!(
            out.trim_end().ends_with("= { 1, 2, 3 };"),
            "expected inline initializer, got:\n{out}"
        );
    }

    #[test]
    fn multiline_initializer_not_collapsed_to_one_line() {
        // A multi-line initializer in the source must not be collapsed even if
        // the total element count is below the small-init threshold.
        let src = "uint8_t buf[] = {\n    0x30,\n    0x06,\n    0x00, 0x04,\n    'a', 'b', 'c', 'd'\n};\n";
        let out = fmt(src);
        assert!(
            out.contains("{\n"),
            "multi-line initializer must stay multi-line, got:\n{out}"
        );
        assert!(
            !out.contains("= { 0x30,"),
            "multi-line initializer must not be collapsed to one line, got:\n{out}"
        );
    }

    #[test]
    fn large_initializer_expands_one_per_line() {
        // 9 elements (17 non-ws tokens) exceeds the small-init threshold of 16.
        let src = "int a[9] = {0, 1, 2, 3, 4, 5, 6, 7, 8};";
        let out = fmt(src);
        assert!(
            out.contains("{\n    0,\n    1,"),
            "large initializer should expand one element per line, got:\n{out}"
        );
        assert!(
            out.contains("    8\n};"),
            "last element must not have trailing comma, got:\n{out}"
        );
    }

    #[test]
    fn large_initializer_expand_disabled() {
        use crate::config::{BraceConfig, BraceStyle};
        let config = Config {
            braces: BraceConfig {
                style: BraceStyle::Kr,
                cuddle_else: true,
                cuddle_catch: true,
                collapse_empty_body: true,
                expand_large_initializers: false,
                fn_brace_newline: true,
                ..BraceConfig::default()
            },
            ..Config::default()
        };
        let src = "int a[9] = {0, 1, 2, 3, 4, 5, 6, 7, 8};";
        let out = fmt_with(src, &config);
        assert!(
            !out.contains("0,\n"),
            "expand_large_initializers=false should keep inline, got:\n{out}"
        );
    }

    #[test]
    fn large_initializer_inline_comment_stays_on_same_line() {
        let src = "int a[9] = {0, // zero\n1, // one\n2, 3, 4, 5, 6, 7, 8};\n";
        let out = fmt(src);
        // Comments must remain on the same line as the element (not split off);
        // exact spacing may vary based on align_right_cmt_gap.
        assert!(
            out.lines()
                .any(|l| l.contains("0,") && l.contains("// zero")),
            "inline comment after element must stay on same line, got:\n{out}"
        );
        assert!(
            out.lines()
                .any(|l| l.contains("1,") && l.contains("// one")),
            "inline comment after second element must stay on same line, got:\n{out}"
        );
    }

    #[test]
    fn endif_comment_space_default_normalizes_to_1() {
        // Default: non-header-guard file gets 1 space (matches uncrustify on .c files).
        let src = "#endif  /* GUARD_H */\n";
        let out = fmt(src);
        assert!(
            out.contains("#endif /* GUARD_H */"),
            "expected single space before comment with default config: {out}"
        );
        assert!(
            !out.contains("#endif  "),
            "double space must not appear with default config: {out}"
        );
    }

    #[test]
    fn endif_comment_space_header_guard_uses_1_by_default() {
        // Header guard files get 1-space #endif by default (uncrustify's actual
        // rule is too complex to replicate — documented as By Design deviation).
        let src = "#ifndef GUARD_H\n#define GUARD_H\nint x;\n#endif /* GUARD_H */\n";
        let out = fmt(src);
        assert!(
            out.contains("#endif /* GUARD_H */"),
            "header-guard file should get 1-space #endif with default config: {out}"
        );
    }

    #[test]
    fn endif_comment_space_1_single_space() {
        let mut cfg = Config::default();
        cfg.preprocessor.endif_comment_space = 1;
        let src = "#ifdef FOO\nint x;\n#endif  /* FOO */\n";
        let out = fmt_with(src, &cfg);
        assert!(
            out.contains("#endif /* FOO */"),
            "expected single space before comment: {out}"
        );
        assert!(
            !out.contains("#endif  "),
            "double space must not appear with endif_comment_space=1: {out}"
        );
    }

    #[test]
    fn endif_comment_space_2_matches_uncrustify() {
        let mut cfg = Config::default();
        cfg.preprocessor.endif_comment_space = 2;
        let src = "#ifndef GUARD_H\n#define GUARD_H\n#endif /* GUARD_H */\n";
        let out = fmt_with(src, &cfg);
        assert!(
            out.contains("#endif  /* GUARD_H */"),
            "expected double space before comment: {out}"
        );
    }

    #[test]
    fn endif_no_comment_untouched() {
        // A bare `#endif` with no comment must not be modified.
        let src = "#ifndef GUARD_H\n#define GUARD_H\n#endif\n";
        let out = fmt(src);
        let endif_line = out.lines().find(|l| l.starts_with("#endif")).unwrap();
        assert_eq!(
            endif_line, "#endif",
            "bare #endif must not gain trailing space: {out}"
        );
    }

    #[test]
    fn preproc_preserved() {
        let src = "#include <stdio.h>\nint x;\n";
        let out = fmt(src);
        assert!(out.starts_with("#include <stdio.h>"), "got:\n{out}");
    }

    #[test]
    fn inline_comment_after_semi() {
        let src = "int x = 1; // note\nint y = 2;\n";
        let out = fmt(src);
        // Comment must stay on the same line as the statement.
        let line = out
            .lines()
            .find(|l| l.contains("int x"))
            .expect("no x line");
        assert!(line.contains("// note"), "comment moved off line: {out}");
        // Subsequent statement must be on its own line.
        assert!(out.contains("\nint y"), "y not on new line: {out}");
    }

    #[test]
    fn inline_block_comment_after_semi_stays_on_same_line() {
        // `/* */` trailing comment must not be moved to a new line by
        // the var-decl-block transition logic.
        let src = "void f() {\n    result_e r = OK; /* default */\n    int x = 0;\n}\n";
        let out = fmt(src);
        let r_line = out
            .lines()
            .find(|l| l.contains("result_e"))
            .expect("no r line");
        assert!(
            r_line.contains("/* default */"),
            "block comment moved off declaration line:\n{out}"
        );
    }

    #[test]
    fn inline_comment_after_semi_unicode() {
        let src = "int x = 1; // 变量定义\n";
        let out = fmt(src);
        let line = out
            .lines()
            .find(|l| l.contains("int x"))
            .expect("no x line");
        assert!(
            line.contains("// 变量定义"),
            "unicode comment moved off line: {out}"
        );
    }

    #[test]
    fn inline_comment_after_brace() {
        let src = "void f() {\n    return;\n} // end\n";
        let out = fmt(src);
        let brace_line = out
            .lines()
            .find(|l| l.trim_start().starts_with('}'))
            .expect("no } line");
        assert!(
            brace_line.contains("// end"),
            "comment not on }} line:\n{out}"
        );
    }

    #[test]
    fn non_inline_comment_stays_separate() {
        let src = "int x = 1;\n// standalone\nint y = 2;\n";
        let out = fmt(src);
        // The x-line must not contain the comment.
        let x_line = out
            .lines()
            .find(|l| l.contains("int x"))
            .expect("no x line");
        assert!(
            !x_line.contains("//"),
            "standalone comment merged into x line:\n{out}"
        );
        // The comment must appear on its own line.
        assert!(
            out.lines().any(|l| l.trim() == "// standalone"),
            "standalone comment missing:\n{out}"
        );
    }

    #[test]
    fn switch_case_indentation() {
        // Default: indent_switch_case=true, so case labels are at switch-body level
        // and case bodies are one further level in.
        let src = "void f(int x){switch(x){case 1:y=1;break;case 2:y=2;break;default:y=0;break;}}";
        let out = fmt(src);
        assert!(
            out.lines().any(|l| l == "        case 1:"),
            "case 1 not at switch-body level:\n{out}"
        );
        assert!(
            out.lines().any(|l| l == "        case 2:"),
            "case 2 not at switch-body level:\n{out}"
        );
        assert!(
            out.lines().any(|l| l == "        default:"),
            "default not at switch-body level:\n{out}"
        );
        // Case body must be indented one further level (12 spaces at func level 1).
        assert!(
            out.lines().any(|l| l.starts_with("            y")),
            "case body not indented deeper than label:\n{out}"
        );
    }

    #[test]
    fn switch_case_indentation_disabled() {
        use crate::config::{IndentConfig, IndentStyle};
        let cfg = Config {
            indent: IndentConfig {
                style: IndentStyle::Spaces,
                width: 4,
                indent_switch_case: false,
                indent_goto_labels: false,
            },
            ..Config::default()
        };
        let src = "void f(int x){switch(x){case 1:y=1;break;default:y=0;break;}}";
        let out = fmt_with(src, &cfg);
        // When disabled, case labels dedent to the switch level (4 spaces).
        assert!(
            out.lines().any(|l| l == "    case 1:"),
            "case 1 not dedented to switch level:\n{out}"
        );
        assert!(
            out.lines().any(|l| l == "    default:"),
            "default not dedented to switch level:\n{out}"
        );
        // Case body at switch-body level (8 spaces).
        assert!(
            out.lines().any(|l| l.starts_with("        y")),
            "case body not at switch-body level:\n{out}"
        );
    }

    #[test]
    fn case_label_followed_by_brace_on_next_line() {
        // Source pattern from hostap: `case X:\n{\n    body\n}`
        // The `{` must be attached to the case label (KR style) and the body
        // must be at case-body indentation (not double-indented).
        let src = "void f(int x){\nswitch(x){\ncase 1:\n{\nint y=x;\nbreak;\n}\ncase 2:\n{\nint z=x;\nbreak;\n}\n}\n}";
        let out = fmt(src);
        assert!(
            out.contains("case 1: {"),
            "case brace must be attached to case label: {out}"
        );
        // Body must be at 12 spaces (3 indent levels: fn=1, switch=2, case-body=3).
        assert!(
            out.lines().any(|l| l == "            int y = x;"),
            "case block body must be at 12 spaces, not over-indented: {out}"
        );
        // Next case label must be at 8 spaces (same as before).
        assert!(
            out.lines().any(|l| l == "        case 2: {"),
            "second case label must be at 8 spaces: {out}"
        );
    }

    #[test]
    fn pointer_align_middle() {
        // explicit middle config: int * p
        let src = "int*p;";
        let out = fmt_with(
            src,
            &Config {
                spacing: SpacingConfig {
                    pointer_align: PointerAlign::Middle,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert!(out.contains("int * p"), "middle mode: got\n{out}");
    }

    #[test]
    fn pointer_align_type() {
        use crate::config::{PointerAlign, SpacingConfig};
        let config = Config {
            spacing: SpacingConfig {
                pointer_align: PointerAlign::Type,
                ..SpacingConfig::default()
            },
            ..Config::default()
        };
        let src = "int*p;";
        let out = fmt_with(src, &config);
        assert!(out.contains("int* p"), "type mode: got\n{out}");
    }

    #[test]
    fn pointer_align_name() {
        use crate::config::{PointerAlign, SpacingConfig};
        let config = Config {
            spacing: SpacingConfig {
                pointer_align: PointerAlign::Name,
                ..SpacingConfig::default()
            },
            ..Config::default()
        };
        let src = "int*p;";
        let out = fmt_with(src, &config);
        assert!(out.contains("int *p"), "name mode: got\n{out}");
    }

    #[test]
    fn pointer_align_name_trailing_comment() {
        // Regression: `Type *name /* comment */` was mis-classified as binary
        // multiplication because the forward scan hit the comment before a
        // terminator.  The star should be treated as a pointer declarator.
        use crate::config::{PointerAlign, SpacingConfig};
        let config = Config {
            spacing: SpacingConfig {
                pointer_align: PointerAlign::Name,
                ..SpacingConfig::default()
            },
            ..Config::default()
        };
        let src = "void f(HashElem *elem /* the elem */, Hash *pH /* the hash */);\n";
        let out = fmt_with(src, &config);
        assert!(
            out.contains("HashElem *elem"),
            "name mode with trailing comment: got\n{out}"
        );
        assert!(
            out.contains("Hash *pH"),
            "name mode with trailing comment (pH): got\n{out}"
        );
    }

    #[test]
    fn pointer_align_double_star_type() {
        use crate::config::{PointerAlign, SpacingConfig};
        let config = Config {
            spacing: SpacingConfig {
                pointer_align: PointerAlign::Type,
                ..SpacingConfig::default()
            },
            ..Config::default()
        };
        let src = "int**p;";
        let out = fmt_with(src, &config);
        assert!(out.contains("int** p"), "type double-ptr: got\n{out}");
    }

    #[test]
    fn pointer_align_double_star_name() {
        use crate::config::{PointerAlign, SpacingConfig};
        let config = Config {
            spacing: SpacingConfig {
                pointer_align: PointerAlign::Name,
                ..SpacingConfig::default()
            },
            ..Config::default()
        };
        let src = "int**p;";
        let out = fmt_with(src, &config);
        assert!(out.contains("int **p"), "name double-ptr: got\n{out}");
    }

    #[test]
    fn pointer_align_does_not_affect_multiplication() {
        // a * b is multiplication — pointer_align=type must not strip its spaces
        use crate::config::{PointerAlign, SpacingConfig};
        let config = Config {
            spacing: SpacingConfig {
                pointer_align: PointerAlign::Type,
                ..SpacingConfig::default()
            },
            ..Config::default()
        };
        let src = "int r=a*b;";
        let out = fmt_with(src, &config);
        assert!(out.contains("a * b"), "multiplication spaces: got\n{out}");
    }

    #[test]
    fn pointer_align_name_user_defined_type() {
        use crate::config::{PointerAlign, SpacingConfig};
        let config = Config {
            spacing: SpacingConfig {
                pointer_align: PointerAlign::Name,
                ..SpacingConfig::default()
            },
            ..Config::default()
        };
        // User-defined type names (Ident) must also get pointer_align applied.
        let src = "MyType * p;";
        let out = fmt_with(src, &config);
        assert!(out.contains("MyType *p"), "name mode user type: got\n{out}");
    }

    #[test]
    fn pointer_align_name_no_affect_on_multiplication_with_user_ident() {
        use crate::config::{PointerAlign, SpacingConfig};
        let config = Config {
            spacing: SpacingConfig {
                pointer_align: PointerAlign::Name,
                ..SpacingConfig::default()
            },
            ..Config::default()
        };
        // a * b after assignment is multiplication, not a pointer declarator.
        let src = "int r=a*b;";
        let out = fmt_with(src, &config);
        assert!(
            out.contains("a * b"),
            "multiplication after assign: got\n{out}"
        );
    }

    #[test]
    fn unary_dereference_no_space() {
        let src = "int x=*ptr;";
        let out = fmt(src);
        assert!(
            out.contains("*ptr"),
            "unary * must not gain a space: got\n{out}"
        );
    }

    #[test]
    fn unary_address_of_no_space() {
        let src = "int*p=&data;";
        let out = fmt(src);
        assert!(
            out.contains("&data"),
            "unary & must not gain a space: got\n{out}"
        );
    }

    #[test]
    fn unary_address_of_after_assign_no_space() {
        let src = "p=&x;";
        let out = fmt(src);
        assert!(
            out.contains("&x"),
            "unary & after = must not gain a space: got\n{out}"
        );
    }

    #[test]
    fn deref_after_block_comment_no_space() {
        // `/* comment */\n*ptr = val;` — deref at line start after a block
        // comment must not gain a space between `*` and the name.
        let src = "void f(void) {\n/* get it */\n*ptr = val;\n}";
        let out = fmt(src);
        assert!(
            out.contains("*ptr"),
            "deref at line start after block comment must not gain a space: got\n{out}"
        );
        assert!(!out.contains("* ptr"), "got spurious space in deref: {out}");
    }

    #[test]
    fn brace_after_comment_retains_indent() {
        // `if (cond) /* note */\n{` — KR enforcement must move { to the same
        // line, not emit it at column 0.
        let src = "void f(void) {\nif (x) /* note */\n{\nreturn;\n}\n}";
        let out = fmt(src);
        assert!(
            out.contains("if (x) /* note */ {"),
            "control-flow brace must be on same line after trailing comment: got\n{out}"
        );
    }

    #[test]
    fn fn_brace_newline_default() {
        // Default: function-definition { goes on its own line.
        let src = "int foo(void) { return 0; }";
        let out = fmt(src);
        assert!(
            out.contains("foo(void)\n"),
            "function brace must be on new line by default: got\n{out}"
        );
    }

    #[test]
    fn fn_brace_newline_false_keeps_same_line() {
        use crate::config::BraceConfig;
        let cfg = Config {
            braces: BraceConfig {
                fn_brace_newline: false,
                ..BraceConfig::default()
            },
            ..Config::default()
        };
        let src = "int foo(void) { return 0; }";
        let out = fmt_with(src, &cfg);
        assert!(
            out.contains("foo(void) {"),
            "fn_brace_newline=false must keep brace on same line: got\n{out}"
        );
    }

    #[test]
    fn ctrl_flow_brace_same_line_with_fn_newline() {
        // Even when fn_brace_newline=true, control-flow braces stay on same line.
        let src = "void f(void) {\nif (x)\n{\nreturn;\n}\n}";
        let out = fmt(src);
        assert!(
            out.contains("if (x) {"),
            "control-flow brace must stay on same line: got\n{out}"
        );
    }

    #[test]
    fn template_no_spaces_default() {
        // Default: no spaces inside angle brackets.
        let src = "std::vector<int> v;";
        let out = fmt(src);
        assert!(out.contains("vector<int>"), "got:\n{out}");
    }

    #[test]
    fn template_map_two_args() {
        let src = "std::map<std::string,int> m;";
        let out = fmt(src);
        assert!(out.contains("map<std::string, int>"), "got:\n{out}");
    }

    #[test]
    fn template_nested() {
        let src = "std::vector<std::vector<int>> vv;";
        let out = fmt(src);
        assert!(out.contains("vector<std::vector<int>>"), "got:\n{out}");
    }

    #[test]
    fn template_space_inside() {
        use crate::config::SpacingConfig;
        let config = Config {
            spacing: SpacingConfig {
                space_inside_angle_brackets: true,
                ..SpacingConfig::default()
            },
            ..Config::default()
        };
        let src = "std::vector<int> v;";
        let out = fmt_with(src, &config);
        assert!(out.contains("vector< int >"), "got:\n{out}");
    }

    #[test]
    fn template_declaration_keyword() {
        // `template<…>` — keyword triggers the heuristic.
        let src = "template<typename T> void f(T x);";
        let out = fmt(src);
        assert!(out.contains("template<typename T>"), "got:\n{out}");
    }

    #[test]
    fn comparison_less_than_unchanged() {
        // Plain comparison: spaces must be preserved.
        let src = "int r=(a<b);";
        let out = fmt(src);
        assert!(out.contains("a < b"), "comparison lost spaces:\n{out}");
    }

    #[test]
    fn comparison_greater_than_unchanged() {
        let src = "int r=(a>b);";
        let out = fmt(src);
        assert!(out.contains("a > b"), "comparison lost spaces:\n{out}");
    }

    #[test]
    fn template_constructor_call() {
        // No space between `>` and `(` for constructor call.
        let src = "auto v=std::vector<int>();";
        let out = fmt(src);
        assert!(out.contains("vector<int>()"), "got:\n{out}");
    }

    fn fmt_with_var_decl_blank(src: &str) -> String {
        let mut config = Config::default();
        config.newlines.blank_line_after_var_decl_block = true;
        fmt_with(src, &config)
    }

    #[test]
    fn var_decl_block_blank_line_inserted() {
        let src = "void f() {\n    int x = 1;\n    int y = 2;\n    foo(x);\n}\n";
        let out = fmt_with_var_decl_blank(src);
        let lines: Vec<&str> = out.lines().collect();
        // Find the blank line between `int y = 2;` and `foo(x);`
        let decl_line = lines.iter().position(|l| l.contains("int y")).unwrap();
        assert_eq!(
            lines[decl_line + 1],
            "",
            "expected blank line after last decl:\n{out}"
        );
        assert!(
            lines[decl_line + 2].contains("foo"),
            "foo not after blank:\n{out}"
        );
    }

    #[test]
    fn var_decl_block_no_blank_when_no_decls() {
        // Function with no leading declarations: no blank line added.
        let src = "void g() {\n    foo();\n    bar();\n}\n";
        let out = fmt_with_var_decl_blank(src);
        assert!(
            !out.contains("\n\n    bar"),
            "should not add blank between two statements: {out}"
        );
    }

    #[test]
    fn var_decl_block_on_by_default() {
        // Feature is on by default — blank line is inserted.
        let src = "void f() {\n    int x = 1;\n    foo(x);\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("1;\n\n"),
            "blank line must appear after var decl block by default: {out}"
        );
    }

    #[test]
    fn var_decl_block_no_false_positive_in_for_body() {
        // A variable declaration inside a for-loop body must not trigger the
        // blank_line_after_var_decl_block rule — that only applies to the
        // leading declaration run at function scope.
        let src = "void f(void) {\n    for (;;) {\n        int c = 1;\n        x++;\n    }\n}\n";
        let out = fmt(src);
        assert!(
            !out.contains("1;\n\n"),
            "blank line must NOT be inserted after decl inside for body: {out}"
        );
    }

    #[test]
    fn var_decl_block_no_false_positive_in_if_body() {
        // Same rule: decl inside an if block body must not trigger the feature.
        let src = "void f(void) {\n    int x = 1;\n    if (x) {\n        const int c = 2;\n        foo(c);\n    }\n}\n";
        let out = fmt(src);
        // The blank line after `int x = 1;` (at function scope) is correct.
        // There must NOT be a blank line after `const int c = 2;` inside the if body.
        let lines: Vec<&str> = out.lines().collect();
        let c_line = lines
            .iter()
            .position(|l| l.contains("const int c"))
            .unwrap();
        assert_ne!(
            lines.get(c_line + 1).copied().unwrap_or("X"),
            "",
            "blank line must NOT follow decl inside if body: {out}"
        );
    }

    #[test]
    fn var_decl_block_user_defined_type() {
        // `TypeName var;` where TypeName is a user-defined type (Ident, not a keyword)
        // must be treated as a declaration so the blank-line rule fires.
        let src =
            "void f(void) {\n    SerialComm_T data;\n    memset(&data, 0, sizeof(data));\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("data;\n\n"),
            "blank line must follow user-defined-type decl: {out}"
        );
    }

    #[test]
    fn var_decl_block_user_defined_type_pointer() {
        // Pointer to user-defined type: `TypeName *ptr;` should also count.
        let src = "void f(void) {\n    MyType *ptr;\n    use(ptr);\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("ptr;\n\n"),
            "blank line must follow pointer-to-user-type decl: {out}"
        );
    }

    #[test]
    fn var_decl_block_comment_before_first_decl_is_transparent() {
        // A leading block comment before the first declaration must not end
        // the declaration run — the blank line should still appear after the
        // declarations and before the first real statement.
        let src = "void f(void) {\n    /* preamble */\n    MyType data;\n    use(data);\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("data;\n\n"),
            "blank line must follow decl even when preceded by a block comment: {out}"
        );
    }

    #[test]
    fn var_decl_block_comment_between_decls_is_transparent() {
        // A section comment between two declarations must not end the var-decl
        // block. The blank line should appear after the last declaration, not
        // before the intermediate comment.
        let src =
            "void f(void) {\n    int x = 0;\n    /* WHEN */\n    int y = 1;\n    use(x, y);\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("int y = 1;\n\n"),
            "blank line must follow last decl, not appear before mid-block comment:\n{out}"
        );
        // No blank line should be inserted between declarations.
        assert!(
            !out.contains("int x = 0;\n\n"),
            "no blank line should appear after first decl:\n{out}"
        );
    }

    #[test]
    fn var_decl_block_macro_calls_not_treated_as_decls() {
        // Adjacent macro calls on separate lines (MACRO_A\nMACRO_B(...)) must
        // not be mistaken for `TypeName varName` declarations.
        let src =
            "void f(void) {\n    int x = 0;\n    EXPECT_BEGIN\n    ASSERT(x);\n    VERIFY_END\n}\n";
        let out = fmt(src);
        // No spurious blank line between ASSERT(...); and VERIFY_END.
        assert!(
            !out.contains("ASSERT(x);\n\n"),
            "no blank line should appear between macro calls:\n{out}"
        );
    }

    #[test]
    fn var_decl_block_attribute_macro_plus_const_type_is_decl() {
        // `ATTR_MACRO const Type* var = ...` must be recognized as a declaration
        // so it stays in the var-decl block.
        let src =
            "void f(void) {\n    int n = 4;\n    ATTR const float* p = buf;\n    use(p);\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("buf;\n\n"),
            "blank line must follow last decl (ATTR const float* p):\n{out}"
        );
        assert!(
            !out.contains("int n = 4;\n\n"),
            "no blank line should split the var-decl block:\n{out}"
        );
    }

    #[test]
    fn var_decl_block_trailing_comment_before_ifdef() {
        // Trailing inline /* comment */ after the last decl must not suppress
        // the blank line that should appear before the following #ifdef.
        let src = "void f(void) {\n    int a;\n    int b; /* note */\n#ifdef X\n    int c;\n#endif\n    a = 1;\n}\n";
        let out = fmt_with_var_decl_blank(src);
        assert!(
            out.contains("b; /* note */\n\n#ifdef"),
            "blank line must appear before #ifdef after trailing comment: {out}"
        );
    }

    #[test]
    fn var_decl_block_macro_function_body() {
        // A macro-defined function body at global scope (e.g. SM_STATE(...) { })
        // must activate the var-decl blank-line rule.
        let src = "}\n\nSM_STATE(WPA, START) {\n    int x;\n    int y;\n#ifdef X\n    int z;\n#endif\n    x = 1;\n}\n";
        let out = fmt_with_var_decl_blank(src);
        assert!(
            out.contains("int y;\n\n#ifdef") || out.contains("int y;\n\n#ifdef"),
            "blank line must appear in SM_STATE macro body before #ifdef: {out}"
        );
    }

    #[test]
    fn blank_line_cap() {
        let src = "int a;\n\n\n\n\nint b;\n";
        let out = fmt(src);
        // max_blank_lines = 2 by default
        let blanks = out
            .lines()
            .collect::<Vec<_>>()
            .windows(3)
            .filter(|w| w[0].is_empty() && w[1].is_empty() && w[2].is_empty())
            .count();
        assert_eq!(blanks, 0, "too many consecutive blank lines:\n{out}");
    }

    #[test]
    fn block_comment_two_blank_lines_preserved() {
        // Two blank lines between block comments must not collapse to one.
        // max_blank_lines=2 should allow both to survive.
        let src = "/*\n * A.\n */\n\n\n/*\n * B.\n */\n";
        let out = fmt(src);
        assert_eq!(
            out,
            "/*\n * A.\n */\n\n\n/*\n * B.\n */\n",
            "two blank lines between block comments collapsed:\n{out}"
        );
    }

    #[test]
    fn access_specifier_indentation() {
        let src = "class Foo{public:int x;private:int y;protected:int z;};";
        let out = fmt(src);
        // public/private/protected must be at class brace level (4 spaces), not member level (8).
        assert!(
            out.lines().any(|l| l == "public:"),
            "public: not at class indent level:\n{out}"
        );
        assert!(
            out.lines().any(|l| l == "private:"),
            "private: not at class indent level:\n{out}"
        );
        assert!(
            out.lines().any(|l| l == "protected:"),
            "protected: not at class indent level:\n{out}"
        );
        // Members must be indented one level deeper than the access specifier.
        assert!(
            out.lines().any(|l| l == "    int x;"),
            "member not indented past access specifier:\n{out}"
        );
    }

    #[test]
    fn access_specifier_in_inheritance_not_dedented() {
        // `public` in base-class list must not be treated as an access specifier label.
        let src = "class Bar {};\nclass Foo : public Bar {};\n";
        let out = fmt(src);
        // The line containing 'public Bar' must not be just "public" at column 0.
        assert!(
            !out.lines().any(|l| l.trim() == "public:"),
            "inheritance public wrongly treated as label:\n{out}"
        );
    }

    #[test]
    fn void_cast_statement_indented() {
        // (void)expr; must keep indentation. Default space_after_cast=preserve so
        // no-space source stays no-space. fn_brace_newline=true puts { on its own line.
        let src = "void f() {\n    (void)func();\n    (void)bar(1, 2);\n}\n";
        let out = fmt(src);
        assert_eq!(
            out,
            "void f()\n{\n    (void)func();\n    (void)bar(1, 2);\n}\n"
        );
    }

    #[test]
    fn cast_space_after_cast_false() {
        let mut cfg = Config::default();
        cfg.spacing.space_after_cast = SpaceOption::Remove;
        let src = "void f() { int x = (int)3.14; }\n";
        let out = fmt_with(src, &cfg);
        assert!(
            out.contains("(int)3.14"),
            "space_after_cast=false should produce no space: {out}"
        );
    }

    #[test]
    fn cast_double_cast_no_space() {
        // Chained casts: (double)(int)x — no space between the two parens regardless of
        // space_after_cast, because the inner `(int)x` is itself a cast expression.
        let mut cfg = Config::default();
        cfg.spacing.space_after_cast = SpaceOption::Remove;
        let src = "void f() { double d = (double)(int)x; }\n";
        let out = fmt_with(src, &cfg);
        assert!(
            out.contains("(double)(int)x"),
            "double cast should have no space between with space_after_cast=false: {out}"
        );
    }

    #[test]
    fn cast_space_after_cast_true() {
        let mut cfg = Config::default();
        cfg.spacing.space_after_cast = SpaceOption::Add;
        // Primitive cast before identifier.
        let src = "void f() { int x = (int)3.14; }\n";
        let out = fmt_with(src, &cfg);
        assert!(out.contains("(int) 3.14"), "cast+ident: {out}");
        // Cast before parenthesised subexpression.
        let src = "void f() { int y = (int)(x + 1); }\n";
        let out = fmt_with(src, &cfg);
        assert!(out.contains("(int) (x + 1)"), "cast+paren: {out}");
        // Chained casts: space only after the innermost cast.
        let src = "void f() { double d = (double)(int)x; }\n";
        let out = fmt_with(src, &cfg);
        assert!(out.contains("(double) (int) x"), "chained casts: {out}");
    }

    #[test]
    fn cast_user_defined_type_no_space() {
        // (MyType) val — user-defined type cast honors space_after_cast=false.
        let mut cfg = Config::default();
        cfg.spacing.space_after_cast = SpaceOption::Remove;
        let src = "void f() { MyType x = (MyType) val; }\n";
        let out = fmt_with(src, &cfg);
        assert!(
            out.contains("(MyType)val"),
            "user-defined type cast should have no space: {out}"
        );
    }

    #[test]
    fn cast_followed_by_dereference_no_space() {
        // (type)*ptr — cast + dereference: space_after_cast does not insert space before `*`.
        let mut cfg = Config::default();
        cfg.spacing.space_after_cast = SpaceOption::Remove;
        let src = "void f() { uint32_t x = (uint32_t)*ptr; int y = (int)*p; }\n";
        let out = fmt_with(src, &cfg);
        assert!(out.contains("(uint32_t)*ptr"), "cast+deref uint32_t: {out}");
        assert!(out.contains("(int)*p"), "cast+deref int: {out}");
    }

    #[test]
    fn designated_initializer_no_space_around_dot() {
        // {.field = val} — dot must have no space after it, but space before
        // when it follows a comma (not after an expression).
        let src = "void f() { tlv_t r = {.value = 1, .len = 2}; }\n";
        let out = fmt(src);
        assert!(
            out.contains("{ .value = 1, .len = 2 }"),
            "designated init spacing: {out}"
        );
    }

    #[test]
    fn initializer_negative_literal_no_space() {
        // {1, -2, -95} — unary minus inside a small initializer must not gain a space.
        let src = "void f() { char p[] = {1, -2, -95}; }\n";
        let out = fmt(src);
        assert!(out.contains("{ 1, -2, -95 }"), "negative literals: {out}");
    }

    #[test]
    fn cast_user_defined_pointer_no_space() {
        // (MyType *) val — pointer cast with space_after_cast=false.
        let mut cfg = Config::default();
        cfg.spacing.space_after_cast = SpaceOption::Remove;
        let src = "void f() { MyType *x = (MyType *) val; }\n";
        let out = fmt_with(src, &cfg);
        assert!(
            out.contains("(MyType*)val") || out.contains("(MyType *)val"),
            "user-defined pointer cast should have no space after ')': {out}"
        );
    }

    #[test]
    fn if_condition_ident_not_misclassified_as_cast() {
        // `if(x) stmt` — the `(x)` must NOT be treated as a cast.
        // With add_braces_to_if=true (default), bodies get wrapped; check that the
        // condition still gets proper space after `)`.
        let src = "void f(void) { if(x) foo(); while(p) bar(); }\n";
        let out = fmt(src);
        assert!(
            out.contains("if (x)"),
            "space missing after if condition: {out}"
        );
        assert!(
            out.contains("while (p)"),
            "space missing after while condition: {out}"
        );
        // Verify foo() and bar() appear somewhere (they're now inside braces).
        assert!(out.contains("foo()"), "foo() missing: {out}");
        assert!(out.contains("bar()"), "bar() missing: {out}");
    }

    #[test]
    fn space_before_keyword_paren_true() {
        // Default: space between control keyword and `(`.
        let src = "void f() { for (int i=0;i<10;i++){} while(x){} if(x){} switch(x){} }\n";
        let out = fmt(src);
        assert!(out.contains("for ("), "for: {out}");
        assert!(out.contains("while ("), "while: {out}");
        assert!(out.contains("if ("), "if: {out}");
        assert!(out.contains("switch ("), "switch: {out}");
    }

    #[test]
    fn space_before_keyword_paren_false() {
        let mut cfg = Config::default();
        cfg.spacing.space_before_keyword_paren = false;
        let src = "void f() { for (int i=0;i<10;i++){} while (x){} if (x){} switch (x){} }\n";
        let out = fmt_with(src, &cfg);
        assert!(out.contains("for("), "for: {out}");
        assert!(out.contains("while("), "while: {out}");
        assert!(out.contains("if("), "if: {out}");
        assert!(out.contains("switch("), "switch: {out}");
    }

    #[test]
    fn indent_style_tabs() {
        use crate::config::{IndentConfig, IndentStyle};
        let cfg = Config {
            indent: IndentConfig {
                style: IndentStyle::Tabs,
                width: 4,
                indent_switch_case: true,
                indent_goto_labels: false,
            },
            ..Config::default()
        };
        let src = "void f(int x) { if (x > 0) { foo(x); } }\n";
        let out = fmt_with(src, &cfg);
        // Each indent level must use a literal tab, not spaces.
        assert!(out.contains("\tif (x > 0)"), "level-1 indent: {out}");
        assert!(out.contains("\t\tfoo(x)"), "level-2 indent: {out}");
        assert!(!out.contains("    "), "no 4-space runs: {out}");
    }

    #[test]
    fn indent_style_tabs_idempotent() {
        use crate::config::{IndentConfig, IndentStyle};
        let cfg = Config {
            indent: IndentConfig {
                style: IndentStyle::Tabs,
                width: 4,
                indent_switch_case: true,
                indent_goto_labels: false,
            },
            ..Config::default()
        };
        let src = "void f() {\n\tif (x) {\n\t\tfoo();\n\t}\n}\n";
        let pass1 = fmt_with(src, &cfg);
        let pass2 = fmt_with(&pass1, &cfg);
        assert_eq!(pass1, pass2, "tab-indent formatting must be idempotent");
    }

    #[test]
    fn unary_after_assignment_space() {
        // = &ptr, = -1, = *ptr, = +val must preserve space after `=`.
        let src = "void f() { int a = &ptr; int b = -1; int c = *ptr; int d = +val; }\n";
        let out = fmt(src);
        assert!(out.contains("= &ptr"), "= &ptr: {out}");
        assert!(out.contains("= -1"), "= -1: {out}");
        assert!(out.contains("= *ptr"), "= *ptr: {out}");
        assert!(out.contains("= +val"), "= +val: {out}");
    }

    #[test]
    fn unary_no_space_after_op() {
        // Unary -/+/*/& must not gain a space between op and operand.
        let src = "void f() { return -1; return *ptr; return &x; int z = x + -y; }\n";
        let out = fmt(src);
        assert!(out.contains("return -1"), "return -1: {out}");
        assert!(out.contains("return *ptr"), "return *ptr: {out}");
        assert!(out.contains("return &x"), "return &x: {out}");
        assert!(out.contains("+ -y"), "x + -y: {out}");
    }

    #[test]
    fn sizeof_no_space_before_paren() {
        let src = "int x = sizeof(int);\n";
        let out = fmt(src);
        assert!(
            out.contains("sizeof(int)"),
            "sizeof should not get space before paren: {out}"
        );
    }

    #[test]
    fn sizeof_minus_not_unary() {
        // sizeof(x) ends an expression; `-` after it must be treated as binary.
        let src = "void f(void) {\n    strncpy(buf, src, sizeof(buf) - 1);\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("sizeof(buf) - 1"),
            "minus after sizeof() must be binary (space on both sides): {out}"
        );
    }

    #[test]
    fn struct_closing_brace_semicolon_same_line() {
        // `struct Foo { … };` must not put `;` on its own line.
        let src = "struct Point { int x; int y; };\n";
        let out = fmt(src);
        assert!(
            out.contains("};\n"),
            "semicolon must follow closing brace on same line: {out}"
        );
        assert!(
            !out.contains("}\n;"),
            "semicolon must not be on its own line: {out}"
        );
    }

    #[test]
    fn alignof_no_space_before_paren() {
        let src = "int x = alignof(int);\n";
        let out = fmt(src);
        assert!(
            out.contains("alignof(int)"),
            "alignof should not get space before paren: {out}"
        );
    }

    #[test]
    fn block_comment_after_closing_brace_stays_on_same_line() {
        let src = "extern \"C\" {\nint f();\n} /* extern \"C\" */\n";
        let out = fmt(src);
        assert!(
            out.lines()
                .any(|l| l.starts_with('}') && l.contains("/* extern \"C\" */")),
            "trailing block comment should stay on same line as closing brace, got:\n{out}"
        );
    }

    #[test]
    fn extern_c_block_no_indent() {
        let src = "extern \"C\" {\nint foo(void);\nvoid bar(int x);\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("\nint foo(void);"),
            "declarations inside extern \"C\" should not be indented, got:\n{out}"
        );
        assert!(
            out.contains("\nvoid bar(int x);"),
            "declarations inside extern \"C\" should not be indented, got:\n{out}"
        );
    }

    #[test]
    fn extern_c_brace_force_same_line() {
        // Default (force_same_line): { always on same line regardless of source.
        let src_allman = "extern \"C\"\n{\nint foo(void);\n}\n";
        let src_kr = "extern \"C\" {\nint foo(void);\n}\n";

        for src in [src_allman, src_kr] {
            let out = fmt(src);
            assert!(
                out.contains("extern \"C\" {"),
                "extern \"C\" {{ must be forced to same line (default config), got:\n{out}"
            );
        }
    }

    #[test]
    fn extern_c_brace_preserve() {
        let mut cfg = Config::default();
        cfg.braces.extern_c_brace = ExternCBrace::Preserve;

        // Source has brace on next line — preserve keeps it there.
        let src_allman = "extern \"C\"\n{\nint foo(void);\n}\n";
        let out_allman = fmt_with(src_allman, &cfg);
        assert!(
            out_allman.lines().any(|l| l.trim() == "{"),
            "extern_c_brace=preserve must keep brace on its own line, got:\n{out_allman}"
        );

        // Source has brace on same line — preserve keeps it there too.
        let src_kr = "extern \"C\" {\nint foo(void);\n}\n";
        let out_kr = fmt_with(src_kr, &cfg);
        assert!(
            out_kr.contains("extern \"C\" {"),
            "extern_c_brace=preserve must keep brace on same line when source has it there, got:\n{out_kr}"
        );
    }

    #[test]
    fn extern_c_nested_function_still_indents() {
        // fn_brace_newline=true puts function { on its own line.
        let src = "extern \"C\" {\nvoid foo(void) {\nint x = 1;\n}\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("\nvoid foo(void)\n"),
            "function declaration in extern \"C\" should not be indented, got:\n{out}"
        );
        assert!(
            out.contains("\n    int x = 1;"),
            "body of function inside extern \"C\" should be indented one level, got:\n{out}"
        );
    }

    #[test]
    fn param_continuation_alignment() {
        let src = "void foo(int a,\nint b,\nint c) {}\n";
        let out = fmt(src);
        assert!(
            out.contains("void foo(int a,\n         int b,\n         int c)"),
            "continuation params should align to opening paren column, got:\n{out}"
        );
    }

    #[test]
    fn space_inside_parens_preserve_no_space_in_source() {
        // Default Preserve: source has no spaces inside parens → output has none.
        let src = "void f(int x) { int z = (x + 1); if (z > 0) foo(z); }\n";
        let out = fmt(src);
        assert!(out.contains("f(int x)"), "decl parens: {out}");
        assert!(out.contains("(x + 1)"), "expr parens: {out}");
        assert!(out.contains("if (z > 0)"), "keyword parens: {out}");
        assert!(out.contains("foo(z)"), "call parens: {out}");
    }

    #[test]
    fn space_inside_parens_preserve_with_space_in_source() {
        // Preserve: source has spaces inside parens → output keeps them.
        let src = "void f(void) { assert( x > 0 ); testcase( x ); }\n";
        let out = fmt(src);
        assert!(
            out.contains("assert( x > 0 )"),
            "preserve should keep spaces inside assert(): {out}"
        );
        assert!(
            out.contains("testcase( x )"),
            "preserve should keep spaces inside testcase(): {out}"
        );
    }

    #[test]
    fn space_inside_parens_true() {
        let mut cfg = Config::default();
        cfg.spacing.space_inside_parens = SpaceOption::Add;
        let src = "void f(int x) { int z = (x + 1); if (z > 0) foo(z); }\n";
        let out = fmt_with(src, &cfg);
        assert!(out.contains("f( int x )"), "decl parens: {out}");
        assert!(out.contains("( x + 1 )"), "expr parens: {out}");
        assert!(out.contains("if ( z > 0 )"), "keyword parens: {out}");
        assert!(out.contains("foo( z )"), "call parens: {out}");
    }

    #[test]
    fn space_inside_parens_continuation_alignment() {
        // When space_inside_parens is enabled the leading space shifts the first
        // argument by one column; continuation lines must align with that shifted
        // column so all arguments line up.
        use crate::config::SpacingConfig;
        let src = "void f() { foo(arg1,\narg2,\narg3); }\n";
        let cfg = Config {
            spacing: SpacingConfig {
                space_inside_parens: SpaceOption::Add,
                ..SpacingConfig::default()
            },
            ..Config::default()
        };
        let out = fmt_with(src, &cfg);
        // `    foo( ` = 9 chars → arg2 aligns at col 9 (9 spaces)
        assert!(
            out.contains("foo( arg1,\n         arg2,\n         arg3 )"),
            "with space_inside_parens, continuation must align to first-arg column, got:\n{out}"
        );
    }

    #[test]
    fn call_continuation_alignment() {
        let src = "void f() { result = some_fn(arg1,\narg2,\narg3); }\n";
        let out = fmt(src);
        // `    result = some_fn(` = 21 chars, so continuation aligns at col 21
        assert!(
            out.contains("some_fn(arg1,\n                     arg2,\n                     arg3)"),
            "continuation call args should align to opening paren, got:\n{out}"
        );
    }

    #[test]
    fn nested_paren_continuation_alignment() {
        let src = "void f() { foo(bar(x,\ny), z); }\n";
        let out = fmt(src);
        // `    foo(bar(` = 12 chars, so inner continuation aligns at col 12
        assert!(
            out.contains("bar(x,\n            y)"),
            "nested paren continuation should align to inner opening paren, got:\n{out}"
        );
    }

    #[test]
    fn paren_eol_continuation_uses_regular_indent() {
        // When `(` is the last token on a line, continuation lines must use
        // a normal block indent rather than aligning to the (deep) paren column.
        let src = "result = some_function(\narg1,\narg2,\narg3);\n";
        let out = fmt(src);
        assert!(
            out.contains("some_function(\n    arg1,\n    arg2,\n    arg3)"),
            "paren-at-eol continuation must use regular indent, got:\n{out}"
        );
    }

    #[test]
    fn paren_eol_continuation_consistent_across_lines() {
        // All continuation lines in an eol-paren must share the same indent,
        // not just the first one.
        let src = "void f() { foo(\narg1,\narg2,\narg3); }\n";
        let out = fmt(src);
        let cont_lines: Vec<&str> = out.lines().filter(|l| l.contains("arg")).collect();
        assert_eq!(cont_lines.len(), 3, "expected 3 arg lines, got:\n{out}");
        let indents: Vec<usize> = cont_lines
            .iter()
            .map(|l| l.len() - l.trim_start().len())
            .collect();
        assert!(
            indents.iter().all(|&i| i == indents[0]),
            "all continuation lines must have same indent, got:\n{out}"
        );
    }

    #[test]
    fn paren_eol_continuation_inside_nested_call_with_assign() {
        // `method = eap_server_get_type(` — EOL paren on an assign-continuation
        // line (visual indent level 4 = 16 spaces).  Continuation args must
        // be one indent deeper (20 spaces), not at the scope level (12-16).
        // Mirrors the hostap pattern: indent_level=3 but visual indent=16.
        let src = "void f() { void g() { void h() { x =\nget_type(\narg1,\narg2); } } }\n";
        let out = fmt(src);
        let arg_lines: Vec<&str> = out.lines().filter(|l| l.contains("arg")).collect();
        assert_eq!(arg_lines.len(), 2, "expected 2 arg lines, got:\n{out}");
        // get_type( is at indent+1 = 16 spaces; args must be at 16+4 = 20.
        let indent = arg_lines[0].len() - arg_lines[0].trim_start().len();
        assert_eq!(indent, 20, "args after assign-continuation EOL paren must be at 20 spaces, got:\n{out}");
    }

    #[test]
    fn paren_eol_continuation_inner_paren_inside_if() {
        // `if (hostapd_config_read_radius_addr(` — two parens opened on the
        // same line (the `if (` and the call `(`).  The EOL paren is the
        // inner call's `(`.  Continuation must be at
        //   line_indent (8) + 2 parens * 4 = 16 spaces.
        let src = "void f() { void g() { if (some_long_call(\narg1,\narg2)) {} } }\n";
        let out = fmt(src);
        let arg_lines: Vec<&str> = out.lines().filter(|l| l.contains("arg")).collect();
        assert_eq!(arg_lines.len(), 2, "expected 2 arg lines, got:\n{out}");
        // if ( is at level 2 = 8 spaces; 2 parens opened → 8 + 2*4 = 16.
        let indent = arg_lines[0].len() - arg_lines[0].trim_start().len();
        assert_eq!(indent, 16, "args inside if(call( EOL paren must be at 16 spaces, got:\n{out}");
    }

    #[test]
    fn assign_continuation_aligns_to_rhs_column() {
        // `    int result = ` = 16 chars, so continuation aligns at col 16
        let src = "void f() { int result = value1 +\nvalue2 +\nvalue3; }\n";
        let out = fmt(src);
        assert!(
            out.contains("= value1 +\n                 value2 +\n                 value3"),
            "continuation after = should align to RHS column, got:\n{out}"
        );
    }

    #[test]
    fn assign_eol_continuation_uses_deeper_indent() {
        // When `=` is the last token on its line, continuation uses indent+1.
        let src = "void f() { int x =\nvalue; }\n";
        let out = fmt(src);
        assert!(
            out.contains("int x =\n        value"),
            "= at EOL continuation should use indent+1, got:\n{out}"
        );
    }

    #[test]
    fn assign_continuation_cleared_by_brace() {
        // Initializer brace `{` should not be aligned to the = column.
        let src = "int a[3] = {\n1,\n2,\n3\n};\n";
        let out = fmt(src);
        assert!(
            out.contains("= {\n    1,\n    2,\n    3\n}"),
            "initializer elements must use brace indent, not = column, got:\n{out}"
        );
    }

    #[test]
    fn space_after_semi_before_plusplus_in_for() {
        let out = fmt("void f() { for (i = 0; i < 10;++i) {} }\n");
        assert!(
            !out.contains(";++i") && out.contains("; ++i"),
            "semicolon before ++ must have a space: {out}"
        );
    }

    #[test]
    fn space_between_consecutive_semis_in_for() {
        // for (i = 0;; i++) — no space between consecutive ;;, space after (matching uncrustify)
        let out = fmt("void f() { for (i = 0;; i++) {} }\n");
        assert!(
            out.contains("for (i = 0;; i++)"),
            "for-loop with empty condition must not get space between ;; : {out}"
        );
        // for (;;) — infinite loop: no space (uncrustify normalizes to no-space)
        let out2 = fmt("void f() { for (;;) {} }\n");
        assert!(
            out2.contains("for (;;)"),
            "infinite-loop for(;;) must not get spaces: {out2}"
        );
        // source already had spaces: for (; ;) — normalize to for (;;)
        let out3 = fmt("void f() { for (; ;) {} }\n");
        assert!(
            out3.contains("for (;;)"),
            "for(; ;) with spaces should normalize to for(;;): {out3}"
        );
    }

    #[test]
    fn func_ptr_typedef_space_before_paren() {
        let out = fmt("typedef void(*Fn)(int);\n");
        assert!(
            out.contains("void (*Fn)"),
            "function pointer typedef needs space before (*: {out}"
        );
    }

    #[test]
    fn func_ptr_typedef_ref_space_before_paren() {
        let out = fmt("typedef void(&Ref)(int);\n");
        assert!(
            out.contains("void (&Ref)"),
            "function reference typedef needs space before (&: {out}"
        );
    }

    #[test]
    fn func_ptr_call_no_extra_space() {
        let out = fmt("void f() { bar(x); }\n");
        assert!(
            out.contains("bar(x)"),
            "normal call must not gain extra space: {out}"
        );
    }

    #[test]
    fn space_after_semi_before_minusminus_in_for() {
        let out = fmt("void f() { for (i = 10; i > 0;--i) {} }\n");
        assert!(
            !out.contains(";--i") && out.contains("; --i"),
            "semicolon before -- must have a space: {out}"
        );
    }

    #[test]
    fn space_after_semi_before_unary_star_in_for() {
        // `for (pos = cmd, len = 0;*pos != '\0'; pos++)` — unary * after ;
        let out = fmt(
            "void f(char *cmd) { char *pos; for (pos = cmd, len = 0;*pos != '\\0'; pos++) {} }\n",
        );
        assert!(
            !out.contains(";*pos") && out.contains("; *pos"),
            "semicolon before unary * in for-loop must have a space: {out}"
        );
    }

    #[test]
    fn space_after_semi_before_unary_amp_in_for() {
        let out = fmt("void f() { for (int i = 0;&x < end; i++) {} }\n");
        assert!(
            !out.contains(";&x") && out.contains("; &x"),
            "semicolon before unary & in for-loop must have a space: {out}"
        );
    }

    #[test]
    fn block_comment_closing_preserve_by_default() {
        // Default config: bare `*/` is preserved as-is (uncrustify parity).
        let out = fmt("/*\n * foo\n*/\nvoid f() {}\n");
        assert!(
            out.contains("\n*/"),
            "default config must preserve flush closing */, got:\n{out}"
        );
    }

    #[test]
    fn block_comment_closing_gets_space_when_opt_in() {
        use crate::config::CommentConfig;
        let cfg = Config {
            comments: CommentConfig {
                normalize_block_comment_closing: true,
            },
            ..Config::default()
        };
        let out = fmt_with("/*\n * foo\n*/\nvoid f() {}\n", &cfg);
        assert!(
            out.contains(" */"),
            "normalize_block_comment_closing=true: closing */ must have a leading space, got:\n{out}"
        );
        assert!(
            !out.contains("\n*/"),
            "normalize_block_comment_closing=true: closing */ must not be flush at line start, got:\n{out}"
        );
    }

    #[test]
    fn binary_op_after_inline_block_comment_gets_space() {
        let out = fmt("int x = 2 /* comment */ + 3;\n");
        assert!(
            out.contains("/* comment */ +"),
            "binary op after inline block comment must have a leading space, got:\n{out}"
        );
    }

    #[test]
    fn binary_amp_after_arrow_not_pointer_decl() {
        // `p->flags & MASK` — the `&` follows a member-access chain and must be
        // treated as bitwise AND, not a pointer declarator.
        let out = fmt("void f(void) { if (p->db->flags & MASK) {} }\n");
        assert!(
            out.contains("flags & MASK"),
            "& after -> must be binary AND with space on both sides, got:\n{out}"
        );
    }

    #[test]
    fn binary_amp_after_dot_not_pointer_decl() {
        let out = fmt("void f(void) { if (s.field & MASK) {} }\n");
        assert!(
            out.contains("field & MASK"),
            "& after . must be binary AND with space on both sides, got:\n{out}"
        );
    }

    #[test]
    fn binary_amp_in_if_condition_no_space_in_source() {
        // `if (auth_type &WPS_AUTH_WPA2PSK)` — binary & inside if() with no space in source.
        // Previously misclassified as pointer declarator due to KwIf not matching Keyword.
        let out = fmt("void f(int auth_type) { if (auth_type &WPS_AUTH_WPA2PSK) {} }\n");
        assert!(
            out.contains("auth_type & WPS_AUTH_WPA2PSK"),
            "& in if() must be binary AND with space on both sides, got:\n{out}"
        );
    }

    #[test]
    fn binary_pipe_in_if_condition_no_space_in_source() {
        let out = fmt("void f(int x) { if (x |MASK) {} }\n");
        assert!(
            out.contains("x | MASK"),
            "| in if() must be binary OR with space on both sides, got:\n{out}"
        );
    }

    #[test]
    fn binary_amp_in_while_condition() {
        let out = fmt("void f(int flags) { while (flags &MASK) {} }\n");
        assert!(
            out.contains("flags & MASK"),
            "& in while() must be binary AND, got:\n{out}"
        );
    }

    #[test]
    fn block_comment_double_star_style_closing_no_space() {
        // SQLite-style: `/*\n** text\n*/` — closing `*/` must NOT get a leading space
        // even when normalize_block_comment_closing is enabled.
        use crate::config::CommentConfig;
        let cfg = Config {
            comments: CommentConfig {
                normalize_block_comment_closing: true,
            },
            ..Config::default()
        };
        let src = "/*\n** SQLite style\n** continuation\n*/\nvoid f() {}\n";
        let out = fmt_with(src, &cfg);
        assert!(
            out.contains("\n*/"),
            "double-star style closing */ must stay flush at col 0, got:\n{out}"
        );
        assert!(
            !out.contains("\n */"),
            "double-star style must not gain a spurious leading space, got:\n{out}"
        );
    }

    #[test]
    fn block_comment_closing_already_spaced_unchanged() {
        use crate::config::CommentConfig;
        let cfg = Config {
            comments: CommentConfig {
                normalize_block_comment_closing: true,
            },
            ..Config::default()
        };
        let src = "/*\n * foo\n */\nvoid f() {}\n";
        let out = fmt_with(src, &cfg);
        assert!(
            out.contains(" */") && !out.contains("  */"),
            "already-correct */ must not be double-spaced, got:\n{out}"
        );
    }

    #[test]
    fn block_comment_trailing_spaces_stripped() {
        // Source has trailing spaces on the continuation and closing lines.
        let src = "/*   \n * line with trailing   \n */   \nvoid f() {}\n";
        let out = fmt(src);
        assert!(
            !out.lines().any(|l| l.ends_with(' ')),
            "trailing spaces must be stripped from all comment lines, got:\n{out}"
        );
    }

    #[test]
    fn block_comment_tab_continuation_expanded_to_spaces() {
        // Source: two-tab indent before ` * text` — must become spaces.
        let src = "\t\t/*\n\t\t * stdout is forbidden\n\t\t */\nint x;\n";
        let out = fmt(src);
        // Each leading \t should be expanded to indent_width (4) spaces.
        assert!(
            out.contains("         * stdout"),
            "tabs in continuation line must be expanded to spaces:\n{out}"
        );
        assert!(
            !out.contains('\t'),
            "no tabs must remain in the output:\n{out}"
        );
    }

    #[test]
    fn block_comment_tab_continuation_tabs_style_preserved() {
        // When indent.style = tabs, leading tabs in comment bodies must stay as tabs.
        use crate::config::IndentConfig;
        let cfg = Config {
            indent: IndentConfig {
                style: IndentStyle::Tabs,
                width: 4,
                indent_switch_case: true,
                indent_goto_labels: false,
            },
            ..Config::default()
        };
        let src = "void f() {\n\t/*\n\t * note\n\t */\n}\n";
        let out = fmt_with(src, &cfg);
        assert!(
            out.contains("\t * note"),
            "tabs in continuation line must be preserved under tabs style:\n{out}"
        );
    }

    #[test]
    fn block_comment_mixed_leading_whitespace_expanded() {
        // Mixed: spaces already present (no tab at very start of continuation).
        let src = "void f() {\n    /*\n     * already spaces\n     */\n}\n";
        let out = fmt(src);
        // Already spaces — must remain unchanged (no extra expansion).
        assert!(
            out.contains("     * already spaces"),
            "space-indented continuation must be left alone:\n{out}"
        );
    }

    fn cfg_align(span: usize) -> Config {
        Config {
            spacing: SpacingConfig {
                align_right_cmt_span: span,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn align_trailing_comments_struct() {
        let src =
            "struct Foo { int x; // short\nlong long_field; // longer\nchar c; // another\n};\n";
        let out = fmt_with(src, &cfg_align(2));
        // All three `//` should start at the same column.
        let positions: Vec<usize> = out.lines().filter_map(trailing_comment_col).collect();
        assert_eq!(
            positions.len(),
            3,
            "expected 3 inline comments, got:\n{out}"
        );
        assert!(
            positions.iter().all(|&c| c == positions[0]),
            "trailing comments not aligned: columns={positions:?}\n{out}"
        );
    }

    #[test]
    fn align_trailing_comments_block_comment_style() {
        // /* */ inline block comments should be aligned the same as // comments.
        let src = "int a; /* short */\nlong long_field; /* longer */\n";
        let out = fmt_with(src, &cfg_align(2));
        let positions: Vec<usize> = out.lines().filter_map(trailing_comment_col).collect();
        assert_eq!(
            positions.len(),
            2,
            "expected 2 inline /* */ comments:\n{out}"
        );
        assert_eq!(
            positions[0], positions[1],
            "/* */ trailing comments must be column-aligned:\n{out}"
        );
    }

    #[test]
    fn align_trailing_comments_doxygen_not_treated_as_regular() {
        // /**< Doxygen member comments must not be picked up by trailing_comment_col
        // (they have their own alignment pass).
        let src = "int x; /**< x */\nlong yy; /**< yy */\n";
        let positions: Vec<usize> = src.lines().filter_map(trailing_comment_col).collect();
        assert!(
            positions.is_empty(),
            "/**< must not be detected as a regular trailing comment:\n{src}"
        );
    }

    #[test]
    fn align_trailing_comments_off_when_span_zero() {
        let src = "struct Foo { int x; // a\nlong long_field; // bb\n};\n";
        let out = fmt_with(src, &cfg_align(0));
        let positions: Vec<usize> = out.lines().filter_map(trailing_comment_col).collect();
        assert_eq!(
            positions.len(),
            2,
            "expected 2 inline comments, got:\n{out}"
        );
        assert_ne!(
            positions[0], positions[1],
            "comments should NOT be aligned when span=0:\n{out}"
        );
    }

    #[test]
    fn align_trailing_comments_single_line_groups_default() {
        // Default style (Groups): a lone trailing comment keeps 1 space — not normalized.
        let src = "uint8_t buf[] = {\n    0x00, /* bad id */\n    0x01, 0x02\n};\n";
        let out = fmt_with(src, &cfg_align(2));
        let line = out.lines().find(|l| l.contains("/* bad id */")).unwrap();
        // With Groups default the comment should be flush after the comma with 1 space.
        assert!(
            line.contains(", /* bad id */"),
            "single trailing comment should keep 1 space in Groups mode:\n{out}"
        );
    }

    #[test]
    fn align_trailing_comments_single_line_all_style() {
        // AlignCmtStyle::All: single trailing comments are padded to min_gap.
        use crate::config::{AlignCmtStyle, SpacingConfig};
        let cfg = Config {
            spacing: SpacingConfig {
                align_right_cmt_span: 1,
                align_right_cmt_gap: 3,
                align_right_cmt_style: AlignCmtStyle::All,
                ..Default::default()
            },
            ..Config::default()
        };
        let src = "uint8_t buf[] = {\n    0x00, /* bad id */\n    0x01, 0x02\n};\n";
        let out = fmt_with(src, &cfg);
        let col = out
            .lines()
            .find(|l| l.contains("/* bad id */"))
            .and_then(trailing_comment_col)
            .expect("comment not found");
        let code_len = out
            .lines()
            .find(|l| l.contains("/* bad id */"))
            .map(|l| l[..col].trim_end().len())
            .unwrap();
        assert!(
            col >= code_len + 3,
            "single trailing comment must be padded to min_gap=3 in All mode, got col={col}:\n{out}"
        );
    }

    #[test]
    fn align_trailing_comments_blank_line_breaks_group() {
        // span=2: only consecutive lines group; blank line breaks the group.
        let src = "int a; // g1\nint b; // g1\n\nint c; // g2\n";
        let out = fmt_with(src, &cfg_align(2));
        let cols: Vec<usize> = out.lines().filter_map(trailing_comment_col).collect();
        assert_eq!(cols.len(), 3, "expected 3 inline comments, got:\n{out}");
        // First two should be aligned (same group); third may differ.
        assert_eq!(cols[0], cols[1], "group-1 comments not aligned:\n{out}");
    }

    #[test]
    fn align_trailing_comments_span_bridges_gap_line() {
        // span=3 (matches uncrustify default): a single non-commented line
        // between two commented lines is allowed within the same group.
        // Distance between ptk and tptk = 2 (one gap line: ptk_set).
        // 2 < span=3 → same group.
        let src = "    struct wpa_ptk ptk; /* Derived PTK */\n    int ptk_set;\n    struct wpa_ptk tptk; /* Derived PTK during rekeying */\n";
        let out = fmt_with(src, &Config::default());
        let positions: Vec<usize> = out.lines().filter_map(trailing_comment_col).collect();
        assert_eq!(positions.len(), 2, "expected 2 trailing comments:\n{out}");
        assert_eq!(
            positions[0], positions[1],
            "ptk and tptk comments must be column-aligned (span=3 bridges 1 gap line):\n{out}"
        );
    }

    #[test]
    fn align_trailing_comments_span_does_not_bridge_two_gap_lines() {
        // span=3: two non-commented lines between commented lines means
        // distance=3, which is NOT < 3 → separate groups.
        let src = "    struct ap_info *hnext; /* next entry in hash table list */\n    u8 addr[6];\n    u8 supported_rates[256];\n    int erp; /* ERP Info */\n";
        let out = fmt_with(src, &Config::default());
        let positions: Vec<usize> = out.lines().filter_map(trailing_comment_col).collect();
        assert_eq!(positions.len(), 2, "expected 2 trailing comments:\n{out}");
        // erp is isolated (single-line group), hnext is also single-line here.
        // They must NOT be at the same column (Groups style, no normalization).
        assert_ne!(
            positions[0], positions[1],
            "hnext and erp must NOT be co-aligned with span=3 (2-gap-line distance):\n{out}"
        );
    }

    fn cfg_enum_align(span: usize) -> Config {
        Config {
            spacing: SpacingConfig {
                align_enum_equ_span: span,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn align_enum_equals_basic() {
        let src = "enum Color { RED = 0,\nGREEN = 1,\nBLUE_DARK = 2,\nWHITE = 3,\n};\n";
        let out = fmt_with(src, &cfg_enum_align(1));
        let positions: Vec<usize> = out.lines().filter_map(enum_eq_col).collect();
        assert_eq!(
            positions.len(),
            4,
            "expected 4 enum value lines, got:\n{out}"
        );
        assert!(
            positions.iter().all(|&c| c == positions[0]),
            "enum = signs not aligned: columns={positions:?}\n{out}"
        );
    }

    #[test]
    fn align_enum_equals_off_when_span_zero() {
        let src = "enum E { A = 1,\nLONG_NAME = 2,\n};\n";
        let out = fmt_with(src, &cfg_enum_align(0));
        let positions: Vec<usize> = out.lines().filter_map(enum_eq_col).collect();
        assert_eq!(
            positions.len(),
            2,
            "expected 2 enum value lines, got:\n{out}"
        );
        assert_ne!(
            positions[0], positions[1],
            "enum = should NOT be aligned when span=0:\n{out}"
        );
    }

    #[test]
    fn align_enum_equals_blank_line_transparent() {
        // Blank lines inside an enum are transparent: all four values should align
        // together even though they are separated by a blank line.
        let src = "enum E { A = 1,\nLONG_NAME = 2,\n\nC = 10,\nD = 11,\n};\n";
        let out = fmt_with(src, &cfg_enum_align(1));
        let positions: Vec<usize> = out.lines().filter_map(enum_eq_col).collect();
        assert_eq!(
            positions.len(),
            4,
            "expected 4 enum value lines, got:\n{out}"
        );
        // All four are in one group (blank line is transparent) and must be aligned.
        assert!(
            positions.windows(2).all(|w| w[0] == w[1]),
            "= signs not all aligned across blank line: {positions:?}\n{out}"
        );
    }

    #[test]
    fn align_enum_equals_isolated_values_aligned() {
        // Each value on its own paragraph (blank line between every pair) — all should align.
        let src =
            "typedef enum {\nSUCCESS = 0,\n\nVERY_LONG_FAILURE_CODE = 1,\n\nTIMEOUT = 2,\n} R;\n";
        let out = fmt_with(src, &cfg_enum_align(1));
        let positions: Vec<usize> = out.lines().filter_map(enum_eq_col).collect();
        assert_eq!(positions.len(), 3, "expected 3 value lines:\n{out}");
        assert!(
            positions.windows(2).all(|w| w[0] == w[1]),
            "= signs not aligned: {positions:?}\n{out}"
        );
    }

    #[test]
    fn align_enum_equals_does_not_touch_function_assignments() {
        let src = "void f() { int x = 5;\nint longer_var = 6;\n}\n";
        let out_default = fmt(src);
        let out_aligned = fmt_with(src, &cfg_enum_align(1));
        assert_eq!(
            out_default, out_aligned,
            "function assignments should not be aligned:\n{out_aligned}"
        );
    }

    #[test]
    fn align_enum_equals_bare_members_transparent() {
        // Bare members (no explicit value) must not break the alignment group.
        let src = "enum E { RED,\nGREEN = 5,\nBLUE,\nYELLOW = 10,\nWHITE,\n};\n";
        let out = fmt_with(src, &cfg_enum_align(1));
        let positions: Vec<usize> = out.lines().filter_map(enum_eq_col).collect();
        assert_eq!(
            positions.len(),
            2,
            "expected 2 assigned members, got:\n{out}"
        );
        assert_eq!(
            positions[0], positions[1],
            "= signs not aligned across bare members:\n{out}"
        );
    }

    #[test]
    fn align_enum_equals_last_member_no_comma() {
        // The last enum member has no trailing comma — it must still be included
        // in the alignment group.
        let src = "enum E {\n    ACCEPT_UNLESS_DENIED = 0,\n    DENY_UNLESS_ACCEPTED = 1,\n    USE_EXTERNAL_RADIUS_AUTH = 2\n};\n";
        let out = fmt_with(src, &cfg_enum_align(1));
        let positions: Vec<usize> = out.lines().filter_map(enum_eq_col).collect();
        assert_eq!(positions.len(), 3, "all 3 members must be detected:\n{out}");
        assert!(
            positions.windows(2).all(|w| w[0] == w[1]),
            "= signs not aligned for last-member-no-comma:\n{out}"
        );
    }

    #[test]
    fn align_enum_equals_ifdef_transparent() {
        // Preprocessor directives inside an enum must not break the alignment group.
        let src = "enum E {\n    SECURITY_PLAINTEXT = 0,\n#ifdef CONFIG_WEP\n    SECURITY_STATIC_WEP = 1,\n#endif\n    SECURITY_WPA_PSK = 3,\n};\n";
        let out = fmt_with(src, &cfg_enum_align(1));
        let positions: Vec<usize> = out.lines().filter_map(enum_eq_col).collect();
        assert_eq!(positions.len(), 3, "3 assigned members expected:\n{out}");
        assert!(
            positions.windows(2).all(|w| w[0] == w[1]),
            "= signs not aligned across #ifdef block:\n{out}"
        );
    }

    #[test]
    fn align_enum_equals_on_tabstop() {
        use crate::config::SpacingConfig;
        let cfg = Config {
            spacing: SpacingConfig {
                align_enum_equ_span: 1,
                align_on_tabstop: true,
                ..Default::default()
            },
            ..Default::default()
        };
        // Names are 3 and 9 chars; max+1 = 10; next 4-multiple = 12.
        let src = "enum E {\n    FOO = 0,\n    LONGER_NAME = 1,\n};\n";
        let out = fmt_with(src, &cfg);
        let positions: Vec<usize> = out.lines().filter_map(enum_eq_col).collect();
        assert_eq!(positions.len(), 2, "2 members expected:\n{out}");
        assert!(
            positions.windows(2).all(|w| w[0] == w[1]),
            "= signs not aligned with tabstop:\n{out}"
        );
        // Column must be a multiple of indent_width (4)
        let col = positions[0];
        assert_eq!(
            col % 4,
            0,
            "tabstop alignment: column {col} must be a multiple of 4:\n{out}"
        );
    }

    #[test]
    fn align_trailing_comments_on_tabstop() {
        use crate::config::SpacingConfig;
        let cfg = Config {
            spacing: SpacingConfig {
                align_right_cmt_span: 3,
                align_right_cmt_gap: 1,
                align_on_tabstop: true,
                ..Default::default()
            },
            ..Default::default()
        };
        // Code lengths 22 and 26; max+gap = 27; next 4-multiple = 28.
        let src = "    int dot11MeshRetryTimeout; /* msec */\n    int dot11MeshConfirmTimeout; /* msec */\n    int dot11MeshHoldingTimeout; /* msec */\n";
        let out = fmt_with(src, &cfg);
        let positions: Vec<usize> = out.lines().filter_map(trailing_comment_col).collect();
        assert_eq!(positions.len(), 3, "3 trailing comments expected:\n{out}");
        assert!(
            positions.windows(2).all(|w| w[0] == w[1]),
            "trailing comments not aligned:\n{out}"
        );
        let col = positions[0];
        assert_eq!(
            col % 4,
            0,
            "tabstop: column {col} must be a multiple of 4:\n{out}"
        );
    }

    fn cfg_doxygen_align(span: usize) -> Config {
        Config {
            spacing: SpacingConfig {
                align_doxygen_cmt_span: span,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn align_doxygen_comments_basic() {
        let src = "typedef struct { int x; /**< x coord */\nconst char *name; /**< name */\n} S;\n";
        let out = fmt_with(src, &cfg_doxygen_align(1));
        let positions: Vec<usize> = out.lines().filter_map(trailing_doxygen_col).collect();
        assert_eq!(
            positions.len(),
            2,
            "expected 2 doxygen comments, got:\n{out}"
        );
        assert!(
            positions.iter().all(|&c| c == positions[0]),
            "/**< comments not aligned: columns={positions:?}\n{out}"
        );
    }

    #[test]
    fn align_doxygen_comments_off_when_span_zero() {
        let src = "typedef struct { int x; /**< x */\nconst char *name; /**< name */\n} S;\n";
        let out = fmt_with(src, &cfg_doxygen_align(0));
        let positions: Vec<usize> = out.lines().filter_map(trailing_doxygen_col).collect();
        assert_eq!(
            positions.len(),
            2,
            "expected 2 doxygen comments, got:\n{out}"
        );
        assert_ne!(
            positions[0], positions[1],
            "/**< should NOT be aligned when span=0:\n{out}"
        );
    }

    #[test]
    fn align_doxygen_comments_standalone_not_treated_as_inline() {
        // A /**< comment on its own line must not anchor a group.
        let src =
            "typedef struct {\n/**< standalone */\nint x; /**< inline */\nint yy; /**< inline2 */\n} S;\n";
        let out = fmt_with(src, &cfg_doxygen_align(1));
        let inline_positions: Vec<usize> = out.lines().filter_map(trailing_doxygen_col).collect();
        // Only the two inline comments should appear; standalone returns None.
        assert_eq!(
            inline_positions.len(),
            2,
            "expected 2 inline doxygen comments, got:\n{out}"
        );
        assert_eq!(
            inline_positions[0], inline_positions[1],
            "two inline /**< comments must be aligned:\n{out}"
        );
    }

    #[test]
    fn align_doxygen_comments_transparent_member_bridges_group() {
        // A comment-less member between two /**< members must not break the group.
        let src = "typedef struct {\n\
            int a; /**< first */\n\
            int b; /**< second */\n\
            int no_comment;\n\
            int c; /**< third */\n\
            } S;\n";
        let out = fmt_with(src, &cfg_doxygen_align(1));
        let positions: Vec<usize> = out.lines().filter_map(trailing_doxygen_col).collect();
        assert_eq!(positions.len(), 3, "expected 3 doxygen comments:\n{out}");
        assert!(
            positions[0] == positions[1] && positions[1] == positions[2],
            "all three /**< comments must align to same column:\n{out}"
        );
    }

    fn fmt_with_open_brace_blank(src: &str) -> String {
        let mut config = Config::default();
        config.newlines.blank_line_after_open_brace = true;
        fmt_with(src, &config)
    }

    #[test]
    fn blank_line_after_open_brace_function() {
        let src = "void f() {\n    return;\n}\n";
        let out = fmt_with_open_brace_blank(src);
        let lines: Vec<&str> = out.lines().collect();
        // Line after `{` must be blank, then `return`
        let brace_line = lines.iter().position(|l| l.ends_with('{')).unwrap();
        assert_eq!(
            lines[brace_line + 1],
            "",
            "expected blank line after open brace:\n{out}"
        );
        assert!(
            lines[brace_line + 2].contains("return"),
            "return not after blank:\n{out}"
        );
    }

    #[test]
    fn blank_line_after_open_brace_block() {
        let src = "void f() {\n    if (x) {\n        foo();\n    }\n}\n";
        let out = fmt_with_open_brace_blank(src);
        // Both the function `{` and the if-block `{` get a blank line after them.
        let brace_lines: Vec<usize> = out
            .lines()
            .enumerate()
            .filter(|(_, l)| l.ends_with('{'))
            .map(|(i, _)| i)
            .collect();
        for idx in &brace_lines {
            let next = out.lines().nth(idx + 1).unwrap_or("");
            assert_eq!(next, "", "expected blank after {{ at line {idx}:\n{out}");
        }
    }

    #[test]
    fn blank_line_after_open_brace_not_struct() {
        // Struct bodies must NOT get a blank line after `{`.
        let src = "struct S {\n    int x;\n};\n";
        let out = fmt_with_open_brace_blank(src);
        let lines: Vec<&str> = out.lines().collect();
        let brace_line = lines.iter().position(|l| l.ends_with('{')).unwrap();
        assert_ne!(
            lines[brace_line + 1],
            "",
            "struct body must not get blank after open brace:\n{out}"
        );
    }

    #[test]
    fn blank_line_after_open_brace_disabled_by_default() {
        let src = "void f() {\n    return;\n}\n";
        let out = fmt(src);
        let lines: Vec<&str> = out.lines().collect();
        let brace_line = lines.iter().position(|l| l.ends_with('{')).unwrap();
        assert_ne!(
            lines[brace_line + 1],
            "",
            "blank after open brace must be off by default:\n{out}"
        );
    }

    fn fmt_merge_comment(src: &str) -> String {
        let mut config = Config::default();
        config.newlines.merge_line_comment = true;
        fmt_with(src, &config)
    }

    #[test]
    fn merge_line_comment_after_open_brace() {
        let src = "void f() {\n// body comment\n    return;\n}\n";
        let out = fmt_merge_comment(src);
        let brace_line = out
            .lines()
            .find(|l| l.contains('{'))
            .expect("no brace line");
        assert!(
            brace_line.contains("// body comment"),
            "comment not merged onto open brace line:\n{out}"
        );
        // Indented code follows on next line
        assert!(
            out.lines().any(|l| l.trim() == "return;"),
            "return missing:\n{out}"
        );
    }

    #[test]
    fn merge_line_comment_after_close_brace() {
        let src = "void f() {\n    return;\n}\n// after fn\nvoid g() {}\n";
        let out = fmt_merge_comment(src);
        let close_line = out
            .lines()
            .find(|l| l.trim_start() == "}" || l.trim_start().starts_with("} "))
            .expect("no close brace line");
        assert!(
            close_line.contains("// after fn"),
            "comment not merged onto close brace line:\n{out}"
        );
    }

    #[test]
    fn merge_line_comment_after_semi() {
        let src = "void f() {\n    int x = 1;\n// about x\n    foo(x);\n}\n";
        let out = fmt_merge_comment(src);
        let semi_line = out
            .lines()
            .find(|l| l.contains("int x"))
            .expect("no int x line");
        assert!(
            semi_line.contains("// about x"),
            "comment not merged onto statement line:\n{out}"
        );
    }

    #[test]
    fn merge_line_comment_blank_line_prevents_merge() {
        // A blank line between the brace and the comment must suppress merging.
        let src = "void f() {\n\n// not merged\n    return;\n}\n";
        let out = fmt_merge_comment(src);
        let brace_line = out
            .lines()
            .find(|l| l.ends_with('{'))
            .expect("no open brace line");
        assert!(
            !brace_line.contains("//"),
            "comment must not merge when blank line present:\n{out}"
        );
        assert!(
            out.lines().any(|l| l.trim() == "// not merged"),
            "standalone comment missing:\n{out}"
        );
    }

    #[test]
    fn blank_line_before_close_brace_preserved() {
        let src = "void foo(void) {\n    int x = 1;\n\n    x++;\n\n}\n";
        let out = fmt(src);
        let lines: Vec<&str> = out.lines().collect();
        let rbrace_idx = lines.iter().rposition(|l| l.trim() == "}").unwrap();
        assert_eq!(
            lines[rbrace_idx - 1],
            "",
            "blank line before closing brace must be preserved:\n{out}"
        );
    }

    #[test]
    fn merge_line_comment_disabled_by_default() {
        let src = "void f() {\n// comment\n    return;\n}\n";
        let out = fmt(src);
        let brace_line = out
            .lines()
            .find(|l| l.ends_with('{'))
            .expect("no open brace line");
        assert!(
            !brace_line.contains("//"),
            "merge_line_comment must be off by default:\n{out}"
        );
        assert!(
            out.lines().any(|l| l.trim() == "// comment"),
            "standalone comment must remain on its own line:\n{out}"
        );
    }

    #[test]
    fn pp_indent_disabled_by_default() {
        let src = "#if defined(FOO)\n#include <foo.h>\n#endif\n";
        let out = fmt(src);
        // Default: no indentation of preprocessor directives.
        assert!(
            out.lines().any(|l| l == "#include <foo.h>"),
            "pp_indent must be off by default:\n{out}"
        );
    }

    #[test]
    fn pp_indent_enabled() {
        use crate::config::PreprocConfig;
        let cfg = Config {
            preprocessor: PreprocConfig {
                pp_indent: true,
                ..PreprocConfig::default()
            },
            ..Config::default()
        };
        let src =
            "#if defined(FOO)\n#include <foo.h>\n#if defined(BAZ)\n#define QUX 2\n#endif\n#endif\n";
        let out = fmt_with(src, &cfg);
        assert!(
            out.lines().any(|l| l == "    #include <foo.h>"),
            "depth-1 directive must be indented 4 spaces:\n{out}"
        );
        assert!(
            out.lines().any(|l| l == "    #if defined(BAZ)"),
            "depth-1 #if must be indented 4 spaces:\n{out}"
        );
        assert!(
            out.lines().any(|l| l == "        #define QUX 2"),
            "depth-2 directive must be indented 8 spaces:\n{out}"
        );
        assert!(
            out.lines().any(|l| l == "    #endif"),
            "depth-1 #endif must be indented 4 spaces:\n{out}"
        );
    }

    #[test]
    fn pp_indent_elif_else_at_outer_level() {
        use crate::config::PreprocConfig;
        let cfg = Config {
            preprocessor: PreprocConfig {
                pp_indent: true,
                ..PreprocConfig::default()
            },
            ..Config::default()
        };
        let src = "#ifdef WIN32\n#define PLATFORM \"windows\"\n#elif defined(LINUX)\n#define PLATFORM \"linux\"\n#else\n#define PLATFORM \"unknown\"\n#endif\n";
        let out = fmt_with(src, &cfg);
        // #elif and #else must be at the #if level (depth 0).
        assert!(
            out.lines().any(|l| l == "#elif defined(LINUX)"),
            "#elif must be at depth 0:\n{out}"
        );
        assert!(
            out.lines().any(|l| l == "#else"),
            "#else must be at depth 0:\n{out}"
        );
        // Content between #elif and #else at depth 1.
        assert!(
            out.lines().any(|l| l == "    #define PLATFORM \"linux\""),
            "#define after #elif must be at depth 1:\n{out}"
        );
    }

    #[test]
    fn operator_overload_no_space_around_symbol() {
        let src = "class Foo {\n    Foo &operator=(const Foo &) = delete;\n    Foo &operator+=(const Foo &other);\n    bool operator==(const Foo &) const;\n    bool operator!=(const Foo &) const;\n};\n";
        let out = fmt(src);
        assert!(
            out.lines().any(|l| l.contains("operator=(const")),
            "operator= must have no space around =:\n{out}"
        );
        assert!(
            out.lines().any(|l| l.contains("operator+=(const")),
            "operator+= must have no space around +=:\n{out}"
        );
        assert!(
            out.lines().any(|l| l.contains("operator==(const")),
            "operator== must have no space around ==:\n{out}"
        );
        assert!(
            out.lines().any(|l| l.contains("operator!=(const")),
            "operator!= must have no space around !=:\n{out}"
        );
    }

    #[test]
    fn operator_overload_ref_aligned_to_name() {
        let src = "class Foo {\n    Foo &operator+=(const Foo &other);\n};\n";
        let out = fmt(src);
        assert!(
            out.lines().any(|l| l.contains("Foo &operator+=")),
            "& must be aligned to name in operator+= return type:\n{out}"
        );
    }

    #[test]
    fn operator_new_delete_space() {
        let src =
            "class Foo {\n    void *operator new(size_t);\n    void operator delete(void *);\n};\n";
        let out = fmt(src);
        assert!(
            out.lines().any(|l| l.contains("operator new(")),
            "operator new must have space before new:\n{out}"
        );
        assert!(
            out.lines().any(|l| l.contains("operator delete(")),
            "operator delete must have space before delete:\n{out}"
        );
    }

    #[test]
    fn operator_subscript_and_call() {
        let src = "class Foo {\n    int &operator[](int i);\n    int operator()(int x);\n};\n";
        let out = fmt(src);
        assert!(
            out.lines().any(|l| l.contains("operator[](int")),
            "operator[] must not have space before (:\n{out}"
        );
        assert!(
            out.lines().any(|l| l.contains("operator()(int")),
            "operator() must not have space before (:\n{out}"
        );
    }

    #[test]
    fn ternary_colon_has_space() {
        let src = "int f(int x) { return x == 0 ? 0 : 1; }\n";
        let out = fmt(src);
        assert!(
            out.contains("? 0 : 1"),
            "ternary : must have space before and after:\n{out}"
        );
    }

    #[test]
    fn nested_ternary_colon_has_space() {
        let src = "int f(int a, int b, int c) { return a ? b ? c : 0 : 1; }\n";
        let out = fmt(src);
        assert!(
            out.contains("? c : 0 : 1"),
            "nested ternary : must have spaces:\n{out}"
        );
    }

    #[test]
    fn binary_plus_after_function_call_not_unary() {
        // `+` after a closing `)` of a regular function call must be binary.
        // Regression: strlen(prefix)+1 was emitting "+strlen(prefix) +1".
        let src = "void f(void) {\n    int n = strlen(topic) + strlen(prefix) + 1;\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("strlen(prefix) + 1"),
            "binary + after func-call ) must have space on both sides: {out}"
        );
        assert!(
            out.contains("strlen(topic) + strlen(prefix)"),
            "binary + between two func-calls must have space on both sides: {out}"
        );
    }

    #[test]
    fn space_after_comma_not_eaten_by_sizeof_pointer() {
        // `*` inside sizeof(type *) set suppress_next_space which leaked through
        // `)` and ate the space after the following comma.
        let src = "void f(void) {\n    qsort(files, count, sizeof(char *), cmp);\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("sizeof(char *), cmp"),
            "space after comma must not be eaten by * inside sizeof(): {out}"
        );
    }

    #[test]
    fn space_before_varargs_ellipsis() {
        // `foo(int x, ...)` — space_after_comma must apply before `...`.
        let out = fmt("void foo(int x, ...);\n");
        assert!(
            out.contains(", ..."),
            "space after comma must appear before ... in varargs, got:\n{out}"
        );
    }

    #[test]
    fn space_after_comma_not_eaten_by_cast_pointer() {
        // Same leak pattern with an explicit cast `(unsigned char *)buf`.
        // Default space_after_cast=preserve so no-space source stays no-space.
        let src = "void f(void) {\n    fn(ctx, (unsigned char *)buf, (unsigned int)len);\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("(unsigned char *)buf, (unsigned int)len"),
            "space after comma following a cast must not be suppressed: {out}"
        );
    }

    #[test]
    fn multiplication_by_sizeof_not_pointer_decl() {
        // `count * sizeof(char *)` — the `*` is binary multiplication, not a
        // pointer declarator.  Regression: was emitting `count *sizeof(char *)`.
        let src = "void f(void) {\n    p = realloc(p, count * sizeof(char *));\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("count * sizeof(char *)"),
            "* before sizeof must be binary multiplication: {out}"
        );
    }

    #[test]
    fn no_trailing_space_after_closing_brace() {
        // `} while(...)` on separate lines: the space emitted for the cuddle check
        // must be stripped before the newline — no trailing whitespace.
        let src = "void f(void) {\n    if (x) {\n        y = 1;\n    }\n    while (x) {\n        x--;\n    }\n}\n";
        let out = fmt(src);
        for line in out.lines() {
            assert!(
                !line.ends_with(' '),
                "no line should have trailing spaces; got: {:?}\nfull output:\n{out}",
                line
            );
        }
    }

    #[test]
    fn ifdef_else_brace_depth_reset() {
        // When #ifdef and #else branches each open one `{`, the code after
        // #endif should be indented at depth 2 (one for the outer block, one for
        // the inner block opened in each branch), not depth 3 (incorrectly
        // accumulated from both branches).
        let src = concat!(
            "void f(void) {\n",
            "    if (a) {\n",
            "#ifdef WIN32\n",
            "        if (b) {\n",
            "#else\n",
            "        if (c) {\n",
            "#endif\n",
            "            x = 1;\n",
            "        }\n",
            "    }\n",
            "}\n"
        );
        let out = fmt(src);
        assert!(
            out.contains("            x = 1;"),
            "#ifdef/#else depth: code after #endif must be at depth 3 (12 spaces): {out}"
        );
    }

    #[test]
    fn else_brace_after_ifdef_block() {
        // A `{` following `else` with a #ifdef directive in between must be treated
        // as a Block brace (not an initializer), so K&R style keeps it on the same
        // line / Allman puts it on its own line rather than at column 0.
        let src = concat!(
            "if (a) {\n",
            "    x = 1;\n",
            "} else\n",
            "#ifdef FOO\n",
            "{\n",
            "    y = 2;\n",
            "}\n",
            "#endif\n",
        );
        let out = fmt(src);
        assert!(
            !out.contains("{\n    y = 2;") || out.contains("else\n{") || out.contains("else {"),
            "brace after else+#ifdef must not be treated as initializer: {out}"
        );
        // The body must be indented, not at column 0.
        assert!(
            out.contains("    y = 2;"),
            "body inside else+#ifdef brace must be indented: {out}"
        );
    }

    #[test]
    fn bitfield_colon_gets_space() {
        // Bitfield `field:N` → `field : N` (space before and after colon in struct context).
        let src = "struct S { unsigned int x:4; unsigned int y:8; };\n";
        let out = fmt(src);
        assert!(
            out.contains("x : 4") && out.contains("y : 8"),
            "bitfield colon must have spaces on both sides:\n{out}"
        );
    }

    #[test]
    fn bitfield_ternary_colon_unaffected() {
        // Ternary `:` inside a function must not be confused with a bitfield colon.
        let src = "void f(void) { int x = cond ? 1 : 2; }\n";
        let out = fmt(src);
        assert!(
            out.contains("1 : 2") || out.contains("? 1 : 2"),
            "ternary colon must still have spaces:\n{out}"
        );
    }

    #[test]
    fn case_label_no_space_before_colon() {
        let src = "void f(int x) { switch (x) { case 1: break; default: break; } }\n";
        let out = fmt(src);
        assert!(
            out.lines().any(|l| l.trim() == "case 1:"),
            "case label must not have space before colon:\n{out}"
        );
        assert!(
            out.lines().any(|l| l.trim() == "default:"),
            "default label must not have space before colon:\n{out}"
        );
    }

    // ── add_braces_to_if / _while / _for ─────────────────────────────────────

    fn cfg_add_braces_if() -> Config {
        use crate::config::BraceConfig;
        Config {
            braces: BraceConfig {
                add_braces_to_if: true,
                ..BraceConfig::default()
            },
            ..Config::default()
        }
    }

    fn cfg_add_braces_all() -> Config {
        use crate::config::BraceConfig;
        Config {
            braces: BraceConfig {
                add_braces_to_if: true,
                add_braces_to_while: true,
                add_braces_to_for: true,
                ..BraceConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn add_braces_if_simple() {
        let src = "void f() { if (x) return; }\n";
        let out = fmt_with(src, &cfg_add_braces_if());
        assert!(out.contains("if (x) {"), "missing opening brace:\n{out}");
        assert!(out.contains("return;"), "body lost:\n{out}");
        // The closing `}` of the if-block must appear before the function `}`.
        let if_close = out.find("if").unwrap();
        let braces: Vec<_> = out[if_close..].match_indices('}').collect();
        assert!(
            braces.len() >= 2,
            "need at least if-close and fn-close:\n{out}"
        );
    }

    #[test]
    fn add_braces_if_already_braced() {
        let src = "void f() { if (x) { return; } }\n";
        let out = fmt_with(src, &cfg_add_braces_if());
        // Should not add extra brace levels.
        let brace_count = out.chars().filter(|&c| c == '{').count();
        // Exactly 2: function body + if body.
        assert_eq!(brace_count, 2, "unexpected brace count:\n{out}");
    }

    #[test]
    fn add_braces_if_else() {
        // Default nl_brace_else=true: else starts on its own line.
        let src = "void f() { if (x) a(); else b(); }\n";
        let out = fmt_with(src, &cfg_add_braces_if());
        assert!(out.contains("if (x) {"), "if branch missing brace:\n{out}");
        assert!(out.contains("else {"), "else branch missing brace:\n{out}");
    }

    #[test]
    fn add_braces_else_if_chain() {
        // Default nl_brace_else=true: else-if on its own line.
        let src = "void f() { if (a) x(); else if (b) y(); else z(); }\n";
        let out = fmt_with(src, &cfg_add_braces_if());
        // All three branches must be braced.
        assert!(out.contains("if (a) {"), "if-branch:\n{out}");
        assert!(out.contains("else if (b) {"), "else-if branch:\n{out}");
        assert!(out.contains("else {"), "else branch:\n{out}");
    }

    #[test]
    fn add_braces_for_simple() {
        let src = "void f() { for (int i=0;i<10;i++) do_thing(i); }\n";
        let out = fmt_with(
            src,
            &Config {
                braces: crate::config::BraceConfig {
                    add_braces_to_for: true,
                    ..crate::config::BraceConfig::default()
                },
                ..Config::default()
            },
        );
        assert!(
            out.contains("i++) {") || out.contains("i < 10; i++) {"),
            "for brace:\n{out}"
        );
        assert!(out.contains("do_thing(i);"), "body lost:\n{out}");
    }

    #[test]
    fn add_braces_while_simple() {
        let src = "void f() { while (cond) work(); }\n";
        let out = fmt_with(
            src,
            &Config {
                braces: crate::config::BraceConfig {
                    add_braces_to_while: true,
                    ..crate::config::BraceConfig::default()
                },
                ..Config::default()
            },
        );
        assert!(out.contains("while (cond) {"), "while brace:\n{out}");
        assert!(out.contains("work();"), "body lost:\n{out}");
    }

    #[test]
    fn add_braces_do_while_not_wrapped() {
        // The `while` in a do-while is a terminator — must NOT get a body wrapped.
        let src = "void f() { do work(); while (cond); }\n";
        let out = fmt_with(
            src,
            &Config {
                braces: crate::config::BraceConfig {
                    add_braces_to_while: true,
                    ..crate::config::BraceConfig::default()
                },
                ..Config::default()
            },
        );
        // The while(...); must remain as a terminator, not while(...) { ... }
        assert!(
            out.contains("while (cond);"),
            "do-while terminator must not be wrapped:\n{out}"
        );
    }

    #[test]
    fn add_braces_nested_if_in_if() {
        // Dangling else: else belongs to inner if.
        let src = "void f() { if (a) if (b) x(); else y(); }\n";
        let out = fmt_with(src, &cfg_add_braces_if());
        // Inner if-else must both be braced.
        assert!(out.contains("if (b) {"), "inner if:\n{out}");
        assert!(out.contains("else {"), "inner else:\n{out}");
        // Outer if wraps the entire inner if-else.
        let open_count = out.chars().filter(|&c| c == '{').count();
        // fn body + outer-if body + inner-if body + inner-else body = 4
        assert_eq!(open_count, 4, "expected 4 braces:\n{out}");
    }

    #[test]
    fn add_braces_preserves_trailing_block_comment() {
        // Inline `/* comment */` on the same line as the statement must stay
        // with the statement when braces are injected, not fall outside the `}`.
        let src = "void f() { if (cond)\n    return -1; /* no good */\n}\n";
        let out = fmt_with(src, &cfg_add_braces_if());
        assert!(
            out.contains("return -1; /* no good */"),
            "inline comment must stay on the return line:\n{out}"
        );
        // The comment must appear before the closing brace of the if-body.
        let comment_pos = out.find("/* no good */").unwrap();
        let after_comment = &out[comment_pos..];
        assert!(
            after_comment.contains('}'),
            "closing brace must come after the comment:\n{out}"
        );
    }

    // ── nl_fcall_brace: macro call + block body ───────────────────────────────

    #[test]
    fn macro_call_brace_stays_on_same_line() {
        // DL_FOREACH_SAFE-style macro call: `{` must NOT be moved to a new line.
        let src = "void f() { DL_FOREACH_SAFE(head, n, tmp) { use(n); } }\n";
        let out = fmt(src);
        assert!(
            out.contains("DL_FOREACH_SAFE(head, n, tmp) {"),
            "macro-call brace must stay on same line:\n{out}"
        );
    }

    #[test]
    fn macro_call_brace_no_space_normalized() {
        // Source has `MACRO(){` with no space — formatter should add the space.
        let src = "void f() { MACRO(a, b){\nwork();\n} }\n";
        let out = fmt(src);
        assert!(
            out.contains("MACRO(a, b) {"),
            "space must be added before macro-call brace:\n{out}"
        );
    }

    #[test]
    fn fn_def_brace_still_on_own_line() {
        // Regular function definition must still put `{` on its own line with fn_brace_newline.
        let src = "int foo(int x) { return x; }\n";
        let out = fmt(src);
        // With fn_brace_newline=true (default), `{` goes on its own line.
        assert!(
            out.contains("int foo(int x)\n{"),
            "fn def brace must go on own line:\n{out}"
        );
    }

    #[test]
    fn constructor_brace_still_on_own_line() {
        // Constructor inside a class body — must still apply fn_brace_newline.
        let src = "class Foo { Foo(int x) { m_ = x; } };\n";
        let out = fmt(src);
        // The constructor `{` should be on its own line.
        assert!(
            out.contains("Foo(int x)\n"),
            "constructor brace must go on own line:\n{out}"
        );
    }

    // ── goto label indentation ────────────────────────────────────────────────

    #[test]
    fn goto_label_at_column_zero() {
        let src = "void f() {\n    if (fail) goto error;\n    return;\nerror:\n    free(p);\n}\n";
        let out = fmt(src);
        // `error:` must appear at column 0 (no leading spaces).
        assert!(
            out.lines().any(|l| l == "error:"),
            "goto label must be at column 0:\n{out}"
        );
    }

    #[test]
    fn goto_label_code_after_remains_indented() {
        let src = "void f() {\n    goto done;\ndone:\n    return;\n}\n";
        let out = fmt(src);
        assert!(out.lines().any(|l| l == "done:"), "label at col 0:\n{out}");
        assert!(
            out.lines().any(|l| l == "    return;"),
            "code after label must still be indented:\n{out}"
        );
    }

    #[test]
    fn goto_label_multiple_labels() {
        let src = "void f() {\nerror_a:\n    cleanup_a();\nerror_b:\n    cleanup_b();\n}\n";
        let out = fmt(src);
        assert!(
            out.lines().any(|l| l == "error_a:"),
            "error_a at col 0:\n{out}"
        );
        assert!(
            out.lines().any(|l| l == "error_b:"),
            "error_b at col 0:\n{out}"
        );
    }

    #[test]
    fn goto_label_indent_goto_labels_true() {
        // When indent_goto_labels = true, labels follow normal indentation.
        use crate::config::IndentConfig;
        let config = Config {
            indent: IndentConfig {
                indent_goto_labels: true,
                ..IndentConfig::default()
            },
            ..Config::default()
        };
        let src = "void f() {\nerror:\n    cleanup();\n}\n";
        let out = fmt_with(src, &config);
        // Label should be indented at function body level (4 spaces).
        assert!(
            out.lines().any(|l| l == "    error:"),
            "label must be indented when indent_goto_labels=true:\n{out}"
        );
    }

    #[test]
    fn goto_label_does_not_affect_ternary_colon() {
        // A ternary `a ? b : c` must not be confused with a goto label.
        let src = "void f() { int x = a ? b : c; }\n";
        let out = fmt(src);
        assert!(
            out.contains("a ? b : c"),
            "ternary must keep spaces around colon:\n{out}"
        );
    }

    #[test]
    fn add_braces_nested_for_in_if() {
        let src = "void f() { if (ok) for (int i=0;i<n;i++) use(i); }\n";
        let out = fmt_with(src, &cfg_add_braces_all());
        assert!(out.contains("if (ok) {"), "if brace:\n{out}");
        // for inside if body must also be braced
        assert!(
            out.contains("i++) {") || out.contains("i < n; i++) {"),
            "for brace:\n{out}"
        );
    }

    #[test]
    fn enum_closing_brace_not_aligned_to_last_assign() {
        // When the last enum value has no trailing comma, the `=` sets assign_col.
        // The closing `}` must NOT align to that column — it goes at indent level 0.
        let src = "enum foo {\n    A = 1,\n    B = 2\n};\n";
        let out = fmt(src);
        // `};` must appear at column 0, not indented to the `=` position
        assert!(
            out.lines().any(|l| l == "};"),
            "closing `}};` should be at column 0:\n{out}"
        );
    }
}
