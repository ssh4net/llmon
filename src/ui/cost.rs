//! COST screen: the cost of local usage at the price tables of
//! `crate::pricing`, by day, model, and project, for one harness or for all
//! of them.

use super::{range_days, truncate_middle, view_snapshots};
use crate::app::{AppState, DayRange};
use crate::harness::Harness;
use crate::locale::DisplayFormatter;
use crate::pricing::{CostBreakdown, PriceTable, Pricing, CODEX_USD_PER_CREDIT};
use crate::usage::{model_display_name, normalize_project_key, LocalUsageSnapshot, UsageZone};
use chrono::NaiveDate;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;
use std::collections::{BTreeMap, HashMap};
use unicode_width::UnicodeWidthStr;

const MAX_PROJECT_ROWS: usize = 12;
const MIDDLE_DOT: &str = "\u{00b7}";
const FULL_BLOCK: &str = "\u{2588}";
const SQUARE: &str = "\u{25a0}";

/// Cost of one harness per day of the range.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HarnessCost {
    pub(crate) harness: Harness,
    pub(crate) per_day: Vec<f64>,
    pub(crate) total: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelCost {
    pub(crate) harness: Harness,
    pub(crate) model: String,
    pub(crate) cost: CostBreakdown,
}

/// Costs of the selected range and harnesses.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct CostView {
    pub(crate) days: Vec<String>,
    pub(crate) harnesses: Vec<HarnessCost>,
    /// Largest first.
    pub(crate) models: Vec<ModelCost>,
    /// Across harnesses, largest first.
    pub(crate) projects: Vec<(String, f64)>,
    /// Models without a price, with their tokens in the range.
    pub(crate) unpriced: Vec<(Harness, String, i64)>,
    /// Models priced by family: model id and the table id used.
    pub(crate) by_family: Vec<(Harness, String, String)>,
}

impl CostView {
    pub(crate) fn total(&self) -> f64 {
        self.harnesses.iter().map(|harness| harness.total).sum()
    }

    fn day_total(&self, index: usize) -> f64 {
        self.harnesses
            .iter()
            .map(|harness| harness.per_day.get(index).copied().unwrap_or(0.0))
            .sum()
    }
}

pub(crate) fn cost_view(
    snapshots: &[(Harness, &LocalUsageSnapshot)],
    zone: UsageZone,
    range: DayRange,
    pricing: &Pricing,
) -> CostView {
    let days = range_days(snapshots, zone, range);
    let day_index: HashMap<&str, usize> = days
        .iter()
        .enumerate()
        .map(|(index, day)| (day.as_str(), index))
        .collect();
    let mut view = CostView::default();
    let mut projects: HashMap<String, (String, f64)> = HashMap::new();
    for (harness, snapshot) in snapshots {
        let table = pricing.table(*harness);
        let mut per_day = vec![0.0; days.len()];
        for series in snapshot.model_daily_for_zone(zone) {
            let mut cost = CostBreakdown::default();
            let mut unpriced_tokens = 0_i64;
            for (day, tokens) in &series.days {
                let Some(index) = day_index.get(day.as_str()) else {
                    continue;
                };
                match table.cost(&series.model, *tokens) {
                    Some(day_cost) => {
                        per_day[*index] += day_cost.total();
                        cost.add(day_cost);
                    }
                    None => unpriced_tokens += tokens.total(),
                }
            }
            if unpriced_tokens > 0 {
                view.unpriced
                    .push((*harness, series.model.clone(), unpriced_tokens));
            }
            if cost.total() > 0.0 {
                if let Some(matched) = table
                    .price_for(&series.model)
                    .filter(|matched| matched.by_family)
                {
                    view.by_family
                        .push((*harness, series.model.clone(), matched.id.to_string()));
                }
                view.models.push(ModelCost {
                    harness: *harness,
                    model: series.model.clone(),
                    cost,
                });
            }
        }
        for project in snapshot.project_model_daily_for_zone(zone) {
            let cost: f64 = project
                .models
                .iter()
                .flat_map(|series| {
                    series
                        .days
                        .iter()
                        .filter(|(day, _)| day_index.contains_key(day.as_str()))
                        .filter_map(|(_, tokens)| table.cost(&series.model, *tokens))
                })
                .map(|cost| cost.total())
                .sum();
            if cost > 0.0 {
                let entry = projects
                    .entry(normalize_project_key(&project.project))
                    .or_insert_with(|| (project.project.clone(), 0.0));
                if project.project.len() < entry.0.len() {
                    entry.0 = project.project.clone();
                }
                entry.1 += cost;
            }
        }
        let total = per_day.iter().sum();
        view.harnesses.push(HarnessCost {
            harness: *harness,
            per_day,
            total,
        });
    }
    view.days = days;
    view.models
        .sort_by(|left, right| right.cost.total().total_cmp(&left.cost.total()));
    view.projects = projects.into_values().collect();
    view.projects
        .sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
    view
}

pub(crate) fn format_usd(value: f64, formatter: DisplayFormatter<'_>) -> String {
    if value.abs() >= 1000.0 {
        format!("${}", formatter.format_count(value.round() as i64))
    } else {
        format!("${}", formatter.format_two_decimals(value))
    }
}

fn format_credits(usd: f64, formatter: DisplayFormatter<'_>) -> String {
    let credits = usd / CODEX_USD_PER_CREDIT;
    format!("{} credits", formatter.format_count(credits.round() as i64))
}

fn boxed(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .padding(Padding {
            left: 1,
            right: 1,
            top: 0,
            bottom: 0,
        })
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(Color::Gray),
        ))
}

/// How each harness's cost is calculated, for the summary.
fn source_line(harness: Harness, table: &PriceTable) -> String {
    let overrides = if table.overrides > 0 {
        format!(", {} from config.json", table.overrides)
    } else {
        String::new()
    };
    match harness {
        Harness::Codex => format!(
            "Codex: OpenAI API prices = credit rate card x ${} (table {}{overrides}).",
            CODEX_USD_PER_CREDIT, table.updated
        ),
        Harness::Claude => format!(
            "Claude: API-equivalent at Anthropic list prices (table {}{overrides}).",
            table.updated
        ),
    }
}

pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let snapshots = view_snapshots(state);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(snapshots.len().max(1) as u16 + 2),
            Constraint::Min(6),
            Constraint::Length(3),
        ])
        .split(area);
    let range = state.cost_range;
    super::render_range_controls(
        frame,
        chunks[0],
        state,
        range,
        crate::app::UiClickAction::SetCostRange,
    );

    let formatter = state.formatter();
    if snapshots.is_empty() {
        frame.render_widget(
            Paragraph::new("Indexing local sessions...").block(boxed("COST")),
            chunks[2],
        );
        return;
    }
    let pairs: Vec<(Harness, &LocalUsageSnapshot)> = snapshots
        .iter()
        .map(|(harness, snapshot)| (*harness, snapshot.as_ref()))
        .collect();
    let view = cost_view(&pairs, state.usage_zone, range, &state.pricing);
    let colors: BTreeMap<Harness, Color> = snapshots
        .iter()
        .map(|(harness, _)| (*harness, state.harness_colors(*harness).1))
        .collect();

    render_summary(frame, chunks[1], state, &view, range, &colors, formatter);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(chunks[2]);
    render_daily(frame, columns[0], &view, &colors, formatter);
    let model_rows = (view.models.len() as u16)
        .saturating_add(3)
        .min(columns[1].height.saturating_sub(4).max(4));
    let tables = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(model_rows), Constraint::Min(3)])
        .split(columns[1]);
    render_models(frame, tables[0], &view, &colors, formatter);
    render_projects(
        frame,
        tables[1],
        &view,
        state.accent_text_color(),
        formatter,
    );
    render_notes(frame, chunks[3], &view, formatter);
}

fn render_summary(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    view: &CostView,
    range: DayRange,
    colors: &BTreeMap<Harness, Color>,
    formatter: DisplayFormatter<'_>,
) {
    let total = view.total();
    let today = view
        .days
        .len()
        .checked_sub(1)
        .map(|last| view.day_total(last))
        .unwrap_or(0.0);
    let active_days = (0..view.days.len())
        .filter(|index| view.day_total(*index) > 0.0)
        .count();
    let average = if active_days > 0 {
        total / active_days as f64
    } else {
        0.0
    };
    let accent = state.accent_text_color();
    let mut spans = vec![
        Span::styled(
            format!("{} ", range.label().to_uppercase()),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            format_usd(total, formatter),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
    ];
    if let [single] = &view.harnesses[..] {
        if single.harness == Harness::Codex {
            spans.push(Span::styled(
                format!(" = {}", format_credits(total, formatter)),
                Style::default().fg(Color::Gray),
            ));
        }
    } else {
        for harness in &view.harnesses {
            spans.push(Span::raw("   "));
            spans.push(Span::styled(
                format!("{SQUARE} {} ", harness_label(harness.harness)),
                Style::default().fg(colors[&harness.harness]),
            ));
            spans.push(Span::raw(format_usd(harness.total, formatter)));
        }
    }
    spans.push(Span::raw(format!(
        "   TODAY {}   AVG / ACTIVE DAY {}",
        format_usd(today, formatter),
        format_usd(average, formatter)
    )));
    let mut lines = vec![Line::from(spans)];
    for harness in &view.harnesses {
        lines.push(Line::from(Span::styled(
            source_line(harness.harness, state.pricing.table(harness.harness)),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(Span::styled(
        "Subscription usage is not billed per token. Add or change prices under `pricing` in config.json.",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn harness_label(harness: Harness) -> &'static str {
    match harness {
        Harness::Codex => "CODEX",
        Harness::Claude => "CLAUDE",
    }
}

/// Cost per day; in the combined view each bar is split by harness color.
fn render_daily(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &CostView,
    colors: &BTreeMap<Harness, Color>,
    formatter: DisplayFormatter<'_>,
) {
    let rows = area.height.saturating_sub(2) as usize;
    let first = view.days.len().saturating_sub(rows);
    let max = (first..view.days.len())
        .map(|index| view.day_total(index))
        .fold(0.0_f64, f64::max)
        .max(0.01);
    let labels: Vec<String> = view.days[first..]
        .iter()
        .map(|day| {
            NaiveDate::parse_from_str(day, "%Y-%m-%d")
                .map(|date| formatter.format_short_date(date))
                .unwrap_or_else(|_| day.clone())
        })
        .collect();
    let values: Vec<String> = (first..view.days.len())
        .map(|index| format_usd(view.day_total(index), formatter))
        .collect();
    let label_width = labels
        .iter()
        .map(|label| UnicodeWidthStr::width(label.as_str()))
        .max()
        .unwrap_or(0);
    let value_width = values
        .iter()
        .map(|value| UnicodeWidthStr::width(value.as_str()))
        .max()
        .unwrap_or(0);
    let bar_width = (area.width as usize)
        .saturating_sub(label_width + value_width + 6)
        .max(1);
    let lines: Vec<Line<'static>> = (first..view.days.len())
        .zip(labels.iter().zip(&values))
        .map(|(index, (label, value))| {
            let mut spans = vec![Span::styled(
                format!("{label:<label_width$} "),
                Style::default().fg(Color::Gray),
            )];
            // Each harness's segment ends where the running total ends, so
            // rounding never makes the bar longer than its total.
            let mut drawn = 0usize;
            let mut running = 0.0;
            for harness in &view.harnesses {
                running += harness.per_day.get(index).copied().unwrap_or(0.0);
                let end = ((running / max) * bar_width as f64).round() as usize;
                let end = end.min(bar_width);
                if end > drawn {
                    spans.push(Span::styled(
                        FULL_BLOCK.repeat(end - drawn),
                        Style::default().fg(colors[&harness.harness]),
                    ));
                    drawn = end;
                }
            }
            spans.push(Span::raw(" ".repeat(bar_width - drawn)));
            spans.push(Span::raw(format!(" {value:>value_width$}")));
            Line::from(spans)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(boxed("BY DAY")),
        area,
    );
}

fn render_models(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &CostView,
    colors: &BTreeMap<Harness, Color>,
    formatter: DisplayFormatter<'_>,
) {
    let total = view.total();
    let combined = view.harnesses.len() > 1;
    // Codex logs have no cache writes.
    let cache_write = |model: &ModelCost| match model.harness {
        Harness::Codex => "--".to_string(),
        Harness::Claude => format_usd(model.cost.cache_write, formatter),
    };
    let rows: Vec<[String; 7]> = view
        .models
        .iter()
        .map(|model| {
            let share = if total > 0.0 {
                model.cost.total() / total * 100.0
            } else {
                0.0
            };
            [
                model_display_name(&model.model),
                format_usd(model.cost.input, formatter),
                cache_write(model),
                format_usd(model.cost.cache_read, formatter),
                format_usd(model.cost.output, formatter),
                format_usd(model.cost.total(), formatter),
                format!("{}%", formatter.format_one_decimal(share)),
            ]
        })
        .collect();
    let header = [
        "MODEL", "INPUT", "CACHE-W", "CACHE-R", "OUTPUT", "TOTAL", "SHARE",
    ];
    let mut widths: [usize; 7] = header.map(UnicodeWidthStr::width);
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(UnicodeWidthStr::width(cell.as_str()));
        }
    }
    widths[0] = widths[0].min(18);
    let marker = if combined { 2 } else { 0 };
    let inner_width = usize::from(area.width.saturating_sub(4));
    let full_width = marker + widths.iter().sum::<usize>() + widths.len() - 1;
    // Narrow panels keep the model, total, and share columns.
    let columns: Vec<usize> = if full_width <= inner_width {
        (0..7).collect()
    } else {
        vec![0, 5, 6]
    };
    let format_row = |cells: [&str; 7]| -> String {
        columns
            .iter()
            .map(|column| {
                let width = widths[*column];
                if *column == 0 {
                    format!("{:<width$}", truncate_middle(cells[0], width))
                } else {
                    format!("{:>width$}", cells[*column])
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("{}{}", " ".repeat(marker), format_row(header)),
        Style::default().fg(Color::Gray),
    ))];
    for (model, row) in view.models.iter().zip(&rows) {
        let mut spans = Vec::new();
        if combined {
            spans.push(Span::styled(
                format!("{SQUARE} "),
                Style::default().fg(colors[&model.harness]),
            ));
        }
        spans.push(Span::raw(format_row(row.each_ref().map(String::as_str))));
        lines.push(Line::from(spans));
    }
    if view.models.is_empty() {
        lines.push(Line::from("--"));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(boxed("BY MODEL")),
        area,
    );
}

fn render_projects(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &CostView,
    accent: Color,
    formatter: DisplayFormatter<'_>,
) {
    let width = area.width.saturating_sub(18) as usize;
    let mut lines: Vec<Line<'static>> = view
        .projects
        .iter()
        .take(MAX_PROJECT_ROWS.min(area.height.saturating_sub(2) as usize))
        .map(|(project, cost)| {
            Line::from(vec![
                Span::raw(format!("{:<width$}", truncate_middle(project, width))),
                Span::styled(
                    format!(" {:>12}", format_usd(*cost, formatter)),
                    Style::default().fg(accent),
                ),
            ])
        })
        .collect();
    if lines.is_empty() {
        lines.push(Line::from("--"));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(boxed("BY PROJECT")),
        area,
    );
}

/// Models priced by family and models without a price.
fn render_notes(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &CostView,
    formatter: DisplayFormatter<'_>,
) {
    let mut lines = Vec::new();
    if !view.by_family.is_empty() {
        let names: Vec<String> = view
            .by_family
            .iter()
            .map(|(_, model, priced_as)| format!("{model} as {priced_as}"))
            .collect();
        lines.push(Line::from(Span::styled(
            format!("Priced by family: {}.", names.join(", ")),
            Style::default().fg(Color::DarkGray),
        )));
    }
    if !view.unpriced.is_empty() {
        let names: Vec<String> = view
            .unpriced
            .iter()
            .map(|(harness, model, tokens)| {
                format!(
                    "{model} ({} {MIDDLE_DOT} {} tokens)",
                    harness.key(),
                    formatter.format_count(*tokens)
                )
            })
            .collect();
        lines.push(Line::from(Span::styled(
            format!(
                "Unpriced, not counted: {}. Add them under `pricing` in config.json.",
                names.join(", ")
            ),
            Style::default().fg(Color::Yellow),
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{ModelDailyUsage, ProjectModelDaily, TokenBreakdown};

    fn snapshot_with(
        models: Vec<ModelDailyUsage>,
        projects: Vec<ProjectModelDaily>,
    ) -> LocalUsageSnapshot {
        let mut snapshot = crate::ui::tests::empty_usage_snapshot();
        snapshot.model_daily = models;
        snapshot.project_model_daily = projects;
        snapshot
    }

    fn series(model: &str, days: &[(String, TokenBreakdown)]) -> ModelDailyUsage {
        ModelDailyUsage {
            model: model.to_string(),
            days: days.iter().cloned().collect(),
        }
    }

    #[test]
    fn cost_view_prices_harnesses_models_projects_and_flags_unpriced_models() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let today = chrono::Local::now().date_naive();
        let key = |days_ago: i64| {
            (today - chrono::Duration::days(days_ago))
                .format("%Y-%m-%d")
                .to_string()
        };
        let million_output = TokenBreakdown {
            output: 1_000_000,
            ..TokenBreakdown::default()
        };
        // Claude Sonnet 5: $10 per million output tokens.
        let claude_days = [(key(1), million_output), (key(0), million_output)];
        let claude = snapshot_with(
            vec![
                series("claude-sonnet-5", &claude_days),
                series("mystery-model", &claude_days),
            ],
            vec![ProjectModelDaily {
                project: "/work/app".to_string(),
                models: vec![series("claude-sonnet-5", &claude_days)],
            }],
        );
        // gpt-5.1-codex-max is priced as gpt-5.1: $10 per million output.
        let codex_days = [(key(0), million_output), (key(40), million_output)];
        let codex = snapshot_with(
            vec![series("gpt-5.1-codex-max", &codex_days)],
            vec![ProjectModelDaily {
                project: "/work/app/".to_string(),
                models: vec![series("gpt-5.1-codex-max", &codex_days)],
            }],
        );

        let view = cost_view(
            &[(Harness::Codex, &codex), (Harness::Claude, &claude)],
            UsageZone::Local,
            DayRange::Last7Days,
            &Pricing::default(),
        );
        assert_eq!(view.days.len(), 7);
        assert_eq!(
            view.harnesses[0].total, 10.0,
            "the 40-day-old usage is outside"
        );
        assert_eq!(view.harnesses[1].total, 20.0);
        assert_eq!(view.day_total(6), 20.0);
        assert_eq!(view.total(), 30.0);
        assert_eq!(view.models[0].model, "claude-sonnet-5");
        assert_eq!(
            view.by_family,
            vec![(
                Harness::Codex,
                "gpt-5.1-codex-max".to_string(),
                "gpt-5.1".to_string()
            )]
        );
        assert_eq!(view.projects, vec![("/work/app".to_string(), 30.0)]);
        assert_eq!(
            view.unpriced,
            vec![(Harness::Claude, "mystery-model".to_string(), 2_000_000)]
        );
        assert_eq!(format_usd(view.total(), formatter), "$30.00");
        assert_eq!(format_usd(1234.4, formatter), "$1,234");
        assert_eq!(format_credits(10.0, formatter), "250 credits");

        let all = cost_view(
            &[(Harness::Codex, &codex)],
            UsageZone::Local,
            DayRange::AllTime,
            &Pricing::default(),
        );
        assert_eq!(all.days.len(), 41);
        assert_eq!(all.total(), 20.0);
    }
}
