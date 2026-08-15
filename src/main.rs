use conjure::cli;
use miette::Result;

pub mod conjure;

fn main() -> Result<()> {
  cli::run()
}
