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

use garnish_annotations_collector::{Collector, Sink, TokenBlock};
use garnish_data::SimpleRuntimeData;
use garnish_lang_compiler::{build_with_data, lex, parse, LexerToken, TokenType};
use garnish_lang_runtime::runtime_impls::SimpleGarnishRuntime;
use garnish_traits::{
    EmptyContext, ExpressionDataType, GarnishLangRuntimeData, GarnishLangRuntimeState,
    GarnishRuntime,
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
    let mut runtime = SimpleGarnishRuntime::new(SimpleRuntimeData::new());

    // maps expected http route to index of expression that will be executed when that route is requested
    let mut route_to_expression = HashMap::new();

    for path in paths {
        let route = path
            .strip_prefix(base_path)
            .and_then(|s| Ok(s.to_string_lossy().replace(".garnish", "")))
            .or_else(|e| Err(e.to_string()))?;

        debug!("Compiling file: {:?}", path.to_string_lossy().to_string());

        let file_text = fs::read_to_string(&path).or_else(|e| Err(e.to_string()))?;

        let collector: Collector = Collector::new(vec![
            Sink::new("@Method").until_token(TokenType::Subexpression),
            Sink::new("@Def").until_token(TokenType::Subexpression),
        ]);

        let blocks: Vec<TokenBlock> = collector.collect_tokens(&file_text)?;

        let (root_blocks, annotation_blocks): (Vec<TokenBlock>, Vec<TokenBlock>) = blocks
            .into_iter()
            .partition(|b| b.annotation_text().is_empty());

        let (method_blocks, def_blocks): (Vec<_>, Vec<_>) = annotation_blocks
            .into_iter()
            .partition(|b| b.annotation_text() == &"@Method".to_string());

        handle_method_annotations(method_blocks, &mut runtime, &path, &route, &mut route_to_expression)?;

        let root_tokens = root_blocks
            .into_iter()
            .flat_map(|b| b.tokens_owned())
            .collect::<Vec<LexerToken>>();

        let parsed = parse(root_tokens)?;
        if parsed.get_nodes().is_empty() {
            debug!("No root script found in file {:?}. Skipping.", &path);
            continue;
        }

        let index = runtime.get_data().get_jump_table_len();
        build_with_data(
            parsed.get_root(),
            parsed.get_nodes().clone(),
            runtime.get_data_mut(),
        )?;
        let execution_start = match runtime.get_data().get_jump_point(index) {
            Some(i) => i,
            None => Err(format!("No jump point found after building {:?}", &path))?,
        };

        info!("Registering route: {}", route);
        route_to_expression.insert(route, execution_start);
    }

    Ok((route_to_expression, runtime))
}

fn handle_method_annotations(
    blocks: Vec<TokenBlock>,
    runtime: &mut SimpleGarnishRuntime<SimpleRuntimeData>,
    path: &PathBuf,
    route: &String,
    route_to_expression: &mut HashMap<String, usize>
) -> Result<(), String> {
    for method in blocks {
        let parsed = parse(method.tokens_owned())?;
        if parsed.get_nodes().is_empty() {
            warn!("Empty method annotation in {:?}", &path);
            continue;
        }

        let index = runtime.get_data().get_jump_table_len();
        build_with_data(
            parsed.get_root(),
            parsed.get_nodes().clone(),
            runtime.get_data_mut(),
        )?;
        let execution_start = match runtime.get_data().get_jump_point(index) {
            Some(i) => i,
            None => Err(format!("No jump point found after building {:?}", &path))?,
        };

        // executing from this start should result in list with annotation parameters
        match runtime.get_data_mut().set_instruction_cursor(execution_start) {
            Err(e) => {
                error!(
                        "Failed to set instructor cursor during annotation build: {:?}",
                        e
                    );
                continue;
            }
            Ok(()) => (),
        }

        loop {
            match runtime.execute_current_instruction::<EmptyContext>(None) {
                Err(e) => {
                    error!("Failure during annotation execution: {:?}", e);
                    continue;
                }
                Ok(data) => match data.get_state() {
                    GarnishLangRuntimeState::Running => (),
                    GarnishLangRuntimeState::End => break,
                },
            }
        }

        let value_ref = match runtime.get_data().get_current_value() {
            None => {
                error!("No value after annotation execution. Expected value of type List.");
                continue;
            }
            Some(v) => v,
        };

        let (name, start) = match runtime.get_data().get_data_type(value_ref) {
            Err(e) => {
                error!("Failed to retrieve value data type after annotation execution.");
                continue;
            }
            Ok(t) => match t {
                ExpressionDataType::List => {
                    // check for 2 values in list
                    let method_name = match runtime.get_data().get_list_item(value_ref, 0.into()) {
                        Err(e) => {
                            error!("Failed to retrieve list item 0 for annotation list value. {:?}", e);
                            continue;
                        }
                        Ok(v) => match runtime.get_data().get_data_type(v) {
                            Err(e) => {
                                error!("Failed to retrieve value data type for annotation list value.");
                                continue;
                            }
                            Ok(t) => match t {
                                ExpressionDataType::Symbol => {
                                    match runtime.get_data().get_symbol(v) {
                                        Err(e) => {
                                            error!("No data found for annotation list value item 0");
                                            continue;
                                        }
                                        Ok(s) => match runtime.get_data().get_symbols().get(&s) {
                                            None => {
                                                error!("Symbol with value {} not found in data symbol table", s);
                                                continue;
                                            }
                                            Some(s) => s.clone()
                                        }
                                    }
                                }
                                ExpressionDataType::CharList => {
                                    match runtime.get_data().get_data().get(v) {
                                        None => {
                                            error!("No data found for annotation list value item 0");
                                            continue;
                                        }
                                        Some(s) => match s.as_char_list() {
                                            Err(e) => {
                                                error!("Value stored in Character List slot {} could not be cast to Character List. {:?}", v, e);
                                                continue;
                                            }
                                            Ok(s) => s,
                                        },
                                    }
                                }
                                t => {
                                    error!("Expected Character List or Symbol type as first parameter in annotation list value");
                                    continue;
                                }
                            },
                        },
                    };

                    let execution_start = match runtime.get_data().get_list_item(value_ref, 1.into()) {
                        Err(e) => {
                            error!("Failed to retrieve list item 1 for annotation list value. {:?}", e);
                            continue;
                        }
                        Ok(v) => match runtime.get_data().get_data_type(v) {
                            Err(e) => {
                                error!("Failed to retrieve value data type for annotation list value.");
                                continue;
                            }
                            Ok(t) => match t {
                                ExpressionDataType::Expression => {
                                    match runtime.get_data().get_expression(v) {
                                        Err(e) => {
                                            error!("No data found for annotation list value item 0");
                                            continue;
                                        }
                                        Ok(s) => match runtime.get_data().get_jump_point(s) {
                                            None => {
                                                error!("Symbol with value {} not found in data symbol table", s);
                                                continue;
                                            }
                                            Some(s) => s
                                        },
                                    }
                                }
                                t => {
                                    error!("Expected Expression type as second parameter in annotation list value");
                                    continue;
                                }
                            },
                        },
                    };

                    (method_name, execution_start)
                }
                t => {
                    warn!(
                            "Expected List data type after annotation execution. Found {:?}",
                            t
                        );
                    continue;
                }
            },
        };

        info!("Registering route: {}@{}", name, route);
        route_to_expression.insert(format!("{}@{}", name, route), execution_start);
    }

    Ok(())
}