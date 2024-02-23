use std::collections::HashMap;
use std::env::current_dir;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use clap::Parser;
use hyper::StatusCode;
use log::{debug, error, info, warn};
use serde::Deserialize;

use garnish_lang_annotations_collector::{Collector, Sink, TokenBlock};
use garnish_lang_simple_data::SimpleRuntimeData;
use garnish_lang_compiler::{
    build_with_data, parse, InstructionMetadata, LexerToken, ParseResult, TokenType,
};
use garnish_lang_runtime::runtime_impls::SimpleGarnishRuntime;
use garnish_lang_traits::{
    EmptyContext, ExpressionDataType, GarnishLangRuntimeData, GarnishLangRuntimeState,
    GarnishRuntime,
};
use garnish_lang_utilities::{create_execution_dump, format_build_info, format_runtime, BuildMetadata};
use hypertext_garnish::{Node, RuleSet};
use serde_garnish::GarnishDataDeserializer;

use crate::args::{ServerArgs, ServerSubCommand};
use crate::context::WebContext;

mod args;
mod context;

pub const INCLUDE_PATTERN_DEFAULT: &str = "**/*.garnish";

#[derive(Clone)]
struct SharedState {
    base_runtime: SimpleGarnishRuntime<SimpleRuntimeData>,
    context: WebContext,
    route_mapping: HashMap<String, RouteInfo>,
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
            "Could not covert serve path to string. Path: {:?}",
            serve_path
        ))?,
        Some(s) => s.to_string(),
    };

    debug!("Serving from path: {}", serve_path_str);

    serve_path.push(INCLUDE_PATTERN_DEFAULT);

    let glob_pattern = match serve_path.to_str() {
        None => Err(format!(
            "Could not covert match pattern string. Path: {:?}",
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

    let (route_mapping, mut runtime, mut context) = create_runtime(paths, serve_path_str.as_str())?;

    match args.command {
        ServerSubCommand::Serve => {
            let state = Arc::new(SharedState {
                route_mapping,
                base_runtime: runtime,
                context,
            });

            // build our application with a single route
            let app = Router::new()
                .route("/", any(handler))
                .route("/*path", any(handler))
                .with_state(state);

            // run it with hyper on localhost:3000
            axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
                .serve(app.into_make_service())
                .await
                .unwrap();
        }
        ServerSubCommand::Dump => {
            let metadata_output = context
                .metadata()
                .iter()
                .map(|meta| (meta.get_name().clone(), format_build_info(meta)))
                .collect::<Vec<(String, String)>>();

            let runtime_output = format_runtime(runtime.get_data(), &context, context.metadata());

            match args.route {
                None => (),
                Some(route) => match route_mapping.get(&route) {
                    None => debug!("Route {:?} not found", route),
                    Some(info) => {
                        match runtime.get_data_mut().set_instruction_cursor(info.execution_start) {
                            Ok(_) => debug!("Set instruction cursor to {}", info.execution_start),
                            Err(e) => error!("Failed to set instruction cursor. Reason: {}", e),
                        }
                    }
                },
            }

            let execution_output = create_execution_dump(&mut runtime, &mut context);

            match args.output_path {
                None => {
                    for o in metadata_output {
                        println!("{}", o.1);
                    }

                    println!("{}", runtime_output);

                    println!("{}", execution_output);
                }
                Some(out_path) => {
                    for (name, text) in metadata_output {
                        let mut path = out_path.clone();
                        path.push(format!("{}.txt", name.replace("/", "_")));

                        match fs::write(&path, text) {
                            Ok(_) => debug!(
                                "Successfully wrote build metadata dump to {}",
                                path.to_string_lossy().to_string()
                            ),
                            Err(e) => error!(
                                "Failed to write build metadata dump to {}. Reason: {}",
                                path.to_string_lossy().to_string(),
                                e
                            ),
                        }
                    }

                    let mut runtime_path = out_path.clone();
                    runtime_path.push("runtime.txt");
                    match fs::write(&runtime_path, runtime_output) {
                        Ok(_) => debug!(
                            "Successfully wrote runtime dump to {}",
                            runtime_path.to_string_lossy().to_string()
                        ),
                        Err(e) => error!(
                            "Failed to write runtime dump to {}. Reason: {}",
                            runtime_path.to_string_lossy().to_string(),
                            e
                        ),
                    }

                    let mut execution_path = out_path.clone();
                    execution_path.push("execution.txt");
                    match fs::write(&execution_path, execution_output) {
                        Ok(_) => debug!(
                            "Successfully wrote execution dump to {}",
                            execution_path.to_string_lossy().to_string()
                        ),
                        Err(e) => error!(
                            "Failed to write execution dump to {}. Reason: {}",
                            execution_path.to_string_lossy().to_string(),
                            e
                        ),
                    }
                }
            }
        }
    }

    Ok(())
}

async fn handler(
    State(state): State<Arc<SharedState>>,
    request: Request<Body>,
) -> Response<String> {
    let mut runtime = state.base_runtime.clone();
    let mut context = state.context.clone();

    let page = request.uri().path().trim().trim_matches('/').trim();
    let page_index = match page.is_empty() {
        true => String::from("index"),
        false => [page, "index"].join("/"),
    };
    let page_method = format!("{}@{}", request.method(), page);
    let page_index_method = format!("{}@{}", request.method(), page_index);

    let options = [page_method, page_index_method, page.into(), page_index];

    info!("Request for route \"{}\"", page);
    debug!("Checking options: {:?}", options);

    // find first options that is in route mapping
    // then get that option
    match options
        .iter()
        .find(|o| state.route_mapping.contains_key(*o))
        .and_then(|s| state.route_mapping.get(s))
    {
        None => {
            info!("No garnish mapping found for route \"{}\"", page);
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(String::new())
                .unwrap()
        }
        Some(info) => {
            match runtime
                .get_data_mut()
                .set_instruction_cursor(info.execution_start)
            {
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
                match runtime.execute_current_instruction(Some(&mut context)) {
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

            debug!("Result: {}", runtime.get_data().display_current_value());

            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/html")
                .body(current_value_to_string(
                    runtime.get_data_mut(),
                    info.file_type,
                ))
                .unwrap()
        }
    }
}

fn current_value_to_string(data: &mut SimpleRuntimeData, file_type: FileType) -> String {
    match file_type {
        FileType::HTML => deserialize_current_value::<Node>(data),
        FileType::CSS => deserialize_current_value::<RuleSet>(data),
    }
}

fn deserialize_current_value<'de, T: Deserialize<'de> + ToString>(
    data: &'de mut SimpleRuntimeData,
) -> String {
    let mut deserializer = GarnishDataDeserializer::new(data);
    match T::deserialize(&mut deserializer) {
        Err(e) => {
            error!(
                "Failed to deserialize garnish data to HTML: {:?}{:?}",
                e.message(),
                e
            );
            String::new()
        }
        Ok(n) => n.to_string(),
    }
}

#[derive(Clone, Copy, Ord, PartialOrd, Eq, PartialEq, Debug)]
enum FileType {
    HTML,
    CSS,
}

#[derive(Clone, Eq, PartialEq, Debug)]
struct RouteInfo {
    route: String,
    file_type: FileType,
    execution_start: usize,
}

impl RouteInfo {
    pub fn new<T: Into<String>>(route: T, file_type: FileType, execution_start: usize) -> Self {
        Self {
            route: route.into(),
            file_type,
            execution_start,
        }
    }
}

fn create_runtime(
    paths: Vec<PathBuf>,
    base_path: &str,
) -> Result<
    (
        HashMap<String, RouteInfo>,
        SimpleGarnishRuntime<SimpleRuntimeData>,
        WebContext,
    ),
    String,
> {
    let mut runtime = SimpleGarnishRuntime::new(SimpleRuntimeData::new());
    let mut context = WebContext::new();

    // maps expected http route to index of expression that will be executed when that route is requested
    let mut route_to_expression = HashMap::new();

    for path in paths {
        let (route, file_type) = path
            .strip_prefix(base_path)
            .and_then(|s| Ok(s.to_string_lossy().replace(".garnish", "")))
            .and_then(|s| {
                Ok(if s.ends_with(".html") {
                    (s.replace(".html", ""), FileType::HTML)
                } else if s.ends_with(".css") {
                    (s.replace(".css", ""), FileType::CSS)
                } else {
                    (s, FileType::HTML)
                })
            })
            .or_else(|e| Err(e.to_string()))?;

        debug!("Compiling file: {:?}", path.to_string_lossy().to_string());

        let file_text = fs::read_to_string(&path).or_else(|e| Err(e.to_string()))?;

        let collector: Collector = Collector::new(vec![
            Sink::new("@Method").until_token(TokenType::Subexpression),
            Sink::new("@Def").until_token(TokenType::Subexpression),
        ]);

        let blocks: Vec<TokenBlock> = collector.collect_tokens_from_input(&file_text)?;

        let (root_blocks, annotation_blocks): (Vec<TokenBlock>, Vec<TokenBlock>) = blocks
            .into_iter()
            .partition(|b| b.annotation_text().is_empty());

        let (method_blocks, def_blocks): (Vec<_>, Vec<_>) = annotation_blocks
            .into_iter()
            .partition(|b| b.annotation_text() == &"@Method".to_string());

        let mut method_metadata = handle_method_annotations(
            method_blocks,
            &mut runtime,
            &mut context,
            &path,
            &route,
            file_type,
            &mut route_to_expression,
        )?;

        context.metadata_mut().append(&mut method_metadata);

        let mut def_metadata =
            handle_def_annotations(def_blocks, &mut runtime, &mut context, &path)?;

        context.metadata_mut().append(&mut def_metadata);

        let root_tokens = root_blocks
            .into_iter()
            .flat_map(|b| b.tokens_owned())
            .collect::<Vec<LexerToken>>();

        let source = root_tokens
            .iter()
            .map(|token| token.get_text().clone())
            .collect::<Vec<String>>()
            .join("");

        let parsed = parse(&root_tokens)?;
        if parsed.get_nodes().is_empty() {
            debug!("No root script found in file {:?}. Skipping.", &path);
            continue;
        }

        let index = runtime.get_data().get_jump_table_len();
        let instruction_data = build_with_data(
            parsed.get_root(),
            parsed.get_nodes().clone(),
            runtime.get_data_mut(),
        )?;
        let execution_start = match runtime.get_data().get_jump_point(index) {
            Some(i) => i,
            None => Err(format!("No jump point found after building {:?}", &path))?,
        };

        let root_metadata = BuildMetadata::new(
            format!("{}", path.to_string_lossy().to_string()),
            source,
            execution_start,
            root_tokens,
            parsed,
            instruction_data,
        );

        context.metadata_mut().push(root_metadata);

        info!("Registering route: {}", route);
        route_to_expression.insert(
            route.clone(),
            RouteInfo::new(route.clone(), file_type, execution_start),
        );
        context.insert_expression(route.clone(), index)
    }

    Ok((route_to_expression, runtime, context))
}

fn handle_def_annotations(
    blocks: Vec<TokenBlock>,
    runtime: &mut SimpleGarnishRuntime<SimpleRuntimeData>,
    context: &mut WebContext,
    path: &PathBuf,
) -> Result<Vec<BuildMetadata<SimpleRuntimeData>>, String> {
    let mut builds = vec![];

    for def in blocks {
        let source = def
            .tokens()
            .iter()
            .map(|token| token.get_text().clone())
            .collect::<Vec<String>>()
            .join("");

        let (parsed, instruction_data, name, start) =
            match build_and_get_parameters(def.tokens(), runtime, path) {
                Err(s) => {
                    error!("{}", s);
                    continue;
                }
                Ok(v) => v,
            };

        builds.push(BuildMetadata::new(
            format!("{} -> {}", path.to_string_lossy().to_string(), name.clone()),
            source,
            start,
            def.tokens_owned(),
            parsed,
            instruction_data,
        ));

        debug!("Found method: {}", name);
        context.insert_expression(name, start);
    }

    Ok(builds)
}

fn handle_method_annotations(
    blocks: Vec<TokenBlock>,
    runtime: &mut SimpleGarnishRuntime<SimpleRuntimeData>,
    context: &mut WebContext,
    path: &PathBuf,
    route: &String,
    file_type: FileType,
    route_to_expression: &mut HashMap<String, RouteInfo>,
) -> Result<Vec<BuildMetadata<SimpleRuntimeData>>, String> {
    let mut builds = vec![];

    for method in blocks {
        let source = method
            .tokens()
            .iter()
            .map(|token| token.get_text().clone())
            .collect::<Vec<String>>()
            .join("");
        let (parsed, instruction_data, name, jump_index) =
            match build_and_get_parameters(method.tokens(), runtime, path) {
                Err(_) => continue,
                Ok(v) => v,
            };

        // http method expressions use direct jump point instead of jump table reference that is stored in the Expression data type
        let start = match runtime.get_data().get_jump_point(jump_index) {
            None => {
                error!(
                    "Jump table reference not found. Searching for {}",
                    jump_index
                );
                return Err("Expression value not found in jump table".into());
            }
            Some(s) => s,
        };

        builds.push(BuildMetadata::new(
            format!("{} -> {}", path.to_string_lossy().to_string(), name.clone()),
            source,
            start,
            method.tokens_owned(),
            parsed,
            instruction_data,
        ));

        info!("Registering route: {}@{}", name, route);
        let route = format!("{}@{}", name, route);
        route_to_expression.insert(route.clone(), RouteInfo::new(&route, file_type, start));
        context.insert_expression(route.clone(), jump_index);
    }

    Ok(builds)
}

fn build_and_get_parameters(
    tokens: &Vec<LexerToken>,
    runtime: &mut SimpleGarnishRuntime<SimpleRuntimeData>,
    path: &PathBuf,
) -> Result<(ParseResult, Vec<InstructionMetadata>, String, usize), String> {
    let parsed = parse(tokens)?;
    if parsed.get_nodes().is_empty() {
        warn!("Empty method annotation in {:?}", &path);
        return Err("Empty annotation".into());
    }

    let index = runtime.get_data().get_jump_table_len();
    let instruction_data = build_with_data(
        parsed.get_root(),
        parsed.get_nodes().clone(),
        runtime.get_data_mut(),
    )?;
    let execution_start = match runtime.get_data().get_jump_point(index) {
        Some(i) => i,
        None => Err(format!("No jump point found after building {:?}", &path))?,
    };

    // executing from this start should result in list with annotation parameters
    match runtime
        .get_data_mut()
        .set_instruction_cursor(execution_start)
    {
        Err(e) => {
            error!(
                "Failed to set instructor cursor during annotation build: {:?}",
                e
            );
            return Err("Couldn't set cursor".into());
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
            return Err("No value after execution".into());
        }
        Some(v) => v,
    };

    let (name, start) =
        get_name_expression_annotation_parameters(runtime, value_ref).or(Err(String::new()))?;

    Ok((parsed, instruction_data, name, start))
}

fn get_name_expression_annotation_parameters(
    runtime: &mut SimpleGarnishRuntime<SimpleRuntimeData>,
    value_ref: usize,
) -> Result<(String, usize), ()> {
    match runtime.get_data().get_data_type(value_ref) {
        Err(_) => {
            error!("Failed to retrieve value data type after annotation execution.");
            Err(())
        }
        Ok(t) => match t {
            ExpressionDataType::List => {
                // check for 2 values in list
                let method_name = match runtime.get_data().get_list_item(value_ref, 0.into()) {
                    Err(e) => {
                        error!(
                            "Failed to retrieve list item 0 for annotation list value. {:?}",
                            e
                        );
                        return Err(());
                    }
                    Ok(v) => match runtime.get_data().get_data_type(v) {
                        Err(_) => {
                            error!("Failed to retrieve value data type for annotation list value.");
                            return Err(());
                        }
                        Ok(t) => match t {
                            ExpressionDataType::Symbol => {
                                match runtime.get_data().get_symbol(v) {
                                    Err(_) => {
                                        error!("No data found for annotation list value item 0");
                                        return Err(());
                                    }
                                    Ok(s) => match runtime.get_data().get_symbols().get(&s) {
                                        None => {
                                            error!("Symbol with value {} not found in data symbol table", s);
                                            return Err(());
                                        }
                                        Some(s) => s.clone(),
                                    },
                                }
                            }
                            ExpressionDataType::CharList => {
                                match runtime.get_data().get_data().get(v) {
                                    None => {
                                        error!("No data found for annotation list value item 0");
                                        return Err(());
                                    }
                                    Some(s) => match s.as_char_list() {
                                        Err(e) => {
                                            error!("Value stored in Character List slot {} could not be cast to Character List. {:?}", v, e);
                                            return Err(());
                                        }
                                        Ok(s) => s,
                                    },
                                }
                            }
                            _ => {
                                error!("Expected Character List or Symbol type as first parameter in annotation list value");
                                return Err(());
                            }
                        },
                    },
                };

                let execution_start = match runtime.get_data().get_list_item(value_ref, 1.into()) {
                    Err(e) => {
                        error!(
                            "Failed to retrieve list item 1 for annotation list value. {:?}",
                            e
                        );
                        return Err(());
                    }
                    Ok(v) => match runtime.get_data().get_data_type(v) {
                        Err(_) => {
                            error!("Failed to retrieve value data type for annotation list value.");
                            return Err(());
                        }
                        Ok(t) => match t {
                            ExpressionDataType::Expression => {
                                match runtime.get_data().get_expression(v) {
                                    Err(_) => {
                                        error!("No data found for annotation list value item 0");
                                        return Err(());
                                    }
                                    Ok(s) => s,
                                }
                            }
                            _ => {
                                error!("Expected Expression type as second parameter in annotation list value");
                                return Err(());
                            }
                        },
                    },
                };

                Ok((method_name, execution_start))
            }
            t => {
                warn!(
                    "Expected List data type after annotation execution. Found {:?}",
                    t
                );
                Err(())
            }
        },
    }
}
