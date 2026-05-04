use clap::Parser;
use std::path::{Path, PathBuf};

mod config;
mod error;
mod formatter;
mod lexer;
mod token;

use config::Config;

#[derive(Parser)]
#[command(
    name = "funky",
    version,
    about = "C/C++ formatter with Unicode support"
)]
struct Cli {
    /// Source file(s) to format. Use `-` to read from stdin.
    #[arg(required = true)]
    files: Vec<PathBuf>,

    /// Path to TOML config file (default: look for funky.toml in cwd).
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Edit file(s) in place instead of writing to stdout.
    #[arg(short = 'i', long)]
    in_place: bool,

    /// Check mode: exit 1 if any file would change; do not write.
    #[arg(long)]
    check: bool,

    /// Print the raw token stream and exit (for debugging).
    #[arg(long, hide = true)]
    dump_tokens: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.check && cli.in_place {
        anyhow::bail!("--check and --in-place are mutually exclusive");
    }

    let config = load_config(cli.config.as_deref())?;

    let mut any_changed = false;

    for path in &cli.files {
        let source = read_source(path)?;
        let (tokens, warnings) = lexer::tokenize(&source, path.display().to_string())?;
        for w in &warnings {
            eprintln!("warning: {w}");
        }

        if cli.dump_tokens {
            for tok in &tokens {
                println!("{:?} {:?}", tok.kind, tok.lexeme);
            }
            continue;
        }

        let formatted = formatter::format(&tokens, &config)?;

        if cli.check {
            if source != formatted {
                eprintln!("{}: would reformat", path.display());
                any_changed = true;
            }
        } else if cli.in_place {
            if source != formatted {
                std::fs::write(path, formatted.as_bytes())
                    .map_err(|e| anyhow::anyhow!("could not write {}: {}", path.display(), e))?;
            }
        } else {
            print!("{}", formatted);
        }
    }

    if cli.check && any_changed {
        std::process::exit(1);
    }

    Ok(())
}

fn load_config(explicit: Option<&Path>) -> anyhow::Result<Config> {
    if let Some(path) = explicit {
        return Ok(Config::load(path)?);
    }
    let default_path = Path::new("funky.toml");
    if default_path.exists() {
        return Ok(Config::load(default_path)?);
    }
    Ok(Config::default())
}

fn read_source(path: &Path) -> anyhow::Result<String> {
    if path == Path::new("-") {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| anyhow::anyhow!("could not read stdin: {}", e))?;
        return Ok(s);
    }
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("could not read {}: {}", path.display(), e))?;
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("{}: not valid UTF-8", path.display()))
}
