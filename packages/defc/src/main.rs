mod build;

use std::path::PathBuf;

/// Fable def compiler
#[derive(argh::FromArgs)]
struct Args {
    /// input directory containing .def, .tpl, and .h files
    #[argh(positional)]
    source: PathBuf,

    /// output directory for .bin files
    #[argh(positional)]
    output: PathBuf,
}

fn main() {
    let args: Args = argh::from_env();

    if let Err(e) = build::build_all(&args.source, &args.output) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
