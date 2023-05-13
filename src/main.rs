use std::env::current_dir;

use axum::{routing::get, Router};
use clap::Parser;

use crate::args::ServerArgs;

mod args;

#[tokio::main]
async fn main() -> Result<(), String> {
    let args = ServerArgs::parse();

    let serve_path = match args.serve_path {
        None => current_dir().or_else(|e| {
            Err(format!(
                "Could not get current working directory. Caused by {:?}",
                e
            ))
        })?,
        Some(p) => p,
    };

    // build our application with a single route
    let app = Router::new().route("/", get(|| async { "Hello, World!" }));

    // run it with hyper on localhost:3000
    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();

    Ok(())
}
