use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "deductive-check", about = "Deductive verification of code equivalence")]
struct Cli {
    #[arg(short, long)]
    input: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _cli = Cli::parse();
    println!("deductive-check: not yet implemented");
    Ok(())
}