# funky

A fast C/C++ code formatter written in Rust. Handles Unicode source files (Chinese comments, CJK identifiers, etc.) and is configured via a `funky.toml` file.

## Features

- K&R, Allman, and Stroustrup brace styles
- Pointer alignment (`type`, `name`, `middle`)
- Trailing comment column-alignment (`//`, `/**<` Doxygen)
- Enum `=` sign alignment
- Configurable blank-line rules (after var-decl blocks, after `{`, etc.)
- Unicode-safe lexer — full-source pass, no silent truncation
- `--check` mode for CI / pre-commit hooks
- `--recursive` directory walk with glob-based ignore patterns
- stdin/stdout pipeline support (`-` as a file argument)

## Installation

```sh
cargo install --path .
```

Or build a release binary directly:

```sh
cargo build --release
# binary at target/release/funky
```

## Usage

```
funky [OPTIONS] <FILES>...
```

| Argument / Option | Description |
|---|---|
| `<FILES>...` | Files or directories to format. Use `-` for stdin. |
| `-i`, `--in-place` | Edit files in place. |
| `--check` | Exit 1 if any file would change; no writes. |
| `-r`, `--recursive` | Recurse into directories (C/C++ extensions only). |
| `-c`, `--config <FILE>` | Explicit config file (default: `funky.toml` in cwd). |
| `--dump-tokens` | Print the token stream and exit (debug). |
| `-h`, `--help` | Print help. |
| `-V`, `--version` | Print version. |

`--check` and `--in-place` are mutually exclusive.

### Examples

```sh
# Format a single file to stdout
funky src/foo.c

# Edit in place
funky -i src/foo.c src/bar.h

# Check a whole tree (CI)
funky --check -r src/

# Pipe through stdin
cat ugly.c | funky - > pretty.c

# Use an explicit config
funky -c /etc/funky.toml -i src/
```

## Configuration

Place a `funky.toml` in your project root (or pass `--config`). All keys are optional; defaults are shown below.

```toml
[indent]
style               = "spaces"  # "spaces" | "tabs"
width               = 4         # spaces per level (ignored for tabs)
indent_switch_case  = true      # indent case/default labels inside switch
indent_goto_labels  = false     # false: goto labels at column 0

[braces]
style               = "kr"      # "kr" | "allman" | "stroustrup"
cuddle_else         = false     # false: } \n else {   true: } else {
cuddle_catch        = false     # false: } \n catch (  true: } catch (
collapse_empty_body = true      # while (x) { } → while (x) {}
fn_brace_newline    = true      # function-def { always on its own line
extern_c_brace      = "force_same_line"  # "force_same_line" | "preserve"
nl_brace_else       = true      # true: newline before else/else-if
add_braces_to_if    = true      # add { } around braceless if bodies
add_braces_to_while = true      # add { } around braceless while bodies
add_braces_to_for   = true      # add { } around braceless for bodies

[spacing]
space_before_call_paren     = false      # foo( vs foo (
space_before_keyword_paren  = true       # if ( vs if(
space_after_comma           = true
space_around_binary_ops     = true
space_inside_parens         = "preserve" # "preserve" | "add" | "remove"
space_inside_brackets       = "preserve" # "preserve" | "add" | "remove"
space_after_cast            = "preserve" # "preserve" | "add" | "remove"
pointer_align               = "name"     # "type" | "name" | "middle"
space_inside_angle_brackets = false      # vector<int> vs vector< int >
align_right_cmt_span        = 3          # 0=off; column-align trailing // comments
align_right_cmt_gap         = 1          # minimum spaces before aligned comment
align_right_cmt_style       = "groups"   # "groups" | "all"
align_enum_equ_span         = 1          # 0=off; align enum = signs
align_doxygen_cmt_span      = 1          # 0=off; column-align /**< comments
align_on_tabstop            = true       # snap alignment to indent-width multiples

[newlines]
style                           = "lf"   # "lf" | "crlf" | "native"
max_blank_lines                 = 2
final_newline                   = true
blank_line_after_var_decl_block = true
blank_line_after_open_brace     = false
merge_line_comment              = false
nl_brace_else                   = true   # newline before else (mirrors braces setting)

[preprocessor]
pp_indent           = false  # indent #if/#else/#endif nesting
endif_comment_space = 1      # spaces between #endif and trailing /* comment */

[comments]
normalize_block_comment_closing = false  # false: preserve bare */  true: rewrite to ` */`

[ignore]
patterns = ["vendor/**", "third_party/**", "*.pb.h"]
```

### Brace styles

| Style | Appearance |
|---|---|
| `kr` | `if (cond) {` — opening brace at end of line |
| `allman` | Opening brace on its own line |
| `stroustrup` | Like K&R, but `else`/`catch` start on a new line |

`fn_brace_newline = true` forces function-definition opening braces to their own line regardless of the global `style`, matching uncrustify's `nl_fdef_brace = add`.

### Pointer alignment

| Value | Example |
|---|---|
| `"type"` | `int* p` |
| `"name"` | `int *p` |
| `"middle"` | `int * p` |

Only declarator `*`/`&` are affected; multiplication and pointer dereference are left alone.

### Ignore patterns

Patterns are matched against paths **relative to the walked directory root** using glob syntax:

```toml
[ignore]
patterns = ["vendor/**", "build/**", "*.pb.h", "generated_*.c"]
```

## Pre-commit hook

```yaml
# .pre-commit-config.yaml
repos:
  - repo: local
    hooks:
      - id: funky
        name: funky format check
        language: system
        entry: funky --check
        types: [c, c++]
```

## Comparison with uncrustify

funky is designed to produce output compatible with common uncrustify configurations.
Known intentional differences and configurable deviations are documented in
[DEVIATIONS.md](DEVIATIONS.md).

## License

MIT — see [LICENSE](LICENSE).
