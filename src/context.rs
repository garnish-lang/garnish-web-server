use garnish_lang_simple_data::{DataError, SimpleRuntimeData};
use garnish_lang_traits::{GarnishLangRuntimeContext, GarnishLangRuntimeData, RuntimeError};
use garnish_utils::{BuildMetadata, DataInfoProvider};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct WebContext {
    expression_map: HashMap<String, usize>,
    build_metadata: Vec<BuildMetadata<SimpleRuntimeData>>,
}

impl WebContext {
    pub fn new() -> Self {
        Self {
            expression_map: HashMap::new(),
            build_metadata: vec![],
        }
    }

    pub fn insert_expression<T: Into<String>>(&mut self, name: T, table_index: usize) {
        self.expression_map.insert(name.into(), table_index);
    }

    pub fn metadata(&self) -> &Vec<BuildMetadata<SimpleRuntimeData>> {
        &self.build_metadata
    }

    pub fn metadata_mut(&mut self) -> &mut Vec<BuildMetadata<SimpleRuntimeData>> {
        &mut self.build_metadata
    }
}

impl GarnishLangRuntimeContext<SimpleRuntimeData> for WebContext {
    fn resolve(
        &mut self,
        symbol: u64,
        data: &mut SimpleRuntimeData,
    ) -> Result<bool, RuntimeError<DataError>> {
        match data.get_symbols().get(&symbol) {
            None => Ok(false),
            Some(s) => match self.expression_map.get(s) {
                None => Ok(false),
                Some(i) => {
                    data.add_expression(*i)
                        .and_then(|i| data.push_register(i))?;
                    Ok(true)
                }
            },
        }
    }
}

impl DataInfoProvider<SimpleRuntimeData> for WebContext {
    fn get_symbol_name(&self, sym: u64, data: &SimpleRuntimeData) -> Option<String> {
        data.get_data().get_symbol(sym).cloned()
    }

    fn get_address_name(&self, addr: usize, data: &SimpleRuntimeData) -> Option<String> {
        self.expression_map
            .iter()
            .map(|(k, v)| (k, data.get_jump_point(*v)))
            .filter(|p| p.1.is_some())
            .map(|p| (p.0, p.1.unwrap()))
            .find(|p| p.1 == addr)
            .and_then(|p| Some(p.0))
            .cloned()
    }
    fn format_symbol_data(
        &self,
        sym: u64,
        data: &SimpleRuntimeData,
    ) -> Option<String> {
        data.get_data().get_symbol(sym).and_then(|sym_name| {
            self.expression_map
                .get(sym_name)
                .and_then(|p| match data.get_jump_point(*p) {
                    None => Some(format!("Symbol resolves to expression: {} @ [no jump table index {}]", sym_name, p)),
                    Some(point) => Some(format!("Symbol resolves to expression: {} @ {}", sym_name, point)),
                })
        })
    }
}
