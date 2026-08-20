use conjure::cli;
use miette::Result;

pub mod conjure;

fn main() -> Result<()> {
  miette::set_hook(Box::new(|_| {
    Box::new(
      miette::MietteHandlerOpts::new()
        .width(80)
        .unicode(true)
        .wrap_lines(true)
        .tab_width(0)
        .context_lines(3)
        .show_related_errors_as_siblings()
        .with_cause_chain()
        .build(),
    )
  }))
  .expect("Failed to set miette hook");

  cli::run()
}
