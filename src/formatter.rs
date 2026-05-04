use crate::config::{BraceStyle, Config, PointerAlign};
use crate::error::FunkyError;
use crate::token::{Token, TokenKind};

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
    /// The last non-whitespace, non-newline token kind we emitted.
    prev: Option<TokenKind>,
    /// Number of switch bodies we are currently inside — used to dedent case/default.
    switch_depth: u32,
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
    /// Set when `public`/`private`/`protected` is emitted so the following `:`
    /// gets a newline after it.
    in_access_label: bool,
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
    /// Parallel to paren_col_stack: true when the `(` was the last
    /// non-whitespace on its line, meaning continuations use a regular indent
    /// instead of column alignment.
    paren_eol_stack: Vec<bool>,

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
            prev: None,
            switch_depth: 0,
            class_depth: 0,
            pending_switch: false,
            pending_type: false,
            pending_extern_c: false,
            in_case_label: false,
            in_access_label: false,
            suppress_next_space: false,
            template_depth: 0,
            last_was_template_close: false,
            cast_paren_stack: Vec::new(),
            last_was_cast_close: false,
            current_col: 0,
            paren_col_stack: Vec::new(),
            paren_eol_stack: Vec::new(),
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
        let mut synthetic_consumed = false;
        while let Some(t) = self.tokens.get(self.pos) {
            match t.kind {
                TokenKind::Whitespace => {
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
        self.output.push_str(self.config.newline_str());
        self.at_line_start = true;
        self.suppress_next_space = false;
        self.current_col = 0;
    }

    fn indent(&mut self) {
        if self.paren_depth > 0 {
            self.align_to_paren();
            return;
        }
        let unit = self.config.indent_str();
        for _ in 0..self.indent_level {
            self.output.push_str(&unit);
            self.current_col += unit.len();
        }
        if self.indent_level > 0 {
            self.at_line_start = false;
        }
    }

    fn align_to_paren(&mut self) {
        // When `(` was the last non-whitespace before the newline, aligning to
        // its column would push continuation far right. Use a normal indent
        // (one level deeper than the current scope) instead.  We detect this
        // lazily on the first continuation line and record it in paren_eol_stack
        // so all subsequent lines in the same paren level behave consistently.
        if let Some(eol) = self.paren_eol_stack.last_mut() {
            if !*eol && self.output.trim_end().ends_with('(') {
                *eol = true;
            }
            if *eol {
                let unit = self.config.indent_str();
                for _ in 0..=self.indent_level {
                    self.output.push_str(&unit);
                    self.current_col += unit.len();
                }
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

    /// Update `self.prev` and clear the template-close flag in one step.
    /// Template-close arms must NOT use this — they set the two fields directly.
    fn set_prev(&mut self, kind: TokenKind) {
        self.prev = Some(kind);
        self.last_was_template_close = false;
        self.last_was_cast_close = false;
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
            if self.blank_lines == 0 {
                self.blank_lines = 1;
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
        let mut i = self.pos;
        loop {
            let Some(tk) = self.tokens.get(i) else {
                return false;
            };
            match tk.kind {
                TokenKind::Whitespace | TokenKind::Newline => i += 1,
                TokenKind::Star => i += 1,
                TokenKind::Ident => return true,
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
            Some(TokenKind::KwElse) => self.config.braces.cuddle_else,
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
        // Comments are transparent — a `/* ... */` or `// ...` before the first
        // real statement does not end the declaration run.
        if matches!(kind, TokenKind::CommentLine | TokenKind::CommentBlock) {
            return;
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
        // Must start with * or &
        if !matches!(
            self.tokens.get(i).map(|t| t.kind),
            Some(TokenKind::Star | TokenKind::Amp)
        ) {
            return false;
        }
        i += 1;
        i = skip_ws(i);
        // Then an identifier (the function-pointer name)
        if !matches!(self.tokens.get(i).map(|t| t.kind), Some(TokenKind::Ident)) {
            return false;
        }
        i += 1;
        i = skip_ws(i);
        // Then immediately `)`
        matches!(self.tokens.get(i).map(|t| t.kind), Some(TokenKind::RParen))
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
            Some(TokenKind::CommentBlock | TokenKind::CommentLine)
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
                | TokenKind::CommentLine => {
                    if i == 0 {
                        return None;
                    }
                    i -= 1;
                }
                k => return Some(k),
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
                } else if self.rparen_closes_ctrl_flow() {
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
        // `)` followed by `*`/`&` is a pointer declarator only when the `)`
        // closed a cast paren — e.g. `(int) *p` dereference vs `(cast*) name`.
        // Plain expression parens `(expr) * value` are multiplication.
        if self.prev == Some(TokenKind::RParen) {
            return self.last_was_cast_close;
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
            ) || (before == TokenKind::Keyword
                && matches!(self.tokens[b].lexeme, "return" | "case" | "throw"))
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
                    ) || (outer == TokenKind::Keyword
                        && matches!(
                            self.tokens[bb].lexeme,
                            "if" | "while" | "for" | "switch" | "return" | "case" | "throw"
                        ))
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
        while i < self.tokens.len() {
            match self.tokens[i].kind {
                TokenKind::Ident | TokenKind::Keyword => {
                    found_name = true;
                    i += 1;
                }
                TokenKind::ColonColon => {
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
        while i < self.tokens.len()
            && matches!(
                self.tokens[i].kind,
                TokenKind::Whitespace | TokenKind::Newline | TokenKind::PreprocLine
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
                return self.config.spacing.space_inside_parens;
            }
            if next == TokenKind::RBracket {
                return self.config.spacing.space_inside_brackets;
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
            return false;
        }

        // Never space after these openers
        if matches!(
            prev,
            TokenKind::LParen | TokenKind::LBracket | TokenKind::Tilde | TokenKind::Bang
        ) {
            if prev == TokenKind::LParen {
                return self.config.spacing.space_inside_parens;
            }
            if prev == TokenKind::LBracket {
                return self.config.spacing.space_inside_brackets;
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

        // No space between unary prefix op and its operand
        if matches!(prev, TokenKind::PlusPlus | TokenKind::MinusMinus) {
            // post-increment was before this token; could also be pre
            // If prev ends an expression it was post, space is fine; if not, no space
            return false;
        }

        // Space before `(` depends on context
        if next == TokenKind::LParen {
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
                // e.g. cast or function pointer call — no extra space
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

        // After comma
        if prev == TokenKind::Comma {
            return self.config.spacing.space_after_comma;
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

        // Colon: ternary, labels, case, member init — space on both sides by default
        if next == TokenKind::Colon {
            // case X: or label: — no trailing space before ':'
            return false;
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
            return self.config.spacing.space_after_cast;
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
                    self.write(&normalized);
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
                    let normalized = if normalized.contains(&format!("{nl}*/")) {
                        normalized.replace(&format!("{nl}*/"), &format!("{nl} */"))
                    } else {
                        normalized
                    };
                    self.write(&normalized);
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
                                    for (idx, (lex, kind)) in content.iter().enumerate() {
                                        if idx > 0 && !matches!(kind, TokenKind::Comma) {
                                            self.write(" ");
                                        }
                                        self.write(lex);
                                    }
                                    self.write(" }");
                                }
                                self.pos = end + 1;
                                self.set_prev(TokenKind::RBrace);
                                continue;
                            }
                        }
                        // extern "C" { } is a linkage specification, not a function
                        // body. The { always stays on the same line regardless of
                        // brace style or fn_brace_newline.
                        BraceCtx::ExternC => {
                            if self.at_line_start {
                                self.trim_to_prev_line_end();
                            }
                            self.space();
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
                    }
                    if ctx == BraceCtx::Type {
                        self.class_depth += 1;
                    }
                    if ctx == BraceCtx::Function
                        && self.config.newlines.blank_line_after_var_decl_block
                    {
                        self.in_var_decl_block = true;
                        self.at_func_stmt_start = true;
                        self.saw_func_decl = false;
                    }
                    self.pending_switch = false;
                    self.pending_type = false;
                    self.pending_extern_c = false;
                    let is_large_init = ctx == BraceCtx::Other
                        && self.config.braces.expand_large_initializers
                        && self.large_flat_initializer_end().is_some();
                    self.brace_stack.push(ctx);
                    self.large_init_stack.push(is_large_init);
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
                    // Discard blank lines right before `}` — trailing blank lines
                    // inside a block are rarely intentional and look odd.
                    self.blank_lines = 0;
                    let closing_ctx = self.brace_stack.last().copied().unwrap_or(BraceCtx::Other);
                    if closing_ctx != BraceCtx::ExternC && self.indent_level > 0 {
                        self.indent_level -= 1;
                    }
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
                    if ctx == BraceCtx::Function {
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
                        if self.in_var_decl_block
                            && self.brace_stack.last() == Some(&BraceCtx::Function)
                        {
                            self.at_func_stmt_start = true;
                        }
                    }
                    self.set_prev(TokenKind::Semi);
                }

                // ── Paren depth tracking ──────────────────────────────────────
                TokenKind::LParen => {
                    self.flush_blank_lines();
                    let is_cast = self.next_is_type_kw();
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
                    self.paren_col_stack.push(self.current_col);
                    self.paren_eol_stack.push(false);
                    self.set_prev(TokenKind::LParen);
                }
                TokenKind::RParen => {
                    self.flush_blank_lines();
                    if self.at_line_start {
                        self.align_to_paren();
                    } else if self.config.spacing.space_inside_parens {
                        self.space();
                    }
                    self.write(")");
                    self.paren_depth = self.paren_depth.saturating_sub(1);
                    self.paren_col_stack.pop();
                    self.paren_eol_stack.pop();
                    let is_cast_close = self.cast_paren_stack.pop().unwrap_or(false);
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
                    if self.config.spacing.space_inside_brackets && !self.at_line_start {
                        self.space();
                    }
                    self.write("]");
                    self.bracket_depth = self.bracket_depth.saturating_sub(1);
                    self.set_prev(TokenKind::RBracket);
                }

                // ── Colon after case / default / access specifier ─────────────
                TokenKind::Colon => {
                    self.flush_blank_lines();
                    self.write(":");
                    if self.in_case_label {
                        self.in_case_label = false;
                        self.nl();
                        self.skip_next_newline = true;
                    } else if self.in_access_label {
                        self.in_access_label = false;
                        self.nl();
                        self.skip_next_newline = true;
                    }
                    self.set_prev(TokenKind::Colon);
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

                // ── case / default labels — dedented to switch level ──────────
                TokenKind::KwCase | TokenKind::KwDefault => {
                    self.flush_blank_lines();
                    if self.at_line_start {
                        // Dedent one level relative to the switch body.
                        let saved = self.indent_level;
                        if self.switch_depth > 0 && self.indent_level > 0 {
                            self.indent_level -= 1;
                        }
                        self.indent();
                        self.indent_level = saved;
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
                    let is_binary = !self.at_line_start && self.prev.is_some_and(|p| p.ends_expr());
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
    let output = Fmt::new(config, tokens).format()?;
    let nl = config.newline_str();
    let output = if config.spacing.align_right_cmt_span > 0 {
        align_trailing_comments(&output, nl, config.spacing.align_right_cmt_gap.max(1))
    } else {
        output
    };
    let output = if config.spacing.align_enum_equ_span > 0 {
        align_enum_equals(&output, nl)
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

/// Returns the byte index of the `//` or `/*` that starts a trailing inline
/// comment on `line`, or `None` if the line has no trailing comment (standalone
/// comment lines and blank lines also return `None`).
fn trailing_comment_col(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("/*") {
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
            let before = &line[..i];
            if before.bytes().any(|b| b != b' ' && b != b'\t') {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Align trailing `//` comments within groups of consecutive lines that all
/// carry a trailing comment.  Each group is aligned to the widest code column.
/// `min_gap` is the minimum number of spaces between code end and comment.
fn align_trailing_comments(output: &str, nl: &str, min_gap: usize) -> String {
    let lines: Vec<&str> = output.split(nl).collect();
    let n = lines.len();
    let cols: Vec<Option<usize>> = lines.iter().map(|l| trailing_comment_col(l)).collect();
    let mut result: Vec<String> = lines.iter().map(|l| l.to_string()).collect();

    let mut i = 0;
    while i < n {
        if cols[i].is_some() {
            let mut j = i + 1;
            while j < n && cols[j].is_some() {
                j += 1;
            }
            // Group is lines[i..j]; align if 2+ lines.
            if j > i + 1 {
                // Find widest code (trimmed) length across the group; comments
                // align at max_code_len + min_gap.
                let max_code_len = (i..j)
                    .map(|k| lines[k][..cols[k].unwrap()].trim_end().len())
                    .max()
                    .unwrap();
                let target = max_code_len + min_gap;
                for k in i..j {
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
    if !trimmed.trim_end().ends_with(',') {
        return None;
    }
    let bytes = line.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] != b'=' {
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
            continue; // ==
        }
        if i > 0
            && matches!(
                bytes[i - 1],
                b'!' | b'<' | b'>' | b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^'
            )
        {
            continue; // compound op
        }
        return Some(i);
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
fn align_enum_equals(output: &str, nl: &str) -> String {
    let lines: Vec<&str> = output.split(nl).collect();
    let n = lines.len();
    let cols: Vec<Option<usize>> = lines.iter().map(|l| enum_eq_col(l)).collect();
    let mut result: Vec<String> = lines.iter().map(|l| l.to_string()).collect();

    let mut i = 0;
    while i < n {
        if cols[i].is_some() {
            // Extend the group through bare members as well as `=` members.
            let mut j = i + 1;
            while j < n && (cols[j].is_some() || is_bare_enum_member(lines[j])) {
                j += 1;
            }
            // Collect indices of only the `=`-bearing lines in this group.
            let eq_indices: Vec<usize> = (i..j).filter(|&k| cols[k].is_some()).collect();
            if eq_indices.len() > 1 {
                let max_name_len = eq_indices
                    .iter()
                    .map(|&k| lines[k][..cols[k].unwrap()].trim_end().len())
                    .max()
                    .unwrap();
                let target = max_name_len + 1;
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
        let src = "void f(int x){switch(x){case 1:y=1;break;case 2:y=2;break;default:y=0;break;}}";
        let out = fmt(src);
        // case/default labels must be at switch indent level (4 spaces, not 8).
        assert!(
            out.lines().any(|l| l == "    case 1:"),
            "case 1 not at switch level:\n{out}"
        );
        assert!(
            out.lines().any(|l| l == "    case 2:"),
            "case 2 not at switch level:\n{out}"
        );
        assert!(
            out.lines().any(|l| l == "    default:"),
            "default not at switch level:\n{out}"
        );
        // Body inside case must be indented one further level (8 spaces).
        assert!(
            out.lines().any(|l| l.starts_with("        y")),
            "case body not indented deeper than label:\n{out}"
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
        // (void)expr; at block scope must keep indentation and must not gain a
        // spurious space after the cast when space_after_cast = false (default).
        // fn_brace_newline=true (default) puts the function { on its own line.
        let src = "void f() {\n    (void)func();\n    (void)bar(1, 2);\n}\n";
        let out = fmt(src);
        assert_eq!(
            out,
            "void f()\n{\n    (void)func();\n    (void)bar(1, 2);\n}\n"
        );
    }

    #[test]
    fn cast_space_after_cast_false() {
        let src = "void f() { int x = (int)3.14; }\n";
        let out = fmt(src);
        assert!(
            out.contains("(int)3.14"),
            "space_after_cast=false should produce no space: {out}"
        );
    }

    #[test]
    fn cast_double_cast_no_space() {
        // Chained casts: (double)(int)x — no space between the two parens.
        let src = "void f() { double d = (double)(int)x; }\n";
        let out = fmt(src);
        assert!(
            out.contains("(double)(int)x"),
            "double cast should have no space between: {out}"
        );
    }

    #[test]
    fn cast_user_defined_type_no_space() {
        // (MyType) val — user-defined type cast must honor space_after_cast=false.
        let src = "void f() { MyType x = (MyType) val; }\n";
        let out = fmt(src);
        assert!(
            out.contains("(MyType)val"),
            "user-defined type cast should have no space: {out}"
        );
    }

    #[test]
    fn cast_user_defined_pointer_no_space() {
        // (MyType *) val — pointer cast with user-defined type.
        let src = "void f() { MyType *x = (MyType *) val; }\n";
        let out = fmt(src);
        assert!(
            out.contains("(MyType*)val") || out.contains("(MyType *)val"),
            "user-defined pointer cast should have no space after ')': {out}"
        );
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
            out.contains("} /* extern \"C\" */"),
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
    fn extern_c_brace_always_same_line() {
        // extern "C" is a linkage specification, not a function body.
        // The { must stay on the same line regardless of fn_brace_newline or brace style.
        let src = "extern \"C\" {\nint foo(void);\n}\n";

        let out = fmt(src);
        assert!(
            out.contains("extern \"C\" {"),
            "extern \"C\" {{ must stay on same line with default config, got:\n{out}"
        );

        // fn_brace_newline=false: still same line
        let mut cfg = Config::default();
        cfg.braces.fn_brace_newline = false;
        let out2 = fmt_with(src, &cfg);
        assert!(
            out2.contains("extern \"C\" {"),
            "extern \"C\" {{ must stay on same line when fn_brace_newline=false, got:\n{out2}"
        );

        // Allman brace style: extern "C" still same line
        let mut cfg3 = Config::default();
        cfg3.braces.style = BraceStyle::Allman;
        let out3 = fmt_with(src, &cfg3);
        assert!(
            out3.contains("extern \"C\" {"),
            "extern \"C\" {{ must stay on same line even in Allman mode, got:\n{out3}"
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
    fn space_after_semi_before_plusplus_in_for() {
        let out = fmt("void f() { for (i = 0; i < 10;++i) {} }\n");
        assert!(
            !out.contains(";++i") && out.contains("; ++i"),
            "semicolon before ++ must have a space: {out}"
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
    fn block_comment_closing_gets_space() {
        let out = fmt("/*\n * foo\n*/\nvoid f() {}\n");
        assert!(
            out.contains(" */"),
            "closing */ must have a leading space, got:\n{out}"
        );
        assert!(
            !out.contains("\n*/"),
            "closing */ must not be flush at line start, got:\n{out}"
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
    fn block_comment_closing_already_spaced_unchanged() {
        let src = "/*\n * foo\n */\nvoid f() {}\n";
        let out = fmt(src);
        assert!(
            out.contains(" */") && !out.contains("  */"),
            "already-correct */ must not be double-spaced, got:\n{out}"
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
        let out = fmt_with(src, &cfg_align(1));
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
        let out = fmt_with(src, &cfg_align(1));
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
    fn align_trailing_comments_blank_line_breaks_group() {
        let src = "int a; // g1\nint b; // g1\n\nint c; // g2\n";
        let out = fmt_with(src, &cfg_align(1));
        let cols: Vec<usize> = out.lines().filter_map(trailing_comment_col).collect();
        assert_eq!(cols.len(), 3, "expected 3 inline comments, got:\n{out}");
        // First two should be aligned (same group); third may differ.
        assert_eq!(cols[0], cols[1], "group-1 comments not aligned:\n{out}");
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
    fn align_enum_equals_blank_line_breaks_group() {
        let src = "enum E { A = 1,\nLONG_NAME = 2,\n\nC = 10,\nD = 11,\n};\n";
        let out = fmt_with(src, &cfg_enum_align(1));
        let positions: Vec<usize> = out.lines().filter_map(enum_eq_col).collect();
        assert_eq!(
            positions.len(),
            4,
            "expected 4 enum value lines, got:\n{out}"
        );
        // First two are in group-1 and must be aligned.
        assert_eq!(positions[0], positions[1], "group-1 = not aligned:\n{out}");
        // Last two are in group-2 (both 1-char names, already equal).
        assert_eq!(positions[2], positions[3], "group-2 = not aligned:\n{out}");
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
}
