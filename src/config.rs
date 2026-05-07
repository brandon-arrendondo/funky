use serde::Deserialize;
use std::path::Path;

use crate::error::FunkyError;

// ── Top-level config ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub indent: IndentConfig,
    pub braces: BraceConfig,
    pub spacing: SpacingConfig,
    pub newlines: NewlineConfig,
    pub preprocessor: PreprocConfig,
    pub ignore: IgnoreConfig,
}

// ── Preprocessor ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PreprocConfig {
    /// Indent preprocessor directives relative to their `#if`/`#ifdef`/`#ifndef`
    /// nesting depth (analogous to uncrustify's `pp_indent = add`). Default false.
    pub pp_indent: bool,
    /// Number of spaces between `#endif` and a trailing `/*` comment.
    /// Default 1. Set to 2 to match uncrustify's `#endif  /* GUARD_H */` style.
    pub endif_comment_space: u32,
}

impl Default for PreprocConfig {
    fn default() -> Self {
        Self {
            pp_indent: false,
            endif_comment_space: 1,
        }
    }
}

// ── Ignore ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct IgnoreConfig {
    /// Glob patterns (relative to the directory being walked) to skip.
    /// Example: ["vendor/**", "third_party/**", "*.pb.h"]
    pub patterns: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, FunkyError> {
        let text = std::fs::read_to_string(path).map_err(|e| FunkyError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        toml::from_str(&text).map_err(|e| FunkyError::Config {
            path: path.display().to_string(),
            source: e,
        })
    }

    /// The string used to terminate lines in the output.
    pub fn newline_str(&self) -> &'static str {
        match self.newlines.style {
            NewlineStyle::Lf => "\n",
            NewlineStyle::Crlf => "\r\n",
            NewlineStyle::Native => {
                if cfg!(windows) {
                    "\r\n"
                } else {
                    "\n"
                }
            }
        }
    }

    /// One indentation level as a string.
    pub fn indent_str(&self) -> String {
        match self.indent.style {
            IndentStyle::Spaces => " ".repeat(self.indent.width as usize),
            IndentStyle::Tabs => "\t".to_string(),
        }
    }
}

// ── Indent ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndentConfig {
    pub style: IndentStyle,
    pub width: u8,
    /// Indent `case`/`default` labels one level inside `switch {}`, and indent
    /// the case body one further level (analogous to uncrustify's
    /// `indent_switch_case = <indent_columns>`).
    pub indent_switch_case: bool,
    /// When `false` (default), goto labels are placed at column 0 regardless
    /// of the current indentation level (analogous to uncrustify's
    /// `indent_label = 1`).  Set to `true` to indent labels at the same level
    /// as surrounding code.
    pub indent_goto_labels: bool,
}

impl Default for IndentConfig {
    fn default() -> Self {
        Self {
            style: IndentStyle::Spaces,
            width: 4,
            indent_switch_case: true,
            indent_goto_labels: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IndentStyle {
    Spaces,
    Tabs,
}

// ── Braces ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BraceConfig {
    pub style: BraceStyle,
    pub cuddle_else: bool,
    pub cuddle_catch: bool,
    /// Collapse `{\n}` to `{}` when the body is empty.
    pub collapse_empty_body: bool,
    /// Expand flat initializers that exceed `small_initializer_end`'s token
    /// limit to one element per line.  Nested-brace initializers are not
    /// affected (they fall through to normal block formatting).
    pub expand_large_initializers: bool,
    /// When `true` (default), function-definition opening braces always go on
    /// their own line regardless of `style`.  Control-flow braces (`if`, `for`,
    /// `while`, `switch`) are not affected and follow `style` as usual.
    /// Matches `nl_fdef_brace = add` in uncrustify.
    pub fn_brace_newline: bool,
    /// Controls placement of the `{` in `extern "C" { }` linkage blocks.
    /// `force_same_line` (default) always keeps `{` on the same line as
    /// `extern "C"`, matching mainstream style guides (Google, LLVM).
    /// `preserve` leaves the brace wherever the source has it, matching
    /// uncrustify's default behaviour when no `nl_extern_brace` rule is set.
    pub extern_c_brace: ExternCBrace,
    /// Add braces to braceless single-statement `if` bodies (analogous to
    /// uncrustify's `mod_full_brace_if = add`). Default false.
    pub add_braces_to_if: bool,
    /// Add braces to braceless single-statement `while` bodies (analogous to
    /// uncrustify's `mod_full_brace_while = add`). Default false.
    pub add_braces_to_while: bool,
    /// Add braces to braceless single-statement `for` bodies (analogous to
    /// uncrustify's `mod_full_brace_for = add`). Default false.
    pub add_braces_to_for: bool,
}

impl Default for BraceConfig {
    fn default() -> Self {
        Self {
            style: BraceStyle::Kr,
            cuddle_else: true,
            cuddle_catch: true,
            collapse_empty_body: true,
            expand_large_initializers: true,
            fn_brace_newline: true,
            extern_c_brace: ExternCBrace::ForceSameLine,
            add_braces_to_if: false,
            add_braces_to_while: false,
            add_braces_to_for: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BraceStyle {
    /// Opening brace at end of the same line: `if (cond) {`
    Kr,
    /// Opening brace on its own line.
    Allman,
    /// Like K&R but `else`/`catch` start on a new line.
    Stroustrup,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExternCBrace {
    /// Always place `{` on the same line as `extern "C"` (Google/LLVM style).
    #[default]
    ForceSameLine,
    /// Leave the brace wherever the source has it (matches uncrustify with no
    /// `nl_extern_brace` rule).
    Preserve,
}

// ── Spacing ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SpacingConfig {
    pub space_before_call_paren: bool,
    pub space_before_keyword_paren: bool,
    pub space_after_comma: bool,
    pub space_around_binary_ops: bool,
    pub space_inside_parens: bool,
    pub space_inside_brackets: bool,
    pub space_after_cast: bool,
    pub pointer_align: PointerAlign,
    pub space_inside_angle_brackets: bool,
    /// Align trailing `//` comments across consecutive lines that all carry a
    /// trailing comment.  0 = disabled; any positive value enables alignment.
    pub align_right_cmt_span: usize,
    /// Minimum number of spaces between code and an aligned trailing comment.
    /// Defaults to 1; increase to 2 or 3 to match uncrustify-style wider gaps.
    pub align_right_cmt_gap: usize,
    /// Controls which trailing comments are normalized to `align_right_cmt_gap`
    /// spaces.
    /// `groups` (default) — only multi-line groups are aligned; a lone trailing
    /// comment on a single line keeps the one space the formatter emits.
    /// Matches uncrustify's default behaviour of leaving single comments alone.
    /// `all` — every trailing comment (including single-line) is padded to at
    /// least `align_right_cmt_gap` spaces.
    pub align_right_cmt_style: AlignCmtStyle,
    /// Align `=` signs across consecutive enum value lines.
    /// 0 = disabled; any positive value enables alignment.
    pub align_enum_equ_span: usize,
    /// Align trailing `/**<` Doxygen member comments across consecutive struct
    /// member lines that all carry such a comment.  0 = disabled.
    pub align_doxygen_cmt_span: usize,
}

impl Default for SpacingConfig {
    fn default() -> Self {
        Self {
            space_before_call_paren: false,
            space_before_keyword_paren: true,
            space_after_comma: true,
            space_around_binary_ops: true,
            space_inside_parens: false,
            space_inside_brackets: false,
            space_after_cast: false,
            pointer_align: PointerAlign::Name,
            space_inside_angle_brackets: false,
            align_right_cmt_span: 3,
            align_right_cmt_gap: 3,
            align_right_cmt_style: AlignCmtStyle::Groups,
            align_enum_equ_span: 1,
            align_doxygen_cmt_span: 1,
        }
    }
}

/// Controls spacing around `*` and `&` in pointer/reference declarations.
///
/// Only applies when the `*`/`&` is detected as a declarator (heuristic:
/// preceded by a type keyword, another `*`/`&`, or `)` for casts).
/// Multiplication and dereference are not affected.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PointerAlign {
    /// `int* p` — star/amp attached to the type.
    Type,
    /// `int *p` — star/amp attached to the name.
    Name,
    /// `int * p` — star/amp centred between type and name.
    #[default]
    Middle,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AlignCmtStyle {
    /// Only align multi-line groups; single trailing comments keep 1 space.
    /// Matches uncrustify's default behaviour.
    #[default]
    Groups,
    /// Normalize every trailing comment (single or group) to at least
    /// `align_right_cmt_gap` spaces.
    All,
}

// ── Newlines ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NewlineConfig {
    pub style: NewlineStyle,
    pub max_blank_lines: u8,
    pub final_newline: bool,
    /// Insert a blank line between the leading variable-declaration block and
    /// the first statement in a function body (analogous to uncrustify's
    /// `nl_func_var_def_blk`).
    pub blank_line_after_var_decl_block: bool,
    /// Insert a blank line immediately after the opening `{` of function
    /// bodies and control-flow blocks (analogous to uncrustify's
    /// `nl_after_brace_open`).
    pub blank_line_after_open_brace: bool,
    /// When a standalone `//` comment immediately follows a `{`, `}`, or `;`
    /// (with no intervening blank lines), hoist it to the end of that
    /// preceding line as a trailing inline comment.
    pub merge_line_comment: bool,
}

impl Default for NewlineConfig {
    fn default() -> Self {
        Self {
            style: NewlineStyle::Lf,
            max_blank_lines: 2,
            final_newline: true,
            blank_line_after_var_decl_block: true,
            blank_line_after_open_brace: false,
            merge_line_comment: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NewlineStyle {
    Lf,
    Crlf,
    Native,
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let toml = r#"
[indent]
style = "spaces"
width = 4
indent_switch_case = true
indent_goto_labels = false

[braces]
style = "kr"
cuddle_else = true
cuddle_catch = true
collapse_empty_body = true
expand_large_initializers = false
fn_brace_newline = true
extern_c_brace = "preserve"
add_braces_to_if    = true
add_braces_to_while = false
add_braces_to_for   = false

[spacing]
space_before_call_paren    = false
space_before_keyword_paren = true
space_after_comma          = true
space_around_binary_ops    = true
space_inside_parens        = false
space_inside_brackets      = false
space_after_cast           = false
pointer_align              = "middle"
space_inside_angle_brackets = false
align_right_cmt_span       = 3
align_right_cmt_gap        = 3
align_right_cmt_style      = "all"
align_enum_equ_span        = 1
align_doxygen_cmt_span     = 1

[newlines]
style           = "lf"
max_blank_lines = 2
final_newline   = true
blank_line_after_var_decl_block = true
blank_line_after_open_brace     = false
merge_line_comment              = false
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.indent.width, 4);
        assert!(!cfg.indent.indent_goto_labels);
        assert_eq!(cfg.braces.style, BraceStyle::Kr);
        assert_eq!(cfg.braces.extern_c_brace, ExternCBrace::Preserve);
        assert!(cfg.braces.add_braces_to_if);
        assert!(!cfg.braces.add_braces_to_while);
        assert!(!cfg.braces.add_braces_to_for);
        assert_eq!(cfg.spacing.align_right_cmt_style, AlignCmtStyle::All);
        assert!(cfg.spacing.space_before_keyword_paren);
        assert!(!cfg.spacing.space_before_call_paren);
        assert_eq!(cfg.newlines.max_blank_lines, 2);
        assert!(cfg.ignore.patterns.is_empty());
    }

    #[test]
    fn default_config_valid() {
        let cfg = Config::default();
        assert_eq!(cfg.indent.style, IndentStyle::Spaces);
        assert_eq!(cfg.indent.width, 4);
        assert_eq!(cfg.newline_str(), "\n");
    }
}
