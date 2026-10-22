use clap::Parser;

#[derive(Parser)]
#[command(
    name = "prime-radiant",
    version,
    about = "Find and read any past AI session from one always-on hub.",
    arg_required_else_help = true
)]
struct Cli {}

fn main() {
    Cli::parse();
}
