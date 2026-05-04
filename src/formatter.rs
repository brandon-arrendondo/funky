use crate::config::{BraceStyle, Config};
use crate::error::FunkyError;
use crate::token::{Token, TokenKind};

// ── Context ───────────────────────────────────────────────────────────────────

/// What opened the most recent `{`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BraceCtx {
    Block,    // if/for/while/do/else/try/catch
    Type,     // struct/class/union/enum
    Namespace,
    Function, // function definition body
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
    }

    fn nl(&mut self) {
        self.output.push_str(self.config.newline_str());
        self.at_line_start = true;
    }

    fn indent(&mut self) {
        let unit = self.config.indent_str();
        for _ in 0..self.indent_level {
            self.output.push_str(&unit);
        }
        if self.indent_level > 0 {
            self.at_line_start = false;
        }
    }

    fn space(&mut self) {
        if !self.at_line_start && !self.output.ends_with(' ') {
            self.output.push(' ');
        }
    }

    /// Emit pending blank lines, capped to `max_blank_lines`.
    fn flush_blank_lines(&mut self) {
        let max = self.config.newlines.max_blank_lines as u32;
        if max > 0 {
            let emit = self.blank_lines.min(max);
            for _ in 0..emit {
                self.nl();
            }
        }
        self.blank_lines = 0;
    }

    /// Ensure we're at the start of a fresh line (emit newline + indent if not).
    fn ensure_own_line(&mut self) {
        if !self.at_line_start {
            self.nl();
        }
        self.indent();
    }

    // ── Inline-comment detection ──────────────────────────────────────────────

    /// True if the next token (skipping only `Whitespace`, not `Newline`) is a
    /// `CommentLine` whose source line matches `source_line`.
    fn peek_inline_comment(&self, source_line: u32) -> bool {
        let mut i = self.pos;
        while i < self.tokens.len() && self.tokens[i].kind == TokenKind::Whitespace {
            i += 1;
        }
        matches!(
            self.tokens.get(i),
            Some(t) if t.kind == TokenKind::CommentLine && t.span.line == source_line
        )
    }

    // ── Brace context inference ───────────────────────────────────────────────

    fn infer_brace_ctx(&self) -> BraceCtx {
        let prev = match self.prev {
            Some(k) => k,
            None => return BraceCtx::Other,
        };
        match prev {
            TokenKind::KwNamespace => BraceCtx::Namespace,
            TokenKind::KwStruct
            | TokenKind::KwClass
            | TokenKind::KwUnion
            | TokenKind::KwEnum => BraceCtx::Type,
            TokenKind::RParen => BraceCtx::Function, // function params or control
            TokenKind::KwElse | TokenKind::KwDo | TokenKind::KwTry => BraceCtx::Block,
            TokenKind::Ident => {
                // Likely a function definition body.
                BraceCtx::Function
            }
            // After `=`, `(`, `,`, `{` → initializer-list style
            TokenKind::Eq
            | TokenKind::PlusEq
            | TokenKind::MinusEq
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Comma
            | TokenKind::Colon => BraceCtx::Other,
            _ => BraceCtx::Other,
        }
    }

    // ── Spacing decision ──────────────────────────────────────────────────────

    /// Should a space be emitted before `next`, given the last emitted token `prev`?
    fn needs_space(&self, next: TokenKind) -> bool {
        let prev = match self.prev {
            Some(k) => k,
            None => return false,
        };

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
            if matches!(prev, TokenKind::Ident) {
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
            // unary context — no space between operator and operand
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
                    self.prev = Some(TokenKind::PreprocLine);
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
                    self.prev = Some(TokenKind::CommentLine);
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
                    self.prev = Some(TokenKind::CommentBlock);
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
                        }
                        _ => {
                            match self.config.braces.style {
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
                            }
                        }
                    }

                    self.brace_stack.push(ctx);
                    self.indent_level += 1;
                    self.nl();
                    self.skip_next_newline = true;
                    self.prev = Some(TokenKind::LBrace);
                }

                // ── Closing brace ─────────────────────────────────────────────
                TokenKind::RBrace => {
                    // Discard blank lines right before `}` — trailing blank lines
                    // inside a block are rarely intentional and look odd.
                    self.blank_lines = 0;
                    if self.indent_level > 0 {
                        self.indent_level -= 1;
                    }
                    self.ensure_own_line();
                    self.write("}");

                    let ctx = self.brace_stack.pop().unwrap_or(BraceCtx::Other);

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

                    // `typedef struct { … } Name;` — name stays on same line as `}`.
                    let typedef_name = matches!(ctx, BraceCtx::Type)
                        && matches!(next_kind, Some(TokenKind::Ident));

                    // Cuddle else/catch/while (do-while)?
                    let cuddle = match next_kind {
                        Some(TokenKind::KwElse) => self.config.braces.cuddle_else,
                        Some(TokenKind::KwCatch) => self.config.braces.cuddle_catch,
                        Some(TokenKind::KwWhile) => matches!(ctx, BraceCtx::Block),
                        _ => false,
                    };

                    if typedef_name {
                        self.space();
                    } else if cuddle && matches!(self.config.braces.style, BraceStyle::Kr) {
                        self.space();
                    } else if cuddle
                        && matches!(self.config.braces.style, BraceStyle::Stroustrup)
                        && next_kind == Some(TokenKind::KwElse)
                    {
                        self.nl();
                        self.skip_next_newline = true;
                    } else if self.peek_inline_comment(tok.span.line) {
                        // trailing inline comment on same line — let CommentLine close it
                    } else {
                        self.nl();
                        self.skip_next_newline = true;
                    }

                    self.prev = Some(TokenKind::RBrace);
                }

                // ── Semicolon ─────────────────────────────────────────────────
                TokenKind::Semi => {
                    self.flush_blank_lines();
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
                    }
                    self.prev = Some(TokenKind::Semi);
                }

                // ── Paren depth tracking ──────────────────────────────────────
                TokenKind::LParen => {
                    self.flush_blank_lines();
                    if self.needs_space(TokenKind::LParen) {
                        self.space();
                    }
                    self.write("(");
                    self.paren_depth += 1;
                    self.prev = Some(TokenKind::LParen);
                }
                TokenKind::RParen => {
                    self.flush_blank_lines();
                    if self.config.spacing.space_inside_parens && !self.at_line_start {
                        self.space();
                    }
                    self.write(")");
                    self.paren_depth = self.paren_depth.saturating_sub(1);
                    self.prev = Some(TokenKind::RParen);
                }

                // ── Bracket depth tracking ────────────────────────────────────
                TokenKind::LBracket => {
                    self.flush_blank_lines();
                    self.write("[");
                    self.bracket_depth += 1;
                    self.prev = Some(TokenKind::LBracket);
                }
                TokenKind::RBracket => {
                    self.flush_blank_lines();
                    if self.config.spacing.space_inside_brackets && !self.at_line_start {
                        self.space();
                    }
                    self.write("]");
                    self.bracket_depth = self.bracket_depth.saturating_sub(1);
                    self.prev = Some(TokenKind::RBracket);
                }

                // ── Colon after case / default ────────────────────────────────
                TokenKind::Colon => {
                    self.flush_blank_lines();
                    self.write(":");
                    // For case/default, peek to see if we need indent adjustment.
                    // We just emit newline and let the next token be indented.
                    // (A full implementation would track case-label depth.)
                    self.prev = Some(TokenKind::Colon);
                }

                // ── Everything else ───────────────────────────────────────────
                _ => {
                    self.flush_blank_lines();

                    if self.at_line_start {
                        self.indent();
                    } else if self.needs_space(tok.kind) {
                        self.space();
                    }

                    self.write(tok.lexeme);
                    self.prev = Some(tok.kind);
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

pub fn format<'src>(
    tokens: &[Token<'src>],
    config: &Config,
) -> Result<String, FunkyError> {
    Fmt::new(config, tokens).format()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn fmt(src: &str) -> String {
        let tokens = tokenize(src, "<test>").unwrap();
        format(&tokens, &Config::default()).unwrap()
    }

    fn fmt_with(src: &str, config: &Config) -> String {
        let tokens = tokenize(src, "<test>").unwrap();
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
        let mut config = Config::default();
        config.braces = BraceConfig {
            style: BraceStyle::Allman,
            cuddle_else: false,
            cuddle_catch: false,
        };
        let src = "if(x){y=1;}";
        let out = fmt_with(src, &config);
        // In Allman style, `{` is on its own line
        let brace_line = out.lines().find(|l| l.trim() == "{");
        assert!(brace_line.is_some(), "no standalone brace line in:\n{out}");
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
        let line = out.lines().find(|l| l.contains("int x")).expect("no x line");
        assert!(line.contains("// note"), "comment moved off line: {out}");
        // Subsequent statement must be on its own line.
        assert!(out.contains("\nint y"), "y not on new line: {out}");
    }

    #[test]
    fn inline_comment_after_semi_unicode() {
        let src = "int x = 1; // 变量定义\n";
        let out = fmt(src);
        let line = out.lines().find(|l| l.contains("int x")).expect("no x line");
        assert!(line.contains("// 变量定义"), "unicode comment moved off line: {out}");
    }

    #[test]
    fn inline_comment_after_brace() {
        let src = "void f() {\n    return;\n} // end\n";
        let out = fmt(src);
        let brace_line = out.lines().find(|l| l.trim_start().starts_with('}'))
            .expect("no } line");
        assert!(brace_line.contains("// end"), "comment not on }} line:\n{out}");
    }

    #[test]
    fn non_inline_comment_stays_separate() {
        let src = "int x = 1;\n// standalone\nint y = 2;\n";
        let out = fmt(src);
        // The x-line must not contain the comment.
        let x_line = out.lines().find(|l| l.contains("int x")).expect("no x line");
        assert!(!x_line.contains("//"), "standalone comment merged into x line:\n{out}");
        // The comment must appear on its own line.
        assert!(out.lines().any(|l| l.trim() == "// standalone"), "standalone comment missing:\n{out}");
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
}
