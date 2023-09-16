use std::path::PathBuf;

use clap::{Parser, Subcommand};

pub const WEB_GARNISH_SERVE_PATH: &str = "WEB_GARNISH_SERVE_PATH";

#[derive(Debug, Parser)]
#[command(name = "web-garnish")]
#[command(about = "Start a web server to serve garnish files.", long_about = None)]
pub struct ServerArgs {
    #[command(subcommand)]
    pub command: ServerSubCommand,

    #[arg(long, env=WEB_GARNISH_SERVE_PATH, verbatim_doc_comment)]
    pub serve_path: Option<PathBuf>,

    /// Route to execute when not serving entire app.
    #[arg(long, verbatim_doc_comment)]
    pub route: Option<String>,

    /// Where to write output. If not provided output will go to stdout.
    #[arg(long, verbatim_doc_comment)]
    pub output_path: Option<PathBuf>
}

#[derive(Debug, Subcommand)]
pub enum ServerSubCommand {
    #[command()]
    Serve,

    /// Builds expression and writes build data to output.
    #[command()]
    Dump,
}