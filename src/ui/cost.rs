//! COST screen: the cost of local usage at the price tables of
//! `crate::pricing`, by day, model, and project, for one harness or for all
//! of them.

use super::{range_days, truncate_middle, view_snapshots};
use crate::app::{AppState, DayRange, MachineScope, UiClickAction, UiHitTarget};
use crate::harness::Harness;
use crate::locale::DisplayFormatter;
use crate::pricing::{CostBreakdown, PriceTable, Pricing, CODEX_USD_PER_CREDIT};
use crate::sync::{MachineInfo, SyncFile};
use crate::usage::{model_display_name, normalize_project_key, LocalUsageSnapshot, UsageZone};
use chrono::NaiveDate;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

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

/// One machine's tokens and cost over the range (distributed mode).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MachineCost {
    pub(crate) name: String,
    pub(crate) os: String,
    pub(crate) this_pc: bool,
    /// Unix seconds of the machine's last file; `None` for this machine.
    pub(crate) updated_at: Option<i64>,
    /// The machine's local calendar is in another time zone, so its UTC days
    /// stand in for its local days.
    pub(crate) utc_days: bool,
    pub(crate) tokens: i64,
    pub(crate) cost: f64,
}

/// The cost and tokens of this machine and of each other machine over `days`
/// (consecutive day keys of the range), largest cost first. `local` are this
/// machine's own snapshots, not the merged ones.
pub(crate) fn machine_costs(
    local: &[(Harness, &LocalUsageSnapshot)],
    this: &MachineInfo,
    peers: &[Arc<SyncFile>],
    zone: UsageZone,
    days: &[String],
    pricing: &Pricing,
) -> Vec<MachineCost> {
    let in_range: HashSet<&str> = days.iter().map(String::as_str).collect();
    let mut this_cost = MachineCost {
        name: this.name.clone(),
        os: this.os.clone(),
        this_pc: true,
        updated_at: None,
        utc_days: false,
        tokens: 0,
        cost: 0.0,
    };
    for (harness, snapshot) in local {
        let table = pricing.table(*harness);
        for series in snapshot.model_daily_for_zone(zone) {
            for (day, tokens) in &series.days {
                if !in_range.contains(day.as_str()) {
                    continue;
                }
                this_cost.tokens = this_cost.tokens.saturating_add(tokens.total());
                if let Some(cost) = table.cost(&series.model, *tokens) {
                    this_cost.cost += cost.total();
                }
            }
        }
    }
    let local_offset = crate::sync::local_utc_offset_minutes();
    let mut machines = vec![this_cost];
    for peer in peers {
        let same_calendar = peer.machine.utc_offset_minutes == local_offset;
        // A peer in another time zone is counted by its UTC days.
        let prefix = if zone == UsageZone::Local && same_calendar {
            "L:"
        } else {
            "U:"
        };
        let mut machine = MachineCost {
            name: peer.machine.name.clone(),
            os: peer.machine.os.clone(),
            this_pc: false,
            updated_at: Some(peer.generated_at),
            utc_days: zone == UsageZone::Local && !same_calendar,
            tokens: 0,
            cost: 0.0,
        };
        for harness in crate::harness::ALL_HARNESSES {
            let Some(groups) = peer.usage.get(harness.key()) else {
                continue;
            };
            let table = pricing.table(harness);
            for group in groups {
                for (key, models) in &group.models {
                    let Some(day) = key.strip_prefix(prefix) else {
                        continue;
                    };
                    if !in_range.contains(day) {
                        continue;
                    }
                    for (model, tokens) in models {
                        machine.tokens = machine.tokens.saturating_add(tokens.total());
                        if let Some(cost) = table.cost(model, *tokens) {
                            machine.cost += cost.total();
                        }
                    }
                }
            }
        }
        machines.push(machine);
    }
    machines.sort_by(|left, right| {
        right
            .cost
            .total_cmp(&left.cost)
            .then_with(|| left.name.cmp(&right.name))
    });
    machines
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
    super::render_range_controls(frame, chunks[0], state, range, UiClickAction::SetCostRange);

    // The project list needs `state` mutably, so the formatter borrows a
    // copy of the locale.
    let system_locale = state.system_locale.clone();
    let formatter = DisplayFormatter::new(state.display_style, &system_locale);
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
    // In distributed mode, with other machines shown: each machine's share.
    let machines = match state.sync.as_ref() {
        Some(sync) if state.machine_scope == MachineScope::All && !sync.peers.is_empty() => {
            let local: Vec<(Harness, Arc<LocalUsageSnapshot>)> =
                super::view_harnesses(state.harness_view)
                    .iter()
                    .filter_map(|harness| {
                        let snapshot = match harness {
                            Harness::Codex => state.codex_usage.clone(),
                            Harness::Claude => state.claude_usage.clone(),
                        };
                        snapshot.map(|snapshot| (*harness, snapshot))
                    })
                    .collect();
            let local: Vec<(Harness, &LocalUsageSnapshot)> = local
                .iter()
                .map(|(harness, snapshot)| (*harness, snapshot.as_ref()))
                .collect();
            machine_costs(
                &local,
                &sync.settings.machine,
                &sync.peers,
                state.usage_zone,
                &view.days,
                &state.pricing,
            )
        }
        _ => Vec::new(),
    };
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
    let machine_rows = if machines.is_empty() {
        0
    } else {
        (machines.len() as u16).saturating_add(3)
    };
    let tables = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(model_rows),
            Constraint::Length(machine_rows),
            Constraint::Min(3),
        ])
        .split(columns[1]);
    render_models(frame, tables[0], &view, &colors, formatter);
    if !machines.is_empty() {
        render_machines(frame, tables[1], &machines, formatter);
    }
    render_projects(frame, tables[2], state, &view, formatter);
    render_notes(frame, chunks[3], &view, &machines, formatter);
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

/// Cost per project with a selected row, so a path and its cost read as one
/// line. Up/Down, the wheel, or a click move the selection; the list scrolls
/// to keep it in view.
/// Tokens and cost per machine, with how old each machine's data is.
fn render_machines(
    frame: &mut Frame<'_>,
    area: Rect,
    machines: &[MachineCost],
    formatter: DisplayFormatter<'_>,
) {
    let total: f64 = machines.iter().map(|machine| machine.cost).sum();
    let now = unix_now();
    let header = format!(
        "{:<20} {:<12} {:>10} {:>9} {:>11} {:>6}",
        "MACHINE", "OS", "UPDATED", "TOKENS", "COST", "SHARE"
    );
    let mut lines = vec![Line::from(Span::styled(
        header,
        Style::default().fg(Color::Gray),
    ))];
    for machine in machines {
        let name = if machine.this_pc {
            format!("{} (this PC)", machine.name)
        } else {
            machine.name.clone()
        };
        let (updated, stale) = match machine.updated_at {
            None => ("now".to_string(), false),
            Some(at) => {
                let age = now.saturating_sub(at).max(0);
                (
                    format!("{} ago", super::format_reset_countdown(age)),
                    age > STALE_MACHINE_SECS,
                )
            }
        };
        let share = if total > 0.0 {
            machine.cost / total * 100.0
        } else {
            0.0
        };
        let text = format!(
            "{:<20} {:<12} {:>10} {:>9} {:>11} {:>5}%",
            truncate_middle(&name, 20),
            truncate_middle(&machine.os, 12),
            updated,
            crate::usage::format_tokens_compact(machine.tokens, formatter),
            format_usd(machine.cost, formatter),
            formatter.format_one_decimal(share),
        );
        let style = if stale {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(text, style)));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(boxed("BY MACHINE")),
        area,
    );
}

/// A machine whose file is older than this is flagged as stale.
const STALE_MACHINE_SECS: i64 = 24 * 60 * 60;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn render_projects(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut AppState,
    view: &CostView,
    formatter: DisplayFormatter<'_>,
) {
    let accent = state.accent_text_color();
    let count = view.projects.len();
    let rows = usize::from(area.height.saturating_sub(2));
    state.cost_project_count = count;
    state.cost_projects_area = Some(area);
    let selected = (count > 0).then(|| state.cost_projects.selected().unwrap_or(0).min(count - 1));
    state.cost_projects.select(selected);
    let mut offset = state.cost_projects.offset();
    if let Some(selected) = selected {
        if selected < offset {
            offset = selected;
        } else if rows > 0 && selected >= offset + rows {
            offset = selected + 1 - rows;
        }
    }
    offset = offset.min(count.saturating_sub(rows));
    *state.cost_projects.offset_mut() = offset;

    let width = area.width.saturating_sub(18) as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows);
    for (row, (project, cost)) in view.projects.iter().enumerate().skip(offset).take(rows) {
        let path = format!("{:<width$}", truncate_middle(project, width));
        let cost = format!(" {:>12} ", format_usd(*cost, formatter));
        lines.push(if Some(row) == selected {
            let style = Style::default()
                .fg(Color::Black)
                .bg(accent)
                .add_modifier(Modifier::BOLD);
            Line::from(vec![Span::styled(path, style), Span::styled(cost, style)])
        } else {
            Line::from(vec![
                Span::raw(path),
                Span::styled(cost, Style::default().fg(accent)),
            ])
        });
        state.ui_hit_targets.push(UiHitTarget {
            area: Rect::new(
                area.x.saturating_add(1),
                area.y.saturating_add(1 + (row - offset) as u16),
                area.width.saturating_sub(2),
                1,
            ),
            action: UiClickAction::SelectCostProject(row),
        });
    }
    if lines.is_empty() {
        lines.push(Line::from("--"));
    }
    let title = match selected {
        Some(selected) => format!("BY PROJECT {}/{count}", selected + 1),
        None => "BY PROJECT".to_string(),
    };
    frame.render_widget(Paragraph::new(Text::from(lines)).block(boxed(&title)), area);
}

/// Models priced by family and models without a price.
fn render_notes(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &CostView,
    machines: &[MachineCost],
    formatter: DisplayFormatter<'_>,
) {
    let mut lines = Vec::new();
    let other_zone: Vec<&str> = machines
        .iter()
        .filter(|machine| machine.utc_days)
        .map(|machine| machine.name.as_str())
        .collect();
    if !other_zone.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "In another time zone, counted by UTC days: {}.",
                other_zone.join(", ")
            ),
            Style::default().fg(Color::DarkGray),
        )));
    }
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

    fn peer(name: &str, utc_offset_minutes: i32, models: &[(&str, &str, i64)]) -> Arc<SyncFile> {
        // (day key, model, output tokens), for the Codex harness.
        let mut group_models: BTreeMap<String, BTreeMap<String, TokenBreakdown>> = BTreeMap::new();
        for (key, model, output) in models {
            group_models.entry(key.to_string()).or_default().insert(
                model.to_string(),
                TokenBreakdown {
                    output: *output,
                    ..TokenBreakdown::default()
                },
            );
        }
        Arc::new(SyncFile {
            format: crate::sync::SYNC_FORMAT.to_string(),
            version: crate::sync::SYNC_VERSION,
            machine: MachineInfo {
                id: "b".repeat(32),
                name: name.to_string(),
                os: "Windows".to_string(),
                llmon: "0.5.6".to_string(),
                utc_offset_minutes,
            },
            generated_at: 77,
            usage: BTreeMap::from([(
                "codex".to_string(),
                vec![crate::usage::ExportGroup {
                    cwd: None,
                    daily: BTreeMap::new(),
                    models: group_models,
                }],
            )]),
            sessions: BTreeMap::new(),
        })
    }

    #[test]
    fn machine_costs_split_the_range_by_machine() {
        let today = chrono::Local::now().date_naive();
        let day = |days_ago: i64| {
            (today - chrono::Duration::days(days_ago))
                .format("%Y-%m-%d")
                .to_string()
        };
        let million = TokenBreakdown {
            output: 1_000_000,
            ..TokenBreakdown::default()
        };
        let mut local = crate::ui::tests::empty_usage_snapshot();
        local.model_daily = vec![ModelDailyUsage {
            model: "gpt-5.1".to_string(),
            days: [(day(0), million), (day(40), million)]
                .into_iter()
                .collect(),
        }];
        let this = MachineInfo {
            id: "a".repeat(32),
            name: "this-pc".to_string(),
            os: "Linux".to_string(),
            llmon: "0.5.6".to_string(),
            utc_offset_minutes: crate::sync::local_utc_offset_minutes(),
        };
        let same_zone = crate::sync::local_utc_offset_minutes();
        let l = |days_ago: i64| format!("L:{}", day(days_ago));
        let u = |days_ago: i64| format!("U:{}", day(days_ago));
        // Same calendar: its local days count. gpt-5.1 is $10 per million output.
        let near = peer(
            "pc-near",
            same_zone,
            &[
                (&l(1), "gpt-5.1", 3_000_000),
                (&u(1), "gpt-5.1", 999_000_000),
            ],
        );
        // Another time zone: only its UTC days count.
        let far = peer(
            "pc-far",
            same_zone + 60,
            &[
                (&l(2), "gpt-5.1", 999_000_000),
                (&u(2), "gpt-5.1", 2_000_000),
                (&u(30), "unpriced-model", 5),
            ],
        );
        let days: Vec<String> = (0..7).rev().map(day).collect();

        let machines = machine_costs(
            &[(Harness::Codex, &local)],
            &this,
            &[near, far],
            UsageZone::Local,
            &days,
            &Pricing::default(),
        );
        let summary: Vec<(&str, bool, i64, f64, bool)> = machines
            .iter()
            .map(|m| (m.name.as_str(), m.this_pc, m.tokens, m.cost, m.utc_days))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("pc-near", false, 3_000_000, 30.0, false),
                ("pc-far", false, 2_000_000, 20.0, true),
                ("this-pc", true, 1_000_000, 10.0, false),
            ],
            "largest cost first; the 40-day-old day is outside the range"
        );
        assert_eq!(machines[0].updated_at, Some(77));
        assert_eq!(machines[2].updated_at, None);
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
