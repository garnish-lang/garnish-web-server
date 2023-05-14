use std::collections::HashMap;
use std::env::current_dir;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::response::Response;
use axum::{routing::get, Router};
use clap::Parser;
use hyper::StatusCode;
use log::{debug, error, info, trace, warn};
use serde::Deserialize;

use garnish_data::SimpleRuntimeData;
use garnish_lang_compiler::{build_with_data, lex, parse};
use garnish_lang_runtime::runtime_impls::SimpleGarnishRuntime;
use garnish_traits::{
    EmptyContext, GarnishLangRuntimeData, GarnishLangRuntimeState, GarnishRuntime,
};
use hypertext_garnish::Node;
use serde_garnish::GarnishDataDeserializer;

use crate::args::ServerArgs;

mod args;

pub const INCLUDE_PATTERN_DEFAULT: &str = "**/*.garnish";

#[derive(Clone)]
struct SharedState {
    base_runtime: SimpleGarnishRuntime<SimpleRuntimeData>,
    route_mapping: HashMap<String, usize>,
}

#[tokio::main]
async fn main() -> Result<(), String> {
    simple_logger::init_with_env().unwrap();

    let args = ServerArgs::parse();

    let mut serve_path = match args.serve_path {
        None => current_dir().or_else(|e| {
            Err(format!(
                "Could not get current working directory. Caused by {:?}",
                e
            ))
        })?,
        Some(p) => p,
    };

    let serve_path_str = match serve_path.to_str() {
        None => Err(format!(
            "Could not covert serve path to str. Path: {:?}",
            serve_path
        ))?,
        Some(s) => s.to_string(),
    };

    serve_path.push(INCLUDE_PATTERN_DEFAULT);

    let glob_pattern = match serve_path.to_str() {
        None => Err(format!(
            "Could not covert serve path to set. Path: {:?}",
            serve_path
        ))?,
        Some(s) => s,
    };

    let (oks, errs): (Vec<_>, Vec<_>) = glob::glob(glob_pattern)
        .or_else(|e| Err(e.to_string()))?
        .into_iter()
        .partition(|g| g.is_ok());

    for e in errs {
        error!("Error during glob: {:?}", e);
    }

    let paths = oks
        .into_iter()
        .map(|g| g.unwrap())
        .collect::<Vec<PathBuf>>();

    let (route_mapping, runtime) = create_runtime(paths, serve_path_str.as_str())?;
    let state = Arc::new(SharedState {
        route_mapping,
        base_runtime: runtime,
    });

    // build our application with a single route
    let app = Router::new()
        .route("/", get(handler))
        .route("/*path", get(handler))
        .with_state(state);

    // run it with hyper on localhost:3000
    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();

    Ok(())
}

async fn handler(
    State(state): State<Arc<SharedState>>,
    request: Request<Body>,
) -> Response<String> {
    let mut runtime = state.base_runtime.clone();

    let page = request.uri().path().trim().trim_matches('/').trim();
    let alt = match page.is_empty() {
        true => String::from("index"),
        false => [page, "index"].join("/"),
    };

    info!("Request for route \"{}\"", page);
    debug!("Checking mappings for \"{}\" and \"{}\"", page, alt);
    match state
        .route_mapping
        .get(page)
        .or_else(|| state.route_mapping.get(&alt))
    {
        None => {
            info!("No garnish mapping found for route \"{}\"", page);
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(String::new())
                .unwrap()
        }
        Some(start) => {
            match runtime.get_data_mut().set_instruction_cursor(*start) {
                Err(e) => {
                    error!("Failed to set instructor cursor: {:?}", e);
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(String::new())
                        .unwrap();
                }
                Ok(()) => (),
            }

            loop {
                match runtime.execute_current_instruction::<EmptyContext>(None) {
                    Err(e) => {
                        error!("Failed to execute: {:?}", e);
                        return Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body(String::new())
                            .unwrap();
                    }
                    Ok(data) => match data.get_state() {
                        GarnishLangRuntimeState::Running => (),
                        GarnishLangRuntimeState::End => break,
                    },
                }
            }

            let mut deserializer = GarnishDataDeserializer::new(runtime.get_data_mut());
            let result = match Node::deserialize(&mut deserializer) {
                Err(e) => {
                    error!(
                        "Failed to deserialize garnish data to HTML: {:?}",
                        e.message()
                    );
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(String::new())
                        .unwrap();
                }
                Ok(n) => n,
            };

            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/html")
                .body(result.to_string())
                .unwrap()
        }
    }
}

fn create_runtime(
    paths: Vec<PathBuf>,
    base_path: &str,
) -> Result<
    (
        HashMap<String, usize>,
        SimpleGarnishRuntime<SimpleRuntimeData>,
    ),
    String,
> {
    let mut data = SimpleRuntimeData::new();

    // maps expected http route to index of expression that will be executed when that route is requested
    let mut route_to_expression = HashMap::new();

    for path in paths {
        let route = path
            .strip_prefix(base_path)
            .and_then(|s| Ok(s.to_string_lossy().replace(".garnish", "")))
            .or_else(|e| Err(e.to_string()))?;

        debug!("Compiling file: {:?}", path.to_string_lossy().to_string());

        let file_text = fs::read_to_string(&path).or_else(|e| Err(e.to_string()))?;
        let tokens = lex(&file_text)?;
        let parsed = parse(tokens)?;
        if parsed.get_nodes().is_empty() {
            warn!("No script found in file {:?}. Skipping.", &path);
            continue;
        }

        let index = data.get_jump_table_len();
        build_with_data(parsed.get_root(), parsed.get_nodes().clone(), &mut data)?;
        let execution_start = match data.get_jump_point(index) {
            Some(i) => i,
            None => Err(format!("No jump point found after building {:?}", &path))?,
        };

        info!("Registering route: {:?}", route);
        route_to_expression.insert(route, execution_start);
    }

    Ok((route_to_expression, SimpleGarnishRuntime::new(data)))
}
