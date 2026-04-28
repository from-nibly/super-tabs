use std::cmp::min;
use std::collections::BTreeMap;

use crate::schema::ColumnSpec;
use crate::style::StyledText;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationSide {
    Start,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeMode {
    Hard(usize),
    Fixed(usize),
    Flow(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeSpec {
    Resize,
    Truncate {
        side: TruncationSide,
        mode: ResizeMode,
    },
}

impl ResizeSpec {
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();
        if input == "resize" {
            return Ok(Self::Resize);
        }

        let mut parts = input.split(':');
        let prefix = parts.next().unwrap_or_default();
        let side = parts.next().unwrap_or_default();
        let mode = parts.next().unwrap_or_default();
        let amount = parts.next().unwrap_or_default();

        if prefix != "trunc" || parts.next().is_some() {
            return Err(format!("invalid resize spec `{input}`"));
        }

        let side = match side {
            "start" => TruncationSide::Start,
            "end" => TruncationSide::End,
            _ => return Err(format!("invalid truncation side `{side}`")),
        };

        let amount = amount
            .parse::<usize>()
            .map_err(|_| format!("invalid resize amount `{amount}`"))?;

        let mode = match mode {
            "hard" => ResizeMode::Hard(amount),
            "fixed" => ResizeMode::Fixed(amount),
            "flow" => ResizeMode::Flow(amount.max(1)),
            _ => return Err(format!("invalid resize mode `{mode}`")),
        };

        Ok(Self::Truncate { side, mode })
    }

    pub fn flow_weight(self) -> Option<usize> {
        match self {
            ResizeSpec::Truncate {
                mode: ResizeMode::Flow(weight),
                ..
            } => Some(weight.max(1)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WidthIndex {
    counts: BTreeMap<usize, usize>,
}

impl WidthIndex {
    pub fn replace(&mut self, old_width: Option<usize>, new_width: usize) {
        if let Some(old_width) = old_width {
            self.remove(old_width);
        }
        self.add(new_width);
    }

    pub fn remove(&mut self, width: usize) {
        if let Some(count) = self.counts.get_mut(&width) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.counts.remove(&width);
            }
        }
    }

    pub fn max(&self) -> usize {
        self.counts
            .last_key_value()
            .map(|(width, _)| *width)
            .unwrap_or(0)
    }

    fn add(&mut self, width: usize) {
        *self.counts.entry(width).or_insert(0) += 1;
    }
}

pub fn solve_column_widths(
    columns: &[ColumnSpec],
    natural_widths: &[usize],
    available_width: usize,
    gap_width: usize,
) -> Vec<usize> {
    debug_assert_eq!(columns.len(), natural_widths.len());

    let mut widths = vec![0; columns.len()];
    let gap_total = gap_width.saturating_mul(columns.len().saturating_sub(1));
    let content_budget = available_width.saturating_sub(gap_total);
    let mut fixed_total = 0usize;
    let mut flow_columns = vec![];

    for (index, (column, natural_width)) in columns.iter().zip(natural_widths).enumerate() {
        match column.resize_spec {
            ResizeSpec::Resize => {
                widths[index] = *natural_width;
                fixed_total += widths[index];
            }
            ResizeSpec::Truncate {
                mode: ResizeMode::Hard(limit),
                ..
            } => {
                widths[index] = min(*natural_width, limit);
                fixed_total += widths[index];
            }
            ResizeSpec::Truncate {
                mode: ResizeMode::Fixed(limit),
                ..
            } => {
                widths[index] = limit;
                fixed_total += widths[index];
            }
            ResizeSpec::Truncate {
                mode: ResizeMode::Flow(weight),
                ..
            } => flow_columns.push((index, *natural_width, weight.max(1))),
        }
    }

    if flow_columns.is_empty() {
        return widths;
    }

    let remaining = content_budget.saturating_sub(fixed_total);
    let total_flow_natural: usize = flow_columns.iter().map(|(_, natural, _)| *natural).sum();
    if total_flow_natural <= remaining {
        for (index, natural_width, _) in flow_columns {
            widths[index] = natural_width;
        }
        return widths;
    }

    let total_weight: usize = flow_columns.iter().map(|(_, _, weight)| *weight).sum();
    let mut assigned = 0usize;
    let mut remainders = vec![];

    for (index, natural_width, weight) in &flow_columns {
        let exact = (remaining as f64 * *weight as f64) / total_weight as f64;
        let base = min(exact.floor() as usize, *natural_width);
        widths[*index] = base;
        assigned += base;
        remainders.push((exact - exact.floor(), *index, *natural_width));
    }

    remainders.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap()
            .then_with(|| left.1.cmp(&right.1))
    });

    let mut leftover = remaining.saturating_sub(assigned);
    while leftover > 0 {
        let mut progressed = false;
        for (_, index, natural_width) in &remainders {
            if widths[*index] < *natural_width {
                widths[*index] += 1;
                leftover -= 1;
                progressed = true;
                if leftover == 0 {
                    break;
                }
            }
        }

        if !progressed {
            break;
        }
    }

    widths
}

pub fn fit_cell_to_width(text: &StyledText, resize_spec: ResizeSpec, width: usize) -> StyledText {
    if width == 0 {
        return StyledText::new();
    }

    if text.display_width() <= width {
        return text.clone();
    }

    match resize_spec {
        ResizeSpec::Resize => text.clip_end(width),
        ResizeSpec::Truncate {
            side: TruncationSide::Start,
            ..
        } => text.truncate_start(width),
        ResizeSpec::Truncate {
            side: TruncationSide::End,
            ..
        } => text.truncate_end(width),
    }
}

pub fn clip_right_edge(text: &StyledText, width: usize) -> StyledText {
    text.clip_end(width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ColumnSpec;
    use crate::style::InlineStyle;

    fn column(name: &str, resize_spec: ResizeSpec) -> ColumnSpec {
        ColumnSpec {
            name: name.to_string(),
            resize_spec,
            default_style: InlineStyle::default(),
        }
    }

    #[test]
    fn parses_resize_specs() {
        assert_eq!(ResizeSpec::parse("resize").unwrap(), ResizeSpec::Resize);
        assert_eq!(
            ResizeSpec::parse("trunc:end:hard:10").unwrap(),
            ResizeSpec::Truncate {
                side: TruncationSide::End,
                mode: ResizeMode::Hard(10),
            }
        );
        assert_eq!(
            ResizeSpec::parse("trunc:end:fixed:10").unwrap(),
            ResizeSpec::Truncate {
                side: TruncationSide::End,
                mode: ResizeMode::Fixed(10),
            }
        );
    }

    #[test]
    fn hard_width_caps_natural_width() {
        let columns = vec![column(
            "status",
            ResizeSpec::Truncate {
                side: TruncationSide::End,
                mode: ResizeMode::Hard(4),
            },
        )];
        assert_eq!(solve_column_widths(&columns, &[10], 20, 1), vec![4]);
    }

    #[test]
    fn fixed_width_reserves_exact_space() {
        let columns = vec![column(
            "status",
            ResizeSpec::Truncate {
                side: TruncationSide::End,
                mode: ResizeMode::Fixed(10),
            },
        )];
        assert_eq!(solve_column_widths(&columns, &[3], 20, 1), vec![10]);
    }

    #[test]
    fn flow_widths_split_by_weight() {
        let columns = vec![
            column(
                "left",
                ResizeSpec::Truncate {
                    side: TruncationSide::End,
                    mode: ResizeMode::Flow(1),
                },
            ),
            column(
                "right",
                ResizeSpec::Truncate {
                    side: TruncationSide::End,
                    mode: ResizeMode::Flow(3),
                },
            ),
        ];

        assert_eq!(solve_column_widths(&columns, &[20, 20], 17, 1), vec![4, 12]);
    }

    #[test]
    fn right_edge_clip_uses_hard_clip() {
        let clipped = clip_right_edge(&StyledText::plain("abcdef"), 4);
        assert_eq!(clipped.plain_text(), "abcd");
    }
}
