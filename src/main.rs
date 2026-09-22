use babeldiff::analyze::{self, CppFinder, NoFinder};
use babeldiff::git::{Git, RepoFinder};
use babeldiff::input::ChangeSet;
use babeldiff::render::{self, Layout, RenderOptions};
use clap::{Parser, Subcommand, ValueEnum};
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

/// Line up C++ removed by a change with the Rust that replaces it, and check
/// that comments, error returns, locks, calls and control flow correspond.
///
/// Exit status is 0 when no issues are found, 1 when there are issues, and 2
/// on error.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,

    /// Output layout.
    #[arg(long, value_enum, default_value_t = LayoutArg::SideBySide, global = true)]
    layout: LayoutArg,

    /// Total output width for the side-by-side layout (default: $COLUMNS or 160).
    #[arg(long, global = true)]
    width: Option<usize>,

    /// Only show rows within N rows of a difference.
    #[arg(long, short = 'U', global = true)]
    context: Option<usize>,

    /// Print only the per-function summaries and findings.
    #[arg(long, global = true)]
    summary: bool,

    /// Force a pairing, as CPP_NAME=RUST_NAME (qualified or unqualified). Repeatable.
    #[arg(long = "pair", value_name = "CPP=RUST", global = true)]
    pairs: Vec<String>,

    /// Minimum fraction of a function's lines the change must touch for the
    /// function to be compared.
    #[arg(long, default_value_t = babeldiff::DEFAULT_MIN_CHANGED, global = true)]
    min_changed: f64,

    /// Don't search the repository for unchanged C++ that corresponds to
    /// otherwise unpaired Rust.
    #[arg(long, global = true)]
    no_search: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Compare a git commit (REV^..REV) or range (A..B). Defaults to HEAD.
    Git {
        #[arg(default_value = "HEAD")]
        rev: String,
        /// Repository directory.
        #[arg(short = 'C', long, default_value = ".")]
        repo: PathBuf,
    },
    /// Compare a unified diff or `git format-patch` file ("-" or omitted for stdin).
    Patch {
        file: Option<PathBuf>,
        /// A git repository to read full file contents from (by the blob ids
        /// in the patch) and to search for unchanged C++.
        #[arg(short = 'C', long)]
        repo: Option<PathBuf>,
        /// Revision to search for unchanged C++ (the patch's parent).
        #[arg(long, default_value = "HEAD")]
        base: String,
    },
    /// Compare every function in the given C++ and Rust files.
    Files {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum LayoutArg {
    SideBySide,
    Stacked,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(issues) => {
            if issues > 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("babeldiff: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> Result<usize, String> {
    let mut forced = Vec::new();
    for p in &cli.pairs {
        let (c, r) = p
            .split_once('=')
            .ok_or_else(|| format!("--pair expects CPP=RUST, got {p:?}"))?;
        forced.push((c.to_string(), r.to_string()));
    }
    let (cs, mut finder): (ChangeSet, Box<dyn CppFinder>) = match &cli.command {
        Cmd::Git { rev, repo } => {
            let git = Git::new(repo);
            let (base, head) = Git::range(rev);
            let cs = git.changeset(&base, &head)?;
            let finder: Box<dyn CppFinder> = if cli.no_search {
                Box::new(NoFinder)
            } else {
                Box::new(RepoFinder::new(git, base))
            };
            (cs, finder)
        }
        Cmd::Patch { file, repo, base } => {
            let text = match file.as_deref() {
                None => read_stdin()?,
                Some(p) if p.as_os_str() == "-" => read_stdin()?,
                Some(p) => {
                    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?
                }
            };
            let files = babeldiff::patch::parse(&text);
            let git = repo.as_ref().map(Git::new).filter(Git::is_repo);
            let cs = match &git {
                Some(g) => ChangeSet::from_patch(&files, &mut |id| g.blob(id)),
                None => ChangeSet::from_patch(&files, &mut |_| None),
            };
            let finder: Box<dyn CppFinder> = match git {
                Some(g) if !cli.no_search => Box::new(RepoFinder::new(g, base.clone())),
                _ => Box::new(NoFinder),
            };
            (cs, finder)
        }
        Cmd::Files { files } => {
            let mut loaded = Vec::new();
            for p in files {
                let text =
                    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
                loaded.push((p.display().to_string(), text));
            }
            (ChangeSet::from_files(&loaded), Box::new(NoFinder))
        }
    };
    let report = babeldiff::run_with(
        &cs,
        &analyze::Options::default(),
        finder.as_mut(),
        cli.min_changed,
        forced,
    );
    let width = cli
        .width
        .or_else(|| std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()))
        .unwrap_or(160);
    let opts = RenderOptions {
        layout: match cli.layout {
            LayoutArg::SideBySide => Layout::SideBySide,
            LayoutArg::Stacked => Layout::Stacked,
        },
        width,
        context: cli.context,
        summary_only: cli.summary,
    };
    print!("{}", render::render(&report, &opts));
    Ok(report.issues())
}

fn read_stdin() -> Result<String, String> {
    let mut s = String::new();
    std::io::stdin()
        .read_to_string(&mut s)
        .map_err(|e| format!("reading stdin: {e}"))?;
    Ok(s)
}
