use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const TRUNCATION_MARKER: &str = "…";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorSpec {
    #[default]
    Default,
    EightBit(u8),
    Rgb(u8, u8, u8),
}

impl ColorSpec {
    pub fn to_ansi_fg(self) -> String {
        match self {
            ColorSpec::Default => String::new(),
            ColorSpec::EightBit(n) => format!("\x1b[38;5;{}m", n),
            ColorSpec::Rgb(r, g, b) => format!("\x1b[38;2;{};{};{}m", r, g, b),
        }
    }

    pub fn to_ansi_bg(self) -> String {
        match self {
            ColorSpec::Default => String::new(),
            ColorSpec::EightBit(n) => format!("\x1b[48;5;{}m", n),
            ColorSpec::Rgb(r, g, b) => format!("\x1b[48;2;{};{};{}m", r, g, b),
        }
    }

    pub fn is_default(self) -> bool {
        matches!(self, ColorSpec::Default)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InlineStyle {
    pub fg: ColorSpec,
    pub bg: ColorSpec,
    pub bold: bool,
    pub dim: bool,
    pub fill: bool,
}

impl InlineStyle {
    pub fn to_ansi(&self) -> String {
        let mut result = String::new();

        if self.bold {
            result.push_str("\x1b[1m");
        }
        if self.dim {
            result.push_str("\x1b[2m");
        }

        result.push_str(&self.fg.to_ansi_fg());
        result.push_str(&self.bg.to_ansi_bg());
        result
    }

    pub fn has_any_style(&self) -> bool {
        !self.fg.is_default() || !self.bg.is_default() || self.bold || self.dim || self.fill
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSegment {
    pub text: String,
    pub style: InlineStyle,
}

impl StyledSegment {
    pub fn display_width(&self) -> usize {
        self.text.width()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyledText {
    pub segments: Vec<StyledSegment>,
}

impl StyledText {
    pub fn new() -> Self {
        Self { segments: vec![] }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        let mut styled = Self::new();
        styled.push(text.into(), InlineStyle::default());
        styled
    }

    pub fn push(&mut self, text: impl Into<String>, style: InlineStyle) {
        let text = text.into();
        if text.is_empty() {
            return;
        }

        if let Some(last) = self.segments.last_mut()
            && last.style == style
        {
            last.text.push_str(&text);
            return;
        }

        self.segments.push(StyledSegment { text, style });
    }

    pub fn push_plain(&mut self, text: impl Into<String>) {
        self.push(text.into(), InlineStyle::default());
    }

    pub fn extend(&mut self, other: StyledText) {
        for segment in other.segments {
            self.push(segment.text, segment.style);
        }
    }

    pub fn display_width(&self) -> usize {
        self.segments.iter().map(StyledSegment::display_width).sum()
    }

    pub fn plain_text(&self) -> String {
        let mut plain = String::new();
        for segment in &self.segments {
            plain.push_str(&segment.text);
        }
        plain
    }

    pub fn to_ansi(&self) -> String {
        let mut result = String::new();
        let mut current_style = InlineStyle::default();

        for segment in &self.segments {
            if segment.style != current_style {
                if current_style.has_any_style() {
                    result.push_str("\x1b[0m");
                }
                result.push_str(&segment.style.to_ansi());
                current_style = segment.style.clone();
            }
            result.push_str(&segment.text);
        }

        if current_style.has_any_style() {
            result.push_str("\x1b[0m");
        }

        result
    }

    pub fn clip_end(&self, max_width: usize) -> Self {
        self.take_prefix(max_width)
    }

    pub fn clip_start(&self, max_width: usize) -> Self {
        self.take_suffix(max_width)
    }

    pub fn truncate_end(&self, max_width: usize) -> Self {
        if self.display_width() <= max_width {
            return self.clone();
        }
        if max_width == 0 {
            return Self::new();
        }

        let marker_width = TRUNCATION_MARKER.width();
        if max_width <= marker_width {
            return Self::plain(TRUNCATION_MARKER);
        }

        let mut truncated = self.take_prefix(max_width - marker_width);
        truncated.push_plain(TRUNCATION_MARKER);
        truncated
    }

    pub fn truncate_start(&self, max_width: usize) -> Self {
        if self.display_width() <= max_width {
            return self.clone();
        }
        if max_width == 0 {
            return Self::new();
        }

        let marker_width = TRUNCATION_MARKER.width();
        if max_width <= marker_width {
            return Self::plain(TRUNCATION_MARKER);
        }

        let mut truncated = Self::plain(TRUNCATION_MARKER);
        truncated.extend(self.take_suffix(max_width - marker_width));
        truncated
    }

    fn take_prefix(&self, max_width: usize) -> Self {
        let mut remaining = max_width;
        let mut result = Self::new();

        for segment in &self.segments {
            for ch in segment.text.chars() {
                let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if ch_width > remaining {
                    return result;
                }
                result.push(ch.to_string(), segment.style.clone());
                remaining = remaining.saturating_sub(ch_width);
                if remaining == 0 {
                    return result;
                }
            }
        }

        result
    }

    fn take_suffix(&self, max_width: usize) -> Self {
        let mut remaining = max_width;
        let mut reversed_chars: Vec<(char, InlineStyle)> = vec![];

        for segment in self.segments.iter().rev() {
            for ch in segment.text.chars().rev() {
                let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if ch_width > remaining {
                    return reverse_chars(reversed_chars);
                }
                reversed_chars.push((ch, segment.style.clone()));
                remaining = remaining.saturating_sub(ch_width);
                if remaining == 0 {
                    return reverse_chars(reversed_chars);
                }
            }
        }

        reverse_chars(reversed_chars)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellState {
    pub raw_input: String,
    pub plain_text: String,
    pub styled_text: StyledText,
    pub had_inline_style: bool,
}

impl CellState {
    pub fn from_raw(raw_input: impl Into<String>, default_style: &InlineStyle) -> Self {
        let raw_input = raw_input.into();
        let had_inline_style = has_style_directive(&raw_input);
        let parsed = parse_styled_string(&raw_input);
        let plain_text = parsed.plain_text();
        let styled_text = if had_inline_style {
            parsed
        } else {
            apply_default_style(default_style, &plain_text)
        };

        Self {
            raw_input,
            plain_text,
            styled_text,
            had_inline_style,
        }
    }

    pub fn from_plain_text(plain_text: impl Into<String>, default_style: &InlineStyle) -> Self {
        let plain_text = plain_text.into();
        Self {
            raw_input: plain_text.clone(),
            styled_text: apply_default_style(default_style, &plain_text),
            plain_text,
            had_inline_style: false,
        }
    }

    pub fn display_width(&self) -> usize {
        self.plain_text.width()
    }
}

pub fn parse_color_spec(name: &str) -> ColorSpec {
    let name = name.trim();

    if let Some(hex) = name.strip_prefix('#')
        && let Some((r, g, b)) = parse_hex_color(hex)
    {
        return ColorSpec::Rgb(r, g, b);
    }

    if let Some(inner) = name.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')'))
        && let Some((r, g, b)) = parse_rgb_func(inner)
    {
        return ColorSpec::Rgb(r, g, b);
    }

    if let Ok(n) = name.parse::<u8>() {
        return ColorSpec::EightBit(n);
    }

    match name.to_lowercase().as_str() {
        "none" | "default" | "reset" => ColorSpec::Default,
        "accent" | "primary" => ColorSpec::EightBit(39),
        "secondary" => ColorSpec::EightBit(75),
        "tertiary" => ColorSpec::EightBit(141),
        "muted" | "quaternary" => ColorSpec::EightBit(245),
        "dim" | "dimmed" => ColorSpec::EightBit(240),
        "black" => ColorSpec::EightBit(0),
        "red" | "error" | "warning" => ColorSpec::EightBit(196),
        "green" | "success" | "ok" => ColorSpec::EightBit(82),
        "yellow" => ColorSpec::EightBit(226),
        "blue" => ColorSpec::EightBit(33),
        "magenta" => ColorSpec::EightBit(201),
        "cyan" => ColorSpec::EightBit(51),
        "white" => ColorSpec::EightBit(15),
        "orange" => ColorSpec::EightBit(208),
        "gray" | "grey" => ColorSpec::EightBit(244),
        "pink" => ColorSpec::EightBit(213),
        "purple" => ColorSpec::EightBit(135),
        _ => ColorSpec::Default,
    }
}

pub fn parse_style_directive(content: &str) -> InlineStyle {
    let mut style = InlineStyle::default();

    for part in content.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        if let Some(color_str) = part.strip_prefix("fg=") {
            style.fg = parse_color_spec(color_str);
        } else if let Some(color_str) = part.strip_prefix("bg=") {
            style.bg = parse_color_spec(color_str);
        } else if part == "bold" {
            style.bold = true;
        } else if part == "dim" {
            style.dim = true;
        } else if part == "fill" {
            style.fill = true;
        } else if part == "default" || part == "none" || part == "reset" {
            style = InlineStyle::default();
        }
    }

    style
}

pub fn parse_style_literal(literal: &str) -> InlineStyle {
    let trimmed = literal.trim();
    if let Some(content) = trimmed.strip_prefix("#[").and_then(|s| s.strip_suffix(']')) {
        parse_style_directive(content)
    } else {
        parse_style_directive(trimmed)
    }
}

pub fn has_style_directive(input: &str) -> bool {
    let mut rest = input;
    while let Some(offset) = rest.find("#[") {
        let candidate = &rest[offset + 2..];
        if candidate.contains(']') {
            return true;
        }
        rest = candidate;
    }
    false
}

pub fn parse_styled_string(input: &str) -> StyledText {
    let mut result = StyledText::new();
    let mut current_style = InlineStyle::default();
    let mut literal = String::new();
    let mut index = 0;

    while index < input.len() {
        let rest = &input[index..];
        if let Some(style_body) = rest.strip_prefix("#[")
            && let Some(close_offset) = style_body.find(']')
        {
            if !literal.is_empty() {
                result.push(std::mem::take(&mut literal), current_style.clone());
            }

            current_style = parse_style_directive(&style_body[..close_offset]);
            index += 2 + close_offset + 1;
            continue;
        }

        let ch = rest.chars().next().unwrap();
        literal.push(ch);
        index += ch.len_utf8();
    }

    if !literal.is_empty() {
        result.push(literal, current_style);
    }

    result
}

pub fn apply_default_style(default_style: &InlineStyle, plain_text: &str) -> StyledText {
    let mut styled = StyledText::new();
    styled.push(plain_text.to_string(), default_style.clone());
    styled
}

fn parse_hex_color(hex: &str) -> Option<(u8, u8, u8)> {
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            Some((r, g, b))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some((r, g, b))
        }
        _ => None,
    }
}

fn parse_rgb_func(inner: &str) -> Option<(u8, u8, u8)> {
    let parts: Vec<&str> = inner.split(',').collect();
    if parts.len() != 3 {
        return None;
    }

    let r = parts[0].trim().parse::<u8>().ok()?;
    let g = parts[1].trim().parse::<u8>().ok()?;
    let b = parts[2].trim().parse::<u8>().ok()?;
    Some((r, g, b))
}

fn reverse_chars(chars: Vec<(char, InlineStyle)>) -> StyledText {
    let mut result = StyledText::new();
    for (ch, style) in chars.into_iter().rev() {
        result.push(ch.to_string(), style);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_inline_styles_to_plain_text() {
        let styled = parse_styled_string("#[fg=blue]main #[fg=red,bold]dirty");
        assert_eq!(styled.plain_text(), "main dirty");
    }

    #[test]
    fn plain_write_uses_default_style() {
        let style = parse_style_literal("#[fg=yellow]");
        let cell = CellState::from_raw("main", &style);
        assert!(!cell.had_inline_style);
        assert_eq!(cell.styled_text.segments.len(), 1);
        assert_eq!(cell.styled_text.segments[0].style, style);
    }

    #[test]
    fn ansi_output_resets_before_default_text() {
        let styled = parse_styled_string("#[fg=yellow]IDLE #[default]rest");
        assert_eq!(styled.to_ansi(), "\x1b[38;5;226mIDLE \x1b[0mrest");
    }

    #[test]
    fn truncate_end_keeps_prefix() {
        let styled = StyledText::plain("abcdefgh");
        assert_eq!(styled.truncate_end(5).plain_text(), "abcd…");
    }

    #[test]
    fn truncate_start_keeps_suffix() {
        let styled = StyledText::plain("abcdefgh");
        assert_eq!(styled.truncate_start(5).plain_text(), "…efgh");
    }

    #[test]
    fn truncation_uses_single_marker_for_tiny_widths() {
        let styled = StyledText::plain("abcdefgh");
        assert_eq!(styled.truncate_end(1).plain_text(), "…");
        assert_eq!(styled.truncate_start(1).plain_text(), "…");
    }
}
