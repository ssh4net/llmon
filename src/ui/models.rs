//! MODELS screen: tokens per day per model, for one harness or for all of
//! them.

use super::{range_days, view_snapshots};
use crate::app::{AppState, UiClickAction};
use crate::harness::Harness;
use crate::locale::DisplayFormatter;
use crate::usage::{
    format_compact_kmb, format_tokens_compact, model_display_name, LocalUsageSnapshot,
    ModelDailyUsage, TokenBreakdown, UsageZone,
};
use chrono::NaiveDate;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Axis, Block, BorderType, Borders, Chart, Dataset, GraphType, Paragraph};
use ratatui::Frame;

const MAX_CHART_MODELS: usize = 6;
const MIDDLE_DOT: &str = "\u{00b7}";
const DOT: &str = "\u{25cf}";

/// One model's usage over the selected range.
#[derive(Debug)]
pub(crate) struct ModelRangeUsage<'a> {
    pub(crate) harness: Harness,
    pub(crate) series: &'a ModelDailyUsage,
    pub(crate) total: TokenBreakdown,
}

/// Models with usage since `first_day`, largest first, across the given
/// harnesses.
pub(crate) fn models_in_range<'a>(
    snapshots: &[(Harness, &'a LocalUsageSnapshot)],
    zone: UsageZone,
    first_day: Option<&str>,
) -> Vec<ModelRangeUsage<'a>> {
    let mut models: Vec<ModelRangeUsage<'a>> = snapshots
        .iter()
        .flat_map(|(harness, snapshot)| {
            snapshot
                .model_daily_for_zone(zone)
                .iter()
                .map(move |series| ModelRangeUsage {
                    harness: *harness,
                    series,
                    total: series.total_since(first_day),
                })
        })
        .filter(|usage| usage.total.total() > 0)
        .collect();
    models.sort_by(|left, right| {
        right
            .total
            .total()
            .cmp(&left.total.total())
            .then_with(|| left.series.model.cmp(&right.series.model))
    });
    models
}

fn series_color(index: usize, accent: Color) -> Color {
    [
        accent,
        Color::LightGreen,
        Color::Yellow,
        Color::LightMagenta,
        Color::LightBlue,
        Color::LightRed,
    ][index % MAX_CHART_MODELS]
}

fn boxed(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(Color::Gray),
        ))
}

pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let range = state.models_range;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(4)])
        .split(area);
    super::render_range_controls(
        frame,
        chunks[0],
        state,
        range,
        UiClickAction::SetModelsRange,
    );
    let area = chunks[1];

    let accent = state.accent_text_color();
    let formatter = state.formatter();
    let snapshots = view_snapshots(state);
    let pairs: Vec<(Harness, &LocalUsageSnapshot)> = snapshots
        .iter()
        .map(|(harness, snapshot)| (*harness, snapshot.as_ref()))
        .collect();
    if pairs.is_empty() {
        frame.render_widget(
            Paragraph::new("Indexing local sessions...").block(boxed("MODELS")),
            area,
        );
        return;
    }
    let zone = state.usage_zone;
    let days = range_days(&pairs, zone, range);
    let models = models_in_range(&pairs, zone, days.first().map(String::as_str));
    if days.is_empty() || models.is_empty() {
        frame.render_widget(
            Paragraph::new("No model usage in this range.").block(boxed("MODELS")),
            area,
        );
        return;
    }

    let card_rows = models.len().div_ceil(2) as u16 * 4;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),
            Constraint::Length(1),
            Constraint::Length(card_rows.min(area.height / 2).max(4)),
        ])
        .split(area);
    render_chart(frame, chunks[0], &days, &models, accent, formatter);
    render_legend(frame, chunks[1], &models, accent);
    let combined = pairs.len() > 1;
    render_cards(frame, chunks[2], &models, accent, combined, formatter);
}

fn render_chart(
    frame: &mut Frame<'_>,
    area: Rect,
    days: &[String],
    models: &[ModelRangeUsage<'_>],
    accent: Color,
    formatter: DisplayFormatter<'_>,
) {
    let points: Vec<Vec<(f64, f64)>> = models
        .iter()
        .take(MAX_CHART_MODELS)
        .map(|usage| {
            days.iter()
                .enumerate()
                .map(|(index, day)| {
                    let tokens = usage
                        .series
                        .days
                        .get(day)
                        .map(|tokens| tokens.total())
                        .unwrap_or(0);
                    (index as f64, tokens as f64)
                })
                .collect()
        })
        .collect();
    let max_value = points
        .iter()
        .flatten()
        .map(|(_, value)| *value)
        .fold(0.0_f64, f64::max)
        .max(1.0);

    let datasets: Vec<Dataset<'_>> = points
        .iter()
        .enumerate()
        .map(|(index, data)| {
            Dataset::default()
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(series_color(index, accent)))
                .data(data)
        })
        .collect();

    let y_label = |value: f64| format_compact_kmb(value.round() as u64, 8, formatter);
    let x_label = |index: usize| {
        days.get(index)
            .and_then(|day| NaiveDate::parse_from_str(day, "%Y-%m-%d").ok())
            .map(|date| formatter.format_short_date(date))
            .unwrap_or_default()
    };
    let last_index = days.len().saturating_sub(1);
    let x_max = (last_index as f64).max(1.0);
    let chart = Chart::new(datasets)
        .block(boxed("TOKENS PER DAY"))
        .legend_position(None)
        .x_axis(
            Axis::default()
                .bounds([0.0, x_max])
                .style(Style::default().fg(Color::Gray))
                .labels(vec![
                    Line::from(x_label(0)),
                    Line::from(x_label(last_index / 2)),
                    Line::from(x_label(last_index)),
                ]),
        )
        .y_axis(
            Axis::default()
                .bounds([0.0, max_value * 1.05])
                .style(Style::default().fg(Color::Gray))
                .labels(vec![
                    Line::from("0"),
                    Line::from(y_label(max_value / 2.0)),
                    Line::from(y_label(max_value)),
                ]),
        );
    frame.render_widget(chart, area);
}

fn render_legend(frame: &mut Frame<'_>, area: Rect, models: &[ModelRangeUsage<'_>], accent: Color) {
    let mut spans = vec![Span::raw(" ")];
    for (index, usage) in models.iter().take(MAX_CHART_MODELS).enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                format!(" {MIDDLE_DOT} "),
                Style::default().fg(Color::Gray),
            ));
        }
        spans.push(Span::styled(
            DOT,
            Style::default().fg(series_color(index, accent)),
        ));
        spans.push(Span::raw(format!(
            " {}",
            model_display_name(&usage.series.model)
        )));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// One card per model: share, input (with cache), output, and the cache
/// split. In the combined view each card names its harness.
fn render_cards(
    frame: &mut Frame<'_>,
    area: Rect,
    models: &[ModelRangeUsage<'_>],
    accent: Color,
    combined: bool,
    formatter: DisplayFormatter<'_>,
) {
    let grand_total: i64 = models.iter().map(|usage| usage.total.total()).sum();
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let mut column_lines: [Vec<Line<'static>>; 2] = [Vec::new(), Vec::new()];
    for (index, usage) in models.iter().enumerate() {
        let share = if grand_total > 0 {
            usage.total.total() as f64 / grand_total as f64 * 100.0
        } else {
            0.0
        };
        let tokens = |value: i64| format_tokens_compact(value, formatter);
        let color = if index < MAX_CHART_MODELS {
            series_color(index, accent)
        } else {
            Color::DarkGray
        };
        let harness = if combined {
            format!(" {MIDDLE_DOT} {}", usage.harness.key())
        } else {
            String::new()
        };
        let input = usage
            .total
            .input
            .saturating_add(usage.total.cache_write)
            .saturating_add(usage.total.cache_read);
        // Codex logs have no cache writes.
        let cache = match usage.harness {
            Harness::Codex => format!("   Cache: {} read", tokens(usage.total.cache_read)),
            Harness::Claude => format!(
                "   Cache: {} read {MIDDLE_DOT} {} write",
                tokens(usage.total.cache_read),
                tokens(usage.total.cache_write)
            ),
        };
        let lines = &mut column_lines[index % 2];
        lines.push(Line::from(vec![
            Span::styled(format!(" {DOT} "), Style::default().fg(color)),
            Span::styled(
                model_display_name(&usage.series.model),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" ({}%{harness})", formatter.format_one_decimal(share)),
                Style::default().fg(Color::Gray),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!(
                "   In: {} {MIDDLE_DOT} Out: {}",
                tokens(input),
                tokens(usage.total.output)
            ),
            Style::default().fg(Color::Gray),
        )));
        lines.push(Line::from(Span::styled(
            cache,
            Style::default().fg(Color::Gray),
        )));
        lines.push(Line::from(""));
    }
    for (column, lines) in columns.iter().zip(column_lines) {
        frame.render_widget(Paragraph::new(Text::from(lines)), *column);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_in_range_merges_harnesses_largest_first() {
        let today = chrono::Local::now().date_naive();
        let key = |days_ago: i64| {
            (today - chrono::Duration::days(days_ago))
                .format("%Y-%m-%d")
                .to_string()
        };
        let tokens = |output: i64| TokenBreakdown {
            output,
            ..TokenBreakdown::default()
        };
        let mut codex = crate::ui::tests::empty_usage_snapshot();
        codex.model_daily = vec![ModelDailyUsage {
            model: "gpt-test".to_string(),
            days: [(key(0), tokens(50)), (key(20), tokens(500))]
                .into_iter()
                .collect(),
        }];
        let mut claude = crate::ui::tests::empty_usage_snapshot();
        claude.model_daily = vec![ModelDailyUsage {
            model: "claude-test-1".to_string(),
            days: [(key(1), tokens(100))].into_iter().collect(),
        }];
        let pairs = [(Harness::Codex, &codex), (Harness::Claude, &claude)];

        let week = models_in_range(&pairs, UsageZone::Local, Some(&key(6)));
        let names: Vec<(&str, i64)> = week
            .iter()
            .map(|usage| (usage.series.model.as_str(), usage.total.total()))
            .collect();
        assert_eq!(names, vec![("claude-test-1", 100), ("gpt-test", 50)]);
        let all = models_in_range(&pairs, UsageZone::Local, None);
        assert_eq!(all[0].series.model, "gpt-test");
        assert_eq!(all[0].harness, Harness::Codex);
    }
}
