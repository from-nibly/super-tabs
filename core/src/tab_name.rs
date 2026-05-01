use std::collections::BTreeMap;

use crate::schema::ColumnSpec;
use crate::style::CellState;

pub const SUPER_TAB_ID_KEY: &str = "__super_tabs_id";

pub fn encode_tab_name(columns: &[ColumnSpec], cells: &[Option<CellState>]) -> String {
    encode_tab_name_with_id(columns, cells, None)
}

pub fn encode_tab_name_with_id(
    columns: &[ColumnSpec],
    cells: &[Option<CellState>],
    super_tab_id: Option<&str>,
) -> String {
    let mut parts = Vec::new();

    if let Some(super_tab_id) = super_tab_id.filter(|super_tab_id| !super_tab_id.is_empty()) {
        push_encoded_part(&mut parts, SUPER_TAB_ID_KEY, super_tab_id);
    }

    for (index, column) in columns.iter().enumerate() {
        let value = cells
            .get(index)
            .and_then(|cell| cell.as_ref())
            .map(|cell| cell.plain_text.as_str())
            .unwrap_or_default();

        if value.is_empty() {
            continue;
        }

        push_encoded_part(&mut parts, &column.name, value);
    }

    parts.join(" | ")
}

fn push_encoded_part(parts: &mut Vec<String>, key: &str, value: &str) {
    parts.push(format!(
        "{}=\"{}\"",
        key,
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ));
}

pub fn decode_tab_name(input: &str) -> Option<BTreeMap<String, String>> {
    let input = input.trim();
    if input.is_empty() {
        return Some(BTreeMap::new());
    }

    let mut result = BTreeMap::new();
    let mut rest = input;

    loop {
        let (key_part, value_part) = rest.split_once('=')?;
        let key = key_part.trim();
        if key.is_empty() {
            return None;
        }

        let (value, remaining) = parse_quoted_value(value_part.trim_start())?;
        result.insert(key.to_string(), value);

        let remaining = remaining.trim_start();
        if remaining.is_empty() {
            return Some(result);
        }

        rest = remaining.strip_prefix('|')?.trim_start();
    }
}

pub fn decode_super_tab_id(input: &str) -> Option<String> {
    decode_tab_name(input)?
        .get(SUPER_TAB_ID_KEY)
        .filter(|super_tab_id| !super_tab_id.is_empty())
        .cloned()
}

fn parse_quoted_value(input: &str) -> Option<(String, &str)> {
    let mut chars = input.chars();
    if chars.next()? != '"' {
        return None;
    }

    let mut escaped = false;
    let mut value = String::new();
    let mut offset = 1;

    for ch in chars {
        offset += ch.len_utf8();
        if escaped {
            match ch {
                '\\' | '"' => value.push(ch),
                _ => return None,
            }
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '"' => return Some((value, &input[offset..])),
            _ => value.push(ch),
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::ResizeSpec;
    use crate::style::{CellState, InlineStyle};

    #[test]
    fn round_trips_quoted_values() {
        let columns = vec![
            ColumnSpec {
                name: "branch".to_string(),
                resize_spec: ResizeSpec::Resize,
                default_style: InlineStyle::default(),
            },
            ColumnSpec {
                name: "title".to_string(),
                resize_spec: ResizeSpec::Resize,
                default_style: InlineStyle::default(),
            },
        ];
        let cells = vec![
            Some(CellState::from_plain_text("main", &InlineStyle::default())),
            Some(CellState::from_plain_text(
                "api | worker \"blue\"",
                &InlineStyle::default(),
            )),
        ];

        let encoded = encode_tab_name(&columns, &cells);
        let decoded = decode_tab_name(&encoded).unwrap();

        assert_eq!(decoded.get("branch").unwrap(), "main");
        assert_eq!(decoded.get("title").unwrap(), "api | worker \"blue\"");
    }

    #[test]
    fn missing_keys_decode_to_absent_entries() {
        let decoded = decode_tab_name("branch=\"main\"").unwrap();
        assert_eq!(decoded.get("branch").unwrap(), "main");
        assert!(!decoded.contains_key("status"));
    }

    #[test]
    fn round_trips_super_tab_id() {
        let columns = vec![ColumnSpec {
            name: "branch".to_string(),
            resize_spec: ResizeSpec::Resize,
            default_style: InlineStyle::default(),
        }];
        let cells = vec![Some(CellState::from_plain_text(
            "main",
            &InlineStyle::default(),
        ))];

        let encoded = encode_tab_name_with_id(&columns, &cells, Some("st-12"));
        let decoded = decode_tab_name(&encoded).unwrap();

        assert_eq!(decode_super_tab_id(&encoded).as_deref(), Some("st-12"));
        assert_eq!(decoded.get(SUPER_TAB_ID_KEY).unwrap(), "st-12");
        assert_eq!(decoded.get("branch").unwrap(), "main");
    }
}
