use std::path::PathBuf;

use clap::Parser;

pub const WEB_GARNISH_SERVE_PATH: &str = "WEB_GARNISH_SERVE_PATH";

#[derive(Debug, Parser)]
#[command(name = "web-garnish")]
#[command(about = "Start a web server to serve garnish files.", long_about = None)]
pub struct ServerArgs {
    #[arg(long, env=WEB_GARNISH_SERVE_PATH, verbatim_doc_comment)]
    pub serve_path: Option<PathBuf>,
}
