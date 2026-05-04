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
    pub ignore: IgnoreConfig,
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
}

impl Default for IndentConfig {
    fn default() -> Self {
        Self {
            style: IndentStyle::Spaces,
            width: 4,
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
}

impl Default for BraceConfig {
    fn default() -> Self {
        Self {
            style: BraceStyle::Kr,
            cuddle_else: true,
            cuddle_catch: true,
            collapse_empty_body: true,
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
            pointer_align: PointerAlign::Middle,
            space_inside_angle_brackets: false,
            align_right_cmt_span: 0,
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
}

impl Default for NewlineConfig {
    fn default() -> Self {
        Self {
            style: NewlineStyle::Lf,
            max_blank_lines: 2,
            final_newline: true,
            blank_line_after_var_decl_block: false,
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

[braces]
style = "kr"
cuddle_else = true
cuddle_catch = true
collapse_empty_body = true

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
align_right_cmt_span       = 0

[newlines]
style           = "lf"
max_blank_lines = 2
final_newline   = true
blank_line_after_var_decl_block = false
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.indent.width, 4);
        assert_eq!(cfg.braces.style, BraceStyle::Kr);
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
