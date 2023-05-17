use std::collections::HashMap;
use garnish_data::{DataError, SimpleRuntimeData};
use garnish_traits::{GarnishLangRuntimeContext, GarnishLangRuntimeData, RuntimeError};

pub struct WebContext {
    expression_map: HashMap<String, usize>,
}

impl WebContext {
    pub fn new() -> Self {
        Self { expression_map: HashMap::new() }
    }

    pub fn insert_expression<T: Into<String>>(&mut self, name: T, table_index: usize) {
        self.expression_map.insert(name.into(), table_index);
    }
}

impl GarnishLangRuntimeContext<SimpleRuntimeData> for WebContext {
    fn resolve(&mut self, symbol: u64, data: &mut SimpleRuntimeData) -> Result<bool, RuntimeError<DataError>> {
        match data.get_symbols().get(&symbol) {
            None => Ok(false),
            Some(s) => match self.expression_map.get(s) {
                None => Ok(false),
                Some(i) => {
                    data.add_expression(*i)?;
                    Ok(true)
                }
            }
        }
    }
}