use std::collections::BTreeMap;

use crate::layout::ResizeSpec;
use crate::style::{InlineStyle, parse_style_literal};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSpec {
    pub name: String,
    pub resize_spec: ResizeSpec,
    pub default_style: InlineStyle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    columns: Vec<ColumnSpec>,
    positions: BTreeMap<String, usize>,
}

impl Schema {
    pub fn from_config(config: &BTreeMap<String, String>) -> Result<Self, String> {
        let columns = config
            .get("columns")
            .ok_or_else(|| "missing `columns` plugin config".to_string())?;

        let column_names: Vec<String> = columns
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        if column_names.is_empty() {
            return Err("`columns` must include at least one column".to_string());
        }

        let mut specs = Vec::with_capacity(column_names.len());
        let mut positions = BTreeMap::new();

        for name in column_names {
            if positions.contains_key(&name) {
                return Err(format!("duplicate column `{name}`"));
            }

            let config_key = format!("column_{name}");
            let definition = config.get(&config_key).map(String::as_str).unwrap_or("");
            let (resize_spec, default_style) = parse_column_definition(definition)?;
            let index = specs.len();

            specs.push(ColumnSpec {
                name: name.clone(),
                resize_spec,
                default_style,
            });
            positions.insert(name, index);
        }

        Ok(Self {
            columns: specs,
            positions,
        })
    }

    pub fn columns(&self) -> &[ColumnSpec] {
        &self.columns
    }

    pub fn len(&self) -> usize {
        self.columns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    pub fn index_of(&self, column_name: &str) -> Option<usize> {
        self.positions.get(column_name).copied()
    }
}

fn parse_column_definition(definition: &str) -> Result<(ResizeSpec, InlineStyle), String> {
    let mut resize_spec = ResizeSpec::Resize;
    let mut style = InlineStyle::default();

    for field in definition.split(';') {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }

        let (key, value) = field
            .split_once('=')
            .ok_or_else(|| format!("invalid column field `{field}`"))?;

        match key.trim() {
            "resize" => resize_spec = ResizeSpec::parse(value.trim())?,
            "style" => style = parse_style_literal(value.trim()),
            other => return Err(format!("unsupported column field `{other}`")),
        }
    }

    Ok((resize_spec, style))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{ResizeMode, TruncationSide};

    #[test]
    fn parses_schema_from_flat_config() {
        let config = BTreeMap::from([
            ("columns".to_string(), "branch,status".to_string()),
            (
                "column_branch".to_string(),
                "resize=resize;style=#[fg=blue]".to_string(),
            ),
            (
                "column_status".to_string(),
                "resize=trunc:end:fixed:10;style=#[fg=yellow]".to_string(),
            ),
        ]);

        let schema = Schema::from_config(&config).unwrap();
        assert_eq!(schema.columns().len(), 2);
        assert_eq!(schema.columns()[0].name, "branch");
        assert_eq!(
            schema.columns()[1].resize_spec,
            ResizeSpec::Truncate {
                side: TruncationSide::End,
                mode: ResizeMode::Fixed(10),
            }
        );
    }
}
