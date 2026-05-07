# Funky vs Uncrustify Deviations

Documents known behavioral differences between funky (default config) and uncrustify
with `~/data/d_lib_common/conf/defaults.cfg`. Updated after each comparison session.

Comparison target: C/C++ source files in `~/data/d_lib_mqtt_serial_lite` (May 2026).

---

## By Design

Funky intentionally differs. The funky behavior is considered correct; uncrustify's
output is a side-effect of it having no applicable rule (passive preservation) rather
than an active style choice.

| Behavior | Funky | Uncrustify | Rationale |
|---|---|---|---|
| Multi-line function call: extra whitespace after `(` | Normalizes to no extra space; aligns continuation lines to column after `(` | Preserves source's extra spaces; aligns continuations to first arg | Source had non-standard `(  arg` — funky normalizes, uncrustify passively preserves. Funky output is consistent and correct. |
| Single trailing comment spacing | Emits 1 space between code and comment | Preserves whatever spacing the source had | Uncrustify has no normalization rule; funky's 1-space is deterministic. Use `align_right_cmt_style = "all"` if wider gap is wanted. |
| `blank_line_after_var_decl_block`: nested block scope | No blank line after var-decl inside `for`/`if`/macro-loop bodies | `nl_func_var_def_blk` adds blank line even inside nested blocks | Funky deliberately limits the rule to the leading declaration run at function scope. Nested-block decls rarely need the visual separator; false-positives outweigh benefit. |
| `blank_line_after_var_decl_block`: `struct { } var;` in decl block | No blank line before an anonymous-struct type declaration | Adds blank line before `struct { ... } name;` even though it is still a declaration | Funky treats `struct` as a declaration keyword; the whole group including anonymous-struct decls is one cohesive block. Uncrustify's split is a heuristic, not a style invariant. |
| `blank_line_after_var_decl_block`: function-pointer declarations | No blank line before `int (*fp)(...)` | Adds blank line before function-pointer decls, treating them as a separate tier | Funky treats any type-starting token (including `int` in `int (*fp)`) as a declaration; the block is not split. Uncrustify's behaviour appears to be a side-effect of its parser treating `(` in a declaration specially. |
| Extra blank line before section block comments between functions | Does not add extra blank line | Adds one extra blank line before `/* … */` section comments that follow `}` | Separate uncrustify rule (`nl_min_blankline_before_block_comment` equivalent). Not related to `nl_func_var_def_blk`. Not implemented in funky; affects ~15 files in the hostap comparison. |
| Tab→space inside `#define` macro bodies | Passes `PreprocLine` tokens verbatim; tabs inside macros are not touched | Normalizes whitespace inside macro bodies, converting tabs to spaces | Funky treats all preprocessor lines as opaque by design. Altering whitespace inside a macro body could break stringification (`#`), token-pasting (`##`), or alignment-sensitive macros. The verbatim pass-through is intentional and correct. |
| Block comment `**`-continuation line indentation | Preserves source indentation on `**` continuation lines | Re-indents `**` continuation lines to match the enclosing block's indent level | Funky treats block comment interiors as opaque content (analogous to preprocessor lines). Rewriting internal `**` indentation would risk misaligning comments written with intentional column alignment. Passive preservation is safer. |
| `#endif /* ... */` comment spacing | Always emits 1 space between `#endif` and `/*` comment | Uses 2 spaces for some `#endif` lines (appears to depend on whether the enclosing `#ifdef` block contains a `#else` or other complex nesting — no documented rule) | Uncrustify's behaviour is internally inconsistent and not driven by a documented config option. Funky's 1-space is deterministic. Use `preprocessor.endif_comment_space = 2` to force 2 everywhere. |

---

## Configurable to Match Uncrustify

Funky defaults differ but can be configured to match.

| Behavior | Funky default | Config key | Value to match uncrustify |
|---|---|---|---|
| `extern "C" {` brace placement | Forces `{` on same line (Google/LLVM style) | `braces.extern_c_brace` | `"preserve"` — leaves brace wherever source has it, same as uncrustify with no `nl_extern_brace` rule |
| Single trailing comment gap | 1 space | `spacing.align_right_cmt_style` | `"all"` — pads every trailing comment to `align_right_cmt_gap` spaces (though still won't exactly match arbitrary source column widths that uncrustify preserves) |

---

## Fixed

Issues found during comparison that were resolved.

| Behavior | Fix | Commit area |
|---|---|---|
| `*p++ = x` missing space before `=` | `PlusPlus`/`MinusMinus` "no space after unary" guard was suppressing binary op space. Fixed to fall through to binary-op spacing rules when next token is a binary operator. | `formatter.rs` |
| Binary `-` after `sizeof(x)` misclassified as unary — `sizeof(buf) -1` instead of `sizeof(buf) - 1` | Added `prev_is_sizeof_like()` guard; `sizeof`/`alignof`/`decltype`/`typeid` preceding `(` no longer mark the paren as a cast close | `formatter.rs` |
| `blank_line_after_var_decl_block`: trailing inline `/* comment */` after last decl suppresses blank line before following `#ifdef` | Fixed `flush_blank_lines` to require 2 newlines (not 1) when not at line-start, so the blank line appears after the comment line is terminated | `formatter.rs` |
| `blank_line_after_var_decl_block`: macro-defined function bodies at global scope (`SM_STATE(...)` pattern) not recognized as function bodies | Fixed LBrace/Semi/RBrace handlers to treat a top-level `Block` brace (stack was empty when opened) the same as `Function` for the var-decl rule | `formatter.rs` |

---

## Will Fix

Known gaps not yet addressed.

| Behavior | Funky | Uncrustify | Notes |
|---|---|---|---|
| Struct/union member declaration alignment | No alignment — members output at natural spacing | Aligns type, name, and initializer columns within a span (`align_var_def_span`) | Aesthetic only, no correctness impact. The trailing-comment alignment (`align_right_cmt_span`) combined with `align_on_tabstop = true` covers the most visible part (comment columns). True `align_var_def_span` (type+name column alignment) not yet implemented. |

---

## Comparison Notes

- **Trailing comment alignment (groups):** Funky aligns multi-line groups of trailing
  comments to a common column (`align_right_cmt_span > 0`). Uncrustify does the same
  with `align_right_cmt_span = 3` in defaults.cfg. Both produce matching output for
  multi-line groups.

- **Function argument alignment (complex):** Uncrustify's `align_on_tabstop = TRUE`
  combined with multi-line call continuation can produce tabstop-snapped alignment.
  Funky aligns strictly to the column after `(`. Not currently tracked as a gap since
  the difference only surfaces when the source already has non-standard spacing.
