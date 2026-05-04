# funky

C/C++ code formatter written in Rust. Handles Unicode (Chinese comments, etc.), configured via `funky.toml`.

## Task tracking

This repo uses `todo-sqlite-cli` (DB resolved via `.todo-sqlite-cli` marker).

**Always check before coding:**
```
todo-sqlite-cli next        # the one task to work on now
todo-sqlite-cli list        # full active backlog
todo-sqlite-cli show <id>   # details for a specific task
```

**When working:**
```
todo-sqlite-cli start <id>  # before touching code
todo-sqlite-cli done <id>   # after committing
```

## Module structure

| File | Purpose |
|------|---------|
| `src/main.rs` | CLI entry point (clap 4). `--in-place`, `--check`, `--config`, `--dump-tokens`. |
| `src/config.rs` | `Config` struct deserialized from `funky.toml` via serde. `BraceStyle`, `IndentStyle`, `SpacingConfig`, `NewlineConfig`. All keys optional with defaults. |
| `src/token.rs` | `TokenKind` enum — **no payload**, text lives in `Token.lexeme`. `Token<'src>` borrows the source string. Helper methods: `is_control_kw`, `is_any_kw`, `ends_expr`, `is_binary_op`. |
| `src/lexer.rs` | `Cursor` (char iterator + byte position), `Lexer`, `tokenize()`. Handles all C/C++ token types including Unicode identifiers/comments, string prefixes (`L""` `u""` `u8""` `R"()"`), hex/binary/octal/float literals, multi-line `#define`. |
| `src/formatter.rs` | `Fmt` struct walks the token stream and rebuilds the source with correct whitespace. `format()` is the public entry point. |
| `src/error.rs` | `FunkyError` (thiserror). `Lex`, `Format`, `Config`, `Io`, `NotUtf8` variants. |

## Key formatter invariants

**`skip_next_newline` flag** — after emitting a formatter-owned `\n` (for `;` or `}`), this flag tells `skip_ws()` to discard the corresponding Newline token from the lexer stream so it isn't double-counted as a blank line.

**`BraceCtx` stack** — tracks what opened each `{`: `Block`, `Type`, `Namespace`, `Function`, `Other`. Used to decide brace placement, whether a `;` follows `}`, and `typedef struct { } Name` (name stays on same line as `}`).

**Preprocessor is opaque** — `PreprocLine` tokens are passed through verbatim (only newline style is normalized). Do not apply spacing rules inside them.

**Whitespace/Newline tokens are preserved** — the lexer keeps them so `skip_ws()` can count blank lines. The formatter discards them and manages all whitespace itself.

## Adding a new formatting rule

1. If it needs a new config key, add the field to the relevant struct in `config.rs` with a `default_*` function and update the sample `funky.toml`.
2. Identify the token(s) involved in `token.rs` (add a new `TokenKind` variant if needed — keep it payload-free).
3. Add a match arm in `Fmt::format()` in `formatter.rs`, or extend `needs_space()` for pure spacing rules.
4. Add a `#[cfg(test)]` test in the same file.

## Adding a new token kind

1. Add the variant to `TokenKind` in `src/token.rs` (derive `Copy` is free — no payload).
2. Add a scan case in `Lexer::next_token()` in `src/lexer.rs`.
3. Handle or explicitly ignore it in `Fmt::format()` (the `_ =>` arm is the safe fallback).

## Running

```
cargo build
cargo test
cargo run -- path/to/file.c
cargo run -- --in-place path/to/file.c
cargo run -- --check path/to/file.c
cargo run -- --dump-tokens path/to/file.c   # debug: print token stream
```

The binary picks up `funky.toml` from the current directory automatically.
