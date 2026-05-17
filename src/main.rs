use clap::Parser;

fn main() {
    let cli = fragpipe::Cli::parse();
    if let Err(err) = fragpipe::run(cli) {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}
