#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

//! Configuration for the Mk1 sequencer.

use clap::{
    Parser,
    builder::{
        Styles,
        styling::{AnsiColor, Color, Style},
    },
};

mod chain;
use chain::{ChainOpts, ContractAddresses, L1Opts, L2Opts};

mod operator;
use operator::OperatorOpts;

mod telemetry;
use telemetry::TelemetryOpts;

mod preconf;
use preconf::PreconfOpts;

/// CLI options for the Mk1 sequencer.
#[derive(Debug, Clone, Parser)]
#[command(author, version, styles = cli_styles(), about)]
pub struct Opts {
    /// A unique name for this MK1 instance, used in metrics and logs
    #[clap(long, env = "MK1_INSTANCE_NAME", default_value = "mk1")]
    pub instance_name: String,
    /// L1-related configuration options
    #[clap(flatten)]
    pub l1: L1Opts,
    /// L2-related configuration options
    #[clap(flatten)]
    pub l2: L2Opts,
    /// Preconfirmation-related configuration options
    #[clap(flatten)]
    pub preconf: PreconfOpts,
    /// Operator-related configuration options
    #[clap(flatten)]
    pub operator: OperatorOpts,
    /// The contract addresses required to run the sequencer.
    #[clap(flatten)]
    pub contracts: ContractAddresses,
    /// The configuration for the chain.
    #[clap(flatten)]
    pub chain: ChainOpts,
    /// Telemetry-related configuration options
    #[clap(flatten)]
    pub telemetry: TelemetryOpts,
}

/// Styles for the CLI.
const fn cli_styles() -> Styles {
    Styles::styled()
        .usage(Style::new().bold().underline().fg_color(Some(Color::Ansi(AnsiColor::Yellow))))
        .header(Style::new().bold().underline().fg_color(Some(Color::Ansi(AnsiColor::Yellow))))
        .literal(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green))))
        .invalid(Style::new().bold().fg_color(Some(Color::Ansi(AnsiColor::Red))))
        .error(Style::new().bold().fg_color(Some(Color::Ansi(AnsiColor::Red))))
        .valid(Style::new().bold().underline().fg_color(Some(Color::Ansi(AnsiColor::Green))))
        .placeholder(Style::new().fg_color(Some(Color::Ansi(AnsiColor::White))))
}

#[cfg(test)]
mod tests {
    use super::Opts;

    #[test]
    fn test_verify_cli() {
        use clap::CommandFactory;
        Opts::command().debug_assert()
    }
}
