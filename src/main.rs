use clap::Parser;

fn main() {
    let cli = fragpipe::Cli::parse();
    if let Err(err) = fragpipe::run(cli) {
        eprintln!("{err:#}");
        let code = err
            .downcast_ref::<fragpipe::android_worker::AndroidCommandError>()
            .map_or(1, |error| error.exit_code());
        std::process::exit(code);
    }
}
