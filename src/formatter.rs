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
        self.at_func_stmt_start = false;
        if Self::is_decl_start(kind) {
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
                TokenKind::LBrace => return None,
                TokenKind::RBrace => return Some(self.pos + offset),
                TokenKind::Whitespace | TokenKind::Newline => {}
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

    // ── Brace context inference ───────────────────────────────────────────────

    fn infer_brace_ctx(&self) -> BraceCtx {
        let prev = match self.prev {
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
                    | TokenKind::RParen
                    | TokenKind::Gt
            )
        ) {
            return true;
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
            ) || (before == TokenKind::Keyword
                && matches!(self.tokens[b].lexeme, "return" | "case" | "throw"));
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
                TokenKind::Whitespace | TokenKind::Newline => {
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
                TokenKind::Whitespace | TokenKind::Newline
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
                    self.flush_blank_lines();
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
                        _ => match self.config.braces.style {
                            BraceStyle::Allman => {
                                self.ensure_own_line();
                                self.write("{");
                            }
                            BraceStyle::Kr | BraceStyle::Stroustrup => {
                                if !self.at_line_start {
                                    self.space();
                                } else {
                                    self.indent();
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
                    self.brace_stack.push(ctx);
                    if ctx != BraceCtx::ExternC {
                        self.indent_level += 1;
                    }
                    self.nl();
                    self.skip_next_newline = true;
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
                    }
                    self.write("(");
                    self.paren_depth += 1;
                    self.paren_col_stack.push(self.current_col);
                    self.set_prev(TokenKind::LParen);
                }
                TokenKind::RParen => {
                    self.flush_blank_lines();
                    if self.config.spacing.space_inside_parens && !self.at_line_start {
                        self.space();
                    }
                    self.write(")");
                    self.paren_depth = self.paren_depth.saturating_sub(1);
                    self.paren_col_stack.pop();
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
                    let is_binary = self.prev.is_some_and(|p| p.ends_expr());
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
    Fmt::new(config, tokens).format()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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
        // `{\n}` should become `{}` when collapse_empty_body = true (default).
        let src = "void f() {\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("void f() {}"),
            "empty function body should collapse to {{}}: {out}"
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
        // default: int * p
        let src = "int*p;";
        let out = fmt(src);
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
    fn var_decl_block_off_by_default() {
        // Feature is off by default — no blank line inserted.
        let src = "void f() {\n    int x = 1;\n    foo(x);\n}\n";
        let out = fmt(src); // default config, feature disabled
        assert!(
            !out.contains("1;\n\n"),
            "blank line must not appear when feature is off: {out}"
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
        let src = "void f() {\n    (void)func();\n    (void)bar(1, 2);\n}\n";
        let out = fmt(src);
        assert_eq!(
            out,
            "void f() {\n    (void)func();\n    (void)bar(1, 2);\n}\n"
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
    fn extern_c_nested_function_still_indents() {
        let src = "extern \"C\" {\nvoid foo(void) {\nint x = 1;\n}\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("\nvoid foo(void) {"),
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
}
