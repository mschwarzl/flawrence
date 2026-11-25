use clap::{Parser, Subcommand};
use flawrence_index::{CodebaseIndex, SymbolKind, fmt};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "flawrence-index",
    about = "Debug CLI for the Flawrence code index"
)]
struct Cli {
    #[arg(short = 'C', long = "directory", default_value = ".")]
    directory: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Summarize,
    Find { name: String },
    Source { name: String, file: String },
    Outline { file: String, kind: Option<String> },
}

fn main() {
    let cli = Cli::parse();
    let dir = if cli.directory.is_dir() {
        cli.directory.canonicalize().unwrap_or(cli.directory)
    } else {
        eprintln!("Error: '{}' is not a directory", cli.directory.display());
        std::process::exit(1);
    };
    eprintln!("Indexing {}...", dir.display());
    let start = Instant::now();
    let index = CodebaseIndex::build(&dir);
    let elapsed = start.elapsed();
    let summary = index.summarize();
    eprintln!(
        "Indexed {} files ({} defs) in {:.2}s\n",
        summary.file_count,
        summary.definition_count,
        elapsed.as_secs_f64()
    );
    let root = index.root();
    match cli.command {
        Command::Summarize => print!("{}", fmt::format_summary(&summary)),
        Command::Find { name } => {
            let results = index.find_symbol(&name);
            if results.is_empty() {
                println!("No definitions found for \"{}\".", name);
            } else {
                println!(
                    "Definitions matching \"{}\" ({} results):",
                    name,
                    results.len()
                );
                print!("{}", fmt::format_find_results(&results, root));
            }
        }
        Command::Source { name, file } => {
            if !index.has_file(Path::new(&file)) {
                eprintln!("Error: file '{}' not found in index", file);
                std::process::exit(1);
            }
            match index.get_body(&name, Path::new(&file)) {
                Some(src) => print!("{}", fmt::format_source(&src, root)),
                None => {
                    eprintln!("Error: symbol not found");
                    std::process::exit(1);
                }
            }
        }
        Command::Outline { file, kind } => {
            if !index.has_file(Path::new(&file)) {
                eprintln!("Error: file '{}' not found in index", file);
                std::process::exit(1);
            }
            let kind_filter = kind.as_deref().and_then(|s| s.parse::<SymbolKind>().ok());
            let results = index.list_symbols(Path::new(&file), kind_filter.as_ref());
            println!("Outline of {} ({} definitions):", file, results.len());
            print!("{}", fmt::format_outline(&results));
        }
    }
}
