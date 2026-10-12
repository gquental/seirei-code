use crate::{
    app::{AnswerForm, App, DraftState, Focus, Link, Mode, ProjectScreen, Row, Screen, Shortfall},
    keymap::{self, Context},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Padding, Paragraph},
};
use seirei_proto::{
    ids::ThreadId,
    model::{
        Access, CoordinatorView, JobLink, PendingRequest, Problem, ProjectView, ThreadStatus, ThreadSummary,
        login_command,
    },
    timeline::{Item, ItemKind, ItemStatus, Timeline},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SIDE_BY_SIDE: u16 = 100;

pub fn render(app: &App, frame: &mut Frame) {
    let alerts = alerts(app);
    let [top, alert_area, body, notice_area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(alerts.len() as u16),
        Constraint::Min(0),
        Constraint::Length(u16::from(app.notice.is_some())),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    title_bar(app, frame, top);
    frame.render_widget(Paragraph::new(alerts), alert_area);
    match &app.screen {
        Screen::Picker { cursor } => picker(app, *cursor, frame, body),
        Screen::Project(p) => project(p, app.shortfall(), frame, body),
    }
    if let Some(notice) = &app.notice {
        frame.render_widget(Line::styled(format!(" {notice}"), Color::Yellow), notice_area);
    }
    frame.render_widget(footer_hints(app), footer);

    match (app.context(), &app.screen) {
        (Context::Help, _) => help(app, frame),
        (_, Screen::Project(p)) => match &p.mode {
            Mode::ConfirmCreate => confirm_create(p, frame),
            Mode::Answer(_) => answer_form(p, frame),
            Mode::Browse | Mode::Compose => {}
        },
        (_, Screen::Picker { .. }) => {}
    }
}

fn title_bar(app: &App, frame: &mut Frame, area: Rect) {
    let mut crumbs = vec![Span::styled(" seirei", Modifier::BOLD)];
    if let Screen::Project(p) = &app.screen {
        let listed =
            || app.listed().find(|(machine, project)| machine.id == p.id.machine && project.id == p.id.project);
        let title = (p.view.as_deref().map(|v| v.project.title.as_str()).filter(|title| !title.is_empty()))
            .or_else(|| listed().map(|(_, project)| project.title.as_str()))
            .unwrap_or(p.id.project.as_str());
        crumbs.push(Span::raw(format!(" › {title}")));
        if let Focus::Job { thread, .. } = &p.focus {
            crumbs.push(Span::raw(format!(" › {}", thread_title(p.view.as_deref(), thread))));
        }
    }
    let (link, color) = match &app.link {
        Link::Up => ("● daemon", Color::Green),
        Link::Connecting => ("◌ connecting", Color::Yellow),
        Link::Lost(_) => ("⟳ reconnecting", Color::Red),
        Link::WrongBuild(_) => ("✗ wrong build", Color::Red),
    };
    frame.render_widget(Line::from(crumbs), area);
    frame.render_widget(Line::styled(format!("{link} "), color).right_aligned(), area);
}

fn alerts(app: &App) -> Vec<Line<'static>> {
    let banner = |text: String, bg: Color| Line::styled(format!(" {text} "), Style::new().fg(Color::Black).bg(bg));
    let mut lines = Vec::new();
    match &app.link {
        Link::Up => {}
        Link::Connecting => lines.push(banner("Connecting to the seirei daemon…".into(), Color::Yellow)),
        Link::Lost(reason) => lines
            .push(banner(format!("No daemon connection ({reason}). Retrying; what you see may be stale."), Color::Red)),
        Link::WrongBuild(daemon) => lines.push(banner(
            format!("The running daemon is another build ({}). Press r to restart it, q to quit.", daemon.0),
            Color::Red,
        )),
    }
    let view = match &app.screen {
        Screen::Project(p) => p.view.as_deref(),
        Screen::Picker { .. } => None,
    };
    if let Some((view, problem)) = view.and_then(|v| Some((v, v.health.problem.as_ref()?))) {
        let hint = match problem {
            Problem::Unauthorized => {
                format!("T3 rejected seirei's credential. Run `{}` to pair again.", login_command(&view.machine))
            }
            Problem::Unreachable { detail } => {
                format!("T3 is unreachable ({detail}). Start T3 Code; seirei catches up on its own.")
            }
            Problem::Failed { detail } => format!("Sync failed ({detail}). Retrying; what you see may be stale."),
            Problem::Unparsed { count, sample } => {
                format!("{count} records from T3 could not be read (e.g. {sample}). Everything else is current.")
            }
        };
        lines.push(Line::styled(format!(" ⚠ {hint}"), Style::new().fg(Color::Red).bold()));
    }
    lines
}

fn picker(app: &App, cursor: usize, frame: &mut Frame, area: Rect) {
    let block = Block::new().title(Line::styled("  Projects", Modifier::BOLD)).padding(Padding::top(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.listed().next().is_none() {
        let message = match app.link {
            Link::Up if app.loading_projects() => "Loading projects…",
            Link::Up => "T3 has no projects yet. Add one in T3 Code.",
            _ => "Waiting for the daemon…",
        };
        frame.render_widget(Line::styled(format!("  {message}"), dim()), inner);
        return;
    }
    let title_width = app.listed().map(|(_, p)| p.title.width()).max().unwrap_or(0);
    let rows = app.listed().map(|(_, p)| {
        let pad = " ".repeat(title_width - p.title.width());
        ListItem::new(Line::from(vec![
            Span::raw(format!("{}{pad}   ", p.title)),
            Span::styled(p.root.display().to_string(), dim()),
        ]))
    });
    let list = List::new(rows).highlight_symbol("  › ").highlight_style(Style::new().fg(Color::Cyan).bold());
    let mut state = ListState::default().with_selected(Some(cursor));
    frame.render_stateful_widget(list.repeat_highlight_symbol(false), inner, &mut state);
}

fn project(p: &ProjectScreen, shortfall: Option<Shortfall>, frame: &mut Frame, area: Rect) {
    let view = p.view.as_deref();
    let strip = view.map(questions_strip).unwrap_or_default();
    let grant = view.zip(shortfall).map(|(view, shortfall)| grant_line(view, &shortfall));
    let composer = composer_lines(p, pane("", true).inner(area).width);
    let [header, body, strip_area, grant_area, composer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(strip.len() as u16),
        Constraint::Length(u16::from(grant.is_some())),
        Constraint::Length(composer.as_ref().map_or(0, |lines| lines.len() as u16 + 2)),
    ])
    .areas(area);

    frame.render_widget(Paragraph::new(strip), strip_area);
    if let Some(line) = grant {
        frame.render_widget(line, grant_area);
    }
    if let Some(lines) = composer {
        composer_box(p, lines, frame, composer_area);
    }
    let Some(view) = view else {
        frame.render_widget(Line::styled(" Loading the project…", dim()), header);
        return;
    };
    if let Focus::Job { thread, back } = &p.focus {
        frame.render_widget(job_header(p, view, thread), header);
        let lines = |width| match p.timelines.get(thread) {
            Some(timeline) => timeline_lines(timeline, width, ["user", "agent"]),
            None => vec![Line::styled("Loading the job's timeline…", dim())],
        };
        scrolled(frame, body, " Timeline ", true, lines, *back);
        return;
    }
    frame.render_widget(project_header(view), header);
    let [left, right] = if area.width >= SIDE_BY_SIDE {
        Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]).areas(body)
    } else {
        let jobs_height = (view.jobs.len() as u16 * 2 + 2).clamp(4, body.height / 2);
        Layout::vertical([Constraint::Min(3), Constraint::Length(jobs_height)]).areas(body)
    };
    conversation(p, view, frame, left);
    jobs(p, view, frame, right);
}

fn project_header(view: &ProjectView) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    match &view.coordinator {
        CoordinatorView::Absent => spans.push(Span::styled("no coordinator", dim())),
        CoordinatorView::Starting => spans.push(Span::styled("◌ coordinator starting", Color::Yellow)),
        CoordinatorView::Ready(c) => {
            let status = status(c, &view.questions);
            spans.push(Span::styled(format!("{} coordinator {}", status.glyph, status.label), status.color));
            let mode = Access::from(c.runtime_mode).name();
            spans.push(Span::styled(format!(" · {mode} · {}", c.model.model.as_str()), dim()));
        }
    }
    spans.push(Span::styled("  ·  ", dim()));
    spans.push(match (&view.health.problem, view.health.last_sync) {
        (Some(_), _) => Span::styled("✗ sync failing", Color::Red),
        (None, None) => Span::styled("◌ not synced yet", dim()),
        (None, Some(_)) => Span::styled("✓ synced", Color::Green),
    });
    Line::from(spans)
}

fn job_header(p: &ProjectScreen, view: &ProjectView, thread: &ThreadId) -> Line<'static> {
    let job = view.jobs.iter().find(|job| &job.thread.id == thread);
    let detail = p.timelines.get(thread).and_then(|t| t.detail.as_ref());
    let Some(summary) = job.map(|job| &job.thread).or(detail.map(|d| &d.summary)) else {
        return Line::styled(" Loading the job…", dim());
    };
    let status = status(summary, &view.questions);
    let mut facts = Vec::new();
    if let Some(job) = job {
        facts.push(link_label(job.link).to_string());
    }
    facts.push(summary.model.model.as_str().to_string());
    if let Some(branch) = detail.and_then(|d| d.branch.as_ref()) {
        facts.push(format!("branch {branch}"));
    }
    if let Some(worktree) = detail.and_then(|d| d.worktree.as_ref()) {
        facts.push(worktree.display().to_string());
    }
    if let Some(n) = job.map(|job| job.subtasks.len()).filter(|n| *n > 0) {
        facts.push(subtasks_label(n));
    }
    let mut spans = vec![Span::styled(format!(" {} {}", status.glyph, status.label), status.color)];
    spans.extend(facts.into_iter().map(|fact| Span::styled(format!(" · {fact}"), dim())));
    Line::from(spans)
}

fn conversation(p: &ProjectScreen, view: &ProjectView, frame: &mut Frame, area: Rect) {
    let lines = |width| match &view.coordinator {
        CoordinatorView::Absent => vec![
            Line::raw("No coordinator yet."),
            Line::styled("Press c to create one. It plans the work and launches jobs.", dim()),
        ],
        CoordinatorView::Starting => vec![Line::styled("The coordinator is starting…", dim())],
        CoordinatorView::Ready(c) => match p.timelines.get(&c.id) {
            Some(timeline) => timeline_lines(timeline, width, ["you", "coordinator"]),
            None => vec![Line::styled("Loading the conversation…", dim())],
        },
    };
    let focused = p.focus == Focus::Conversation && p.mode == Mode::Browse;
    scrolled(frame, area, " Conversation ", focused, lines, p.conversation_back);
}

fn scrolled(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    focused: bool,
    layout: impl FnOnce(u16) -> Vec<Line<'static>>,
    back: usize,
) {
    let mut block = pane(title, focused);
    let inner = block.inner(area);
    let mut lines = layout(inner.width);
    let height = inner.height as usize;
    let back = back.min(lines.len().saturating_sub(height));
    let end = lines.len() - back;
    if back > 0 {
        block = block.title_bottom(Line::styled(format!(" ↓ {back} more lines "), Color::Cyan).right_aligned());
    }
    lines.truncate(end);
    lines.drain(..end.saturating_sub(height));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn jobs(p: &ProjectScreen, view: &ProjectView, frame: &mut Frame, area: Rect) {
    let focused = p.focus == Focus::Jobs && p.mode == Mode::Browse;
    let block = pane(&format!(" Jobs {} ", view.jobs.len()), focused);
    if view.jobs.is_empty() {
        let empty = Paragraph::new(Line::styled("No jobs yet. The coordinator launches them.", dim()));
        frame.render_widget(empty.block(block), area);
        return;
    }
    let width = block.inner(area).width.saturating_sub(1) as usize;
    let rows = view.jobs.iter().map(|job| {
        let status = status(&job.thread, &view.questions);
        let title = truncate(display_title(&job.thread.title), width.saturating_sub(2));
        let mut facts = format!(" · {}", link_label(job.link));
        if !job.subtasks.is_empty() {
            facts.push_str(&format!(" · {}", subtasks_label(job.subtasks.len())));
        }
        ListItem::new(vec![
            Line::from(vec![Span::styled(format!("{} ", status.glyph), status.color), Span::raw(title)]),
            Line::from(vec![Span::styled(format!("  {}", status.label), status.color), Span::styled(facts, dim())]),
        ])
    });
    let selected = p.selected.as_ref().and_then(|id| view.jobs.iter().position(|job| &job.thread.id == id));
    let highlight = if focused { Style::new().bg(Color::DarkGray) } else { Style::new() };
    let list =
        List::new(rows).block(block).highlight_style(highlight).highlight_symbol("▌").repeat_highlight_symbol(true);
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(list, area, &mut state);
}

fn questions_strip(view: &ProjectView) -> Vec<Line<'static>> {
    let Some(oldest) = view.questions.first() else { return vec![] };
    let count = view.questions.len();
    let asks = oldest.questions.first().map_or("", |q| if q.prompt.is_empty() { &q.header } else { &q.prompt });
    let lead = if count == 1 { "1 open question".to_string() } else { format!("{count} open questions") };
    let who = thread_title(Some(view), &oldest.thread);
    vec![Line::from(vec![
        Span::styled(format!(" ? {lead}"), Style::new().fg(Color::Magenta).bold()),
        Span::styled(format!(" · {who} asks: "), dim()),
        Span::raw(clean(asks).replace('\n', " ")),
    ])]
}

fn grant_line(view: &ProjectView, shortfall: &Shortfall) -> Line<'static> {
    let text = match shortfall {
        Shortfall::Launch => format!(
            "This login is read-only, so it cannot create a coordinator. Run `{} --access full-access`.",
            login_command(&view.machine)
        ),
        Shortfall::Thread { thread, mode, access } => {
            let who = match &view.coordinator {
                CoordinatorView::Ready(c) if &c.id == thread => "The coordinator".to_string(),
                _ => format!("\"{}\"", thread_title(Some(view), thread)),
            };
            let grant = match access {
                Access::ReadOnly => "this login is read-only".to_string(),
                access => format!("this login allows {}", access.name()),
            };
            let needed = Access::from(*mode).name();
            format!("{who} runs at {needed}; {grant}. Run `{} --access {needed}`.", login_command(&view.machine))
        }
    };
    Line::styled(format!(" ⚠ {text}"), Color::Yellow)
}

fn composer_lines(p: &ProjectScreen, width: u16) -> Option<Vec<String>> {
    if p.mode != Mode::Compose && p.draft.state == DraftState::Editing {
        return None;
    }
    let mut lines = wrap(&p.draft.text, width as usize);
    let overflow = lines.len().saturating_sub(6);
    lines.drain(..overflow);
    Some(lines)
}

fn composer_box(p: &ProjectScreen, lines: Vec<String>, frame: &mut Frame, area: Rect) {
    let composing = p.mode == Mode::Compose;
    let mut block = pane(" Message the coordinator ", composing);
    block = match &p.draft.state {
        DraftState::Editing => block,
        DraftState::Sending(_) => block.title_bottom(Line::styled(" sending… ", Color::Yellow).right_aligned()),
        DraftState::Failed(e) => {
            let retry = if e.retry_safe { " Enter resends." } else { "" };
            block.title_bottom(Line::styled(format!(" ✗ {}{retry} ", e.message), Color::Red).right_aligned())
        }
    };
    let inner = block.inner(area);
    if composing && !matches!(p.draft.state, DraftState::Sending(_)) {
        let spaces = p.draft.text.len() - p.draft.text.trim_end_matches(' ').len();
        let column = (lines.last().map_or(0, |l| l.width()) + spaces).min(inner.width as usize) as u16;
        let row = lines.len().saturating_sub(1) as u16;
        frame.set_cursor_position(Position::new(inner.x + column, inner.y + row));
    }
    let text: Vec<Line> = lines.into_iter().map(Line::raw).collect();
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn footer_hints(app: &App) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (keys, action, help) in keymap::hints(app.context()) {
        if app.live(action) {
            spans.push(Span::styled(keys, Modifier::BOLD));
            spans.push(Span::styled(format!(" {help}   "), dim()));
        }
    }
    Line::from(spans)
}

fn help(app: &App, frame: &mut Frame) {
    let mut entries = keymap::hints(app.screen_context());
    entries.extend(keymap::hints(Context::Global));
    let key_width = entries.iter().map(|(keys, _, _)| keys.width()).max().unwrap_or(0);
    let lines: Vec<Line> = (entries.into_iter())
        .map(|(keys, _, help)| {
            let pad = " ".repeat(key_width - keys.width());
            Line::from(vec![Span::styled(format!("{keys}{pad}  "), Modifier::BOLD), Span::raw(help)])
        })
        .collect();
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4;
    overlay(frame, " Keys ", width.max(30), lines);
}

fn confirm_create(p: &ProjectScreen, frame: &mut Frame) {
    let title = p.view.as_deref().map_or("this project", |v| v.project.title.as_str());
    let lines = vec![
        Line::raw(format!("{title} has no coordinator. Create one?")),
        Line::default(),
        Line::styled("It is a T3 thread that plans the work and launches jobs,", dim()),
        Line::styled("running at your credential's runtime ceiling.", dim()),
        Line::default(),
        Line::from(vec![
            Span::styled("y", Modifier::BOLD),
            Span::raw(" create   "),
            Span::styled("n", Modifier::BOLD),
            Span::raw(" cancel"),
        ]),
    ];
    overlay(frame, " Create a coordinator ", 64, lines);
}

fn answer_form(p: &ProjectScreen, frame: &mut Frame) {
    let Some((form, request, question)) = p.answering() else { return };
    let width = frame.area().width.saturating_sub(4).min(84);
    let text_width = width.saturating_sub(4) as usize;
    let mut lines = vec![Line::styled(question.header.clone(), Modifier::BOLD)];
    lines.extend(wrap(&question.prompt, text_width).into_iter().map(Line::raw));
    if question.multi_select {
        lines.push(Line::styled("Pick any number with space.", dim()));
    }
    lines.push(Line::default());
    let mut cursor = None;
    for row in AnswerForm::rows(question) {
        let here = row == form.row;
        let marker = if here { "› " } else { "  " };
        let style = if here { Style::new().fg(Color::Cyan).bold() } else { Style::new() };
        match row {
            Row::Choice(i) => {
                let choice = &question.options[i];
                let check = match (question.multi_select, form.toggled.contains(&i)) {
                    (false, _) => "",
                    (true, true) => "[x] ",
                    (true, false) => "[ ] ",
                };
                lines.push(Line::styled(format!("{marker}{check}{}", choice.label), style));
                let indent = 4 + check.len();
                for l in wrap(&choice.description, text_width.saturating_sub(indent)) {
                    if !l.is_empty() {
                        lines.push(Line::styled(format!("{}{l}", " ".repeat(indent)), dim()));
                    }
                }
            }
            Row::Custom => {
                let typed = truncate(&form.custom, text_width.saturating_sub(10));
                if here {
                    cursor = Some((lines.len() as u16, (2 + 7 + typed.width()) as u16));
                }
                lines.push(Line::from(vec![Span::styled(format!("{marker}Other: "), style), Span::raw(typed)]));
            }
        }
    }
    let n = request.questions.len();
    let who = thread_title(p.view.as_deref(), &request.thread);
    let title = format!(" Question {} of {n} · {who} ", form.question + 1);
    let area = overlay(frame, &title, width, lines);
    if let Some((row, col)) = cursor {
        frame.set_cursor_position(Position::new(area.x + 2 + col, area.y + 1 + row));
    }
}

fn overlay(frame: &mut Frame, title: &str, width: u16, lines: Vec<Line>) -> Rect {
    let screen = frame.area();
    let height = (lines.len() as u16 + 2).min(screen.height);
    let area = screen.centered(Constraint::Length(width.min(screen.width)), Constraint::Length(height));
    let block =
        Block::bordered().title(Line::styled(title.to_string(), Modifier::BOLD)).padding(Padding::horizontal(1));
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
    area
}

fn pane(title: &str, focused: bool) -> Block<'static> {
    let border = if focused { Style::new().fg(Color::Cyan) } else { Style::new().fg(Color::DarkGray) };
    Block::bordered()
        .border_style(border)
        .title(Line::styled(title.to_string(), Style::new().fg(Color::Reset).bold()))
        .padding(Padding::horizontal(1))
}

fn timeline_lines(timeline: &Timeline, width: u16, speakers: [&str; 2]) -> Vec<Line<'static>> {
    let width = width as usize;
    let mut lines = Vec::new();
    if timeline.earlier > 0 {
        lines.push(Line::styled(format!("{} earlier items are in T3", timeline.earlier), dim()));
    }
    let mut speaker = None;
    for item in &timeline.items {
        let who = usize::from(item.kind != ItemKind::UserMessage);
        if speaker != Some(who) {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::styled(speakers[who].to_string(), Modifier::BOLD));
            speaker = Some(who);
        }
        match item.kind {
            ItemKind::UserMessage | ItemKind::AssistantMessage => {
                lines.extend(
                    wrap(&item.text.shown, width.saturating_sub(2)).into_iter().map(|l| Line::raw(format!("  {l}"))),
                );
                if item.text.more_at.is_some() {
                    lines.push(Line::styled("  … the rest is in T3", dim()));
                }
            }
            _ => lines.push(item_line(item, width)),
        }
    }
    lines
}

fn item_line(item: &Item, width: usize) -> Line<'static> {
    let label = match &item.kind {
        ItemKind::UserMessage => "you",
        ItemKind::AssistantMessage => "said",
        ItemKind::Reasoning => "thought",
        ItemKind::CommandExecution => "ran",
        ItemKind::DynamicTool => "tool",
        ItemKind::FileChange => "edited",
        ItemKind::UserInputRequest => "asked",
        ItemKind::ApprovalRequest => "needs approval in T3:",
        ItemKind::Subagent => "subagent",
        ItemKind::Checkpoint => "checkpoint",
        ItemKind::Notification => "note",
        ItemKind::Other(raw) => raw,
    };
    let tool = match item.kind {
        ItemKind::DynamicTool => tool_name(&item.text.shown),
        _ => None,
    };
    let detail = (item.title.as_deref().filter(|t| !t.trim().is_empty()))
        .or(tool)
        .or_else(|| item.text.shown.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("");
    let (suffix, style) = match item.status {
        ItemStatus::Failed => (" (failed)", Style::new().fg(Color::Red)),
        ItemStatus::Running | ItemStatus::Pending => (" …", Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM)),
        ItemStatus::Waiting => (" (waiting)", Style::new().fg(Color::Magenta)),
        _ => ("", dim()),
    };
    Line::styled(truncate(&format!("  · {label} {}{suffix}", detail.trim()), width), style)
}

/// The tool a `dynamic_tool` item ran, without an MCP tool's `mcp__<server>__` prefix. T3's
/// text is JSON that opens with `toolName` and may be cut short, so the name is read as a
/// prefix rather than parsed.
fn tool_name(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix('{')?.trim_start().strip_prefix("\"toolName\"")?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let name = &rest[..rest.find('"')?];
    let name = name.strip_prefix("mcp__").and_then(|n| n.split_once("__")).map_or(name, |(_, tool)| tool);
    (!name.is_empty()).then_some(name)
}

struct Status {
    glyph: &'static str,
    label: &'static str,
    color: Color,
}

/// `Waiting` with no open question means a tool approval, which only T3's own UI can grant.
fn status(thread: &ThreadSummary, questions: &[PendingRequest]) -> Status {
    let (glyph, label, color) = match thread.status {
        ThreadStatus::Waiting if questions.iter().any(|r| r.thread == thread.id) => {
            ("?", "needs an answer", Color::Magenta)
        }
        ThreadStatus::Waiting => ("!", "needs approval in T3", Color::Magenta),
        ThreadStatus::Running => ("●", "running", Color::Yellow),
        ThreadStatus::Starting => ("◌", "starting", Color::Yellow),
        ThreadStatus::Preparing => ("◌", "preparing", Color::Yellow),
        ThreadStatus::Queued => ("◌", "queued", Color::Yellow),
        ThreadStatus::Idle => ("○", "idle", Color::Gray),
        ThreadStatus::Completed => ("✓", "completed", Color::Green),
        ThreadStatus::Failed => ("✗", "failed", Color::Red),
        ThreadStatus::Interrupted => ("–", "interrupted", Color::DarkGray),
        ThreadStatus::Cancelled => ("–", "cancelled", Color::DarkGray),
        ThreadStatus::RolledBack => ("–", "rolled back", Color::DarkGray),
    };
    Status { glyph, label, color }
}

fn link_label(link: JobLink) -> &'static str {
    match link {
        JobLink::Launched => "launched",
        JobLink::Subagent => "subagent",
    }
}

fn subtasks_label(n: usize) -> String {
    if n == 1 { "1 subtask".into() } else { format!("{n} subtasks") }
}

fn display_title(title: &str) -> &str {
    match title.rfind("[job:") {
        Some(at) if title.ends_with(']') => title[..at].trim_end(),
        _ => title,
    }
}

fn thread_title<'a>(view: Option<&'a ProjectView>, thread: &'a ThreadId) -> &'a str {
    let Some(view) = view else { return thread.as_str() };
    if let CoordinatorView::Ready(c) = &view.coordinator
        && &c.id == thread
    {
        return "Coordinator";
    }
    let threads = view.jobs.iter().flat_map(|job| std::iter::once(&job.thread).chain(&job.subtasks));
    threads.into_iter().find(|t| &t.id == thread).map_or(thread.as_str(), |t| display_title(&t.title))
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn clean(text: &str) -> String {
    text.replace('\t', "    ").chars().filter(|c| *c == '\n' || !c.is_control()).collect()
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in clean(text).split('\n') {
        let (mut line, mut used) = (String::new(), 0);
        for word in paragraph.split_inclusive(' ') {
            if used > 0 && used + word.trim_end_matches(' ').width() > width {
                lines.push(line.trim_end().to_string());
                (line, used) = (String::new(), 0);
            }
            for c in word.chars() {
                let w = c.width().unwrap_or(0);
                if used + w > width {
                    if c == ' ' {
                        continue;
                    }
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += w;
            }
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

fn truncate(text: &str, width: usize) -> String {
    let text = clean(text.lines().next().unwrap_or(""));
    if text.width() <= width {
        return text;
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::Event, fixtures::*};
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::{Terminal, backend::TestBackend};
    use seirei_proto::{
        ids::{ItemId, MachineId, T3Time},
        model::Health,
        timeline::ItemText,
        wire::{BuildId, CallId, Reply, ServerMsg},
    };
    use std::sync::Arc;

    fn item(kind: &str, title: Option<&str>, text: &str) -> Item {
        Item {
            id: ItemId::new(format!("{kind}:{text}")),
            kind: ItemKind::from(kind.to_string()),
            status: ItemStatus::Completed,
            run: None,
            title: title.map(Into::into),
            text: ItemText { shown: text.into(), more_at: None },
            updated_at: T3Time(0),
        }
    }

    fn screen(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..height).map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>());
        rows.map(|row| row.trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    fn press(app: &mut App, code: KeyCode) {
        app.update(Event::Key(KeyEvent::from(code)));
    }

    fn pane_view() -> ProjectView {
        let mut subagent = job("j2", "Fix the parser");
        subagent.link = JobLink::Subagent;
        subagent.thread.status = ThreadStatus::Waiting;
        let mut launched = job("j1", "Add a thumbnail view [job:7QK2]");
        launched.subtasks =
            vec![thread("j1a", "helper", ThreadStatus::Idle), thread("j1b", "helper", ThreadStatus::Idle)];
        ready_view(vec![launched, subagent], vec![])
    }

    fn pane_app() -> App {
        let mut app = app_in_project(pane_view());
        let long = "I'll split this into two jobs. The first adds a thumbnail view to the gallery so that every \
                    image shows a preview, and the second fixes the parser that chokes on nested lists.";
        let items = vec![
            item("user_message", None, "add a thumbnail view and fix the parser"),
            item("reasoning", None, "Two independent changes.\nThey can run in parallel."),
            item("command_execution", Some("cargo test -p gallery"), ""),
            item("assistant_message", None, long),
            item("context_compaction", None, "compacted 40 items"),
        ];
        app.update(Event::Server(ServerMsg::Timeline(Arc::new(timeline(COORDINATOR, items)))));
        app
    }

    #[test]
    fn the_picker_lists_projects_and_the_link() {
        let mut app = new_app(None);
        assert!(screen(&app, 80, 10).contains("Connecting to the seirei daemon"));
        app.update(Event::Server(ServerMsg::Welcome));
        let projects = local(vec![project_meta("a", "alpha"), project_meta("b", "seirei-code")]);
        app.update(Event::Server(ServerMsg::Reply { id: CallId(1), result: Ok(Reply::Projects(projects)) }));
        let shown = screen(&app, 80, 10);
        assert!(shown.contains("› alpha"), "{shown}");
        assert!(shown.contains("seirei-code   /src/seirei-code"), "{shown}");
        assert!(shown.contains("● daemon"), "{shown}");
        assert!(shown.contains("enter open project"), "{shown}");
        assert!(!shown.contains("Connecting"), "{shown}");
    }

    #[test]
    fn the_pane_shows_the_conversation_and_jobs_without_tags() {
        let shown = screen(&pane_app(), 140, 40);
        for expected in [
            "seirei › seirei-code",
            "○ coordinator idle · full-access · claude-opus-5-5",
            "you",
            "  add a thumbnail view and fix the parser",
            "  · thought Two independent changes.",
            "  · ran cargo test -p gallery",
            "  · context_compaction compacted 40 items",
            "nested lists.",
            "Jobs 2",
            "Add a thumbnail view",
            "running · launched · 2 subtasks",
            "Fix the parser",
            "i message the coordinator",
        ] {
            assert!(shown.contains(expected), "missing {expected:?} in\n{shown}");
        }
        assert!(!shown.contains("[job:"), "{shown}");
        assert!(!shown.contains("create coordinator"), "a dead binding is not hinted\n{shown}");
    }

    #[test]
    fn a_waiting_thread_without_questions_needs_approval_in_t3() {
        let shown = screen(&pane_app(), 140, 40);
        assert!(shown.contains("! Fix the parser"), "{shown}");
        assert!(shown.contains("needs approval in T3 · subagent"), "{shown}");
    }

    #[test]
    fn an_unauthorized_daemon_says_how_to_fix_it() {
        let mut view = pane_view();
        view.health = Health { last_sync: None, problem: Some(Problem::Unauthorized) };
        let app = app_in_project(view);
        let shown = screen(&app, 140, 20);
        assert!(shown.contains("Run `seirei login`"), "{shown}");
        assert!(shown.contains("✗ sync failing"), "{shown}");
    }

    #[test]
    fn a_rejected_credential_names_the_login_of_its_machine() {
        let mut view = pane_view();
        view.machine = MachineId::parse("macbook").unwrap();
        view.health = Health { last_sync: None, problem: Some(Problem::Unauthorized) };
        let shown = screen(&app_in_project(view.clone()), 140, 20);
        assert!(shown.contains("Run `seirei login macbook` to pair again."), "{shown}");
        view.health.problem = None;
        view.access = Some(Access::Auto);
        let shown = screen(&app_in_project(view), 140, 40);
        assert!(shown.contains("Run `seirei login macbook --access full-access`."), "{shown}");
    }

    #[test]
    fn a_binding_the_grant_kills_names_the_login_that_revives_it() {
        let mut view = pane_view();
        view.access = Some(Access::Auto);
        let shown = screen(&app_in_project(view), 140, 40);
        let hint =
            "The coordinator runs at full-access; this login allows auto. Run `seirei login --access full-access`.";
        assert!(shown.contains(hint), "{shown}");
        assert!(!shown.contains("message the coordinator"), "{shown}");
    }

    #[test]
    fn a_failed_sync_shows_its_cause_in_the_header() {
        let mut view = pane_view();
        let detail = "T3 rejected the call: thread_not_found".to_string();
        view.health = Health { last_sync: None, problem: Some(Problem::Failed { detail }) };
        let shown = screen(&app_in_project(view), 140, 20);
        assert!(shown.contains("⚠ Sync failed (T3 rejected the call: thread_not_found). Retrying"), "{shown}");
        assert!(shown.contains("✗ sync failing"), "{shown}");
    }

    #[test]
    fn an_unreachable_t3_is_named_once_with_its_cause() {
        let mut view = pane_view();
        let detail = "http://127.0.0.1:3773/mcp: error sending request".to_string();
        view.health = Health { last_sync: None, problem: Some(Problem::Unreachable { detail }) };
        let shown = screen(&app_in_project(view), 140, 20);
        let banner = "⚠ T3 is unreachable (http://127.0.0.1:3773/mcp: error sending request). Start T3 Code;";
        assert!(shown.contains(banner), "{shown}");
        assert_eq!(shown.matches("unreachable").count(), 1, "{shown}");
    }

    #[test]
    fn a_project_t3_no_longer_lists_keeps_its_name_and_says_why() {
        let mut app = app_in_project(view(CoordinatorView::Absent, vec![], vec![]));
        let mut unlisted = view(CoordinatorView::Absent, vec![], vec![]);
        unlisted.project.title = String::new();
        let detail = "T3 has no project p".to_string();
        unlisted.health = Health { last_sync: None, problem: Some(Problem::Failed { detail }) };
        app.update(Event::Server(ServerMsg::Project(Arc::new(unlisted))));
        let shown = screen(&app, 100, 12);
        assert!(shown.contains("seirei › seirei-code"), "the picker's title stands in for the empty one\n{shown}");
        assert!(shown.contains("⚠ Sync failed (T3 has no project p). Retrying"), "{shown}");
    }

    #[test]
    fn a_wrong_build_banner_keeps_the_pane_on_screen() {
        let mut app = pane_app();
        app.update(Event::Server(ServerMsg::WrongBuild { daemon: BuildId("0.1.0+old".into()) }));
        let shown = screen(&app, 140, 30);
        assert!(shown.contains("another build (0.1.0+old). Press r to restart it"), "{shown}");
        assert!(shown.contains("Add a thumbnail view"), "{shown}");
        assert!(shown.contains("r restart the daemon"), "{shown}");
    }

    #[test]
    fn composing_shows_the_draft_and_its_fate() {
        let mut app = pane_app();
        press(&mut app, KeyCode::Char('i'));
        for c in "ship it".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        let shown = screen(&app, 140, 40);
        assert!(shown.contains("Message the coordinator"), "{shown}");
        assert!(shown.contains("ship it"), "{shown}");
        assert!(shown.contains("sending…"), "{shown}");
        app.update(Event::Disconnected("daemon exited".into()));
        let shown = screen(&app, 140, 40);
        assert!(shown.contains("Enter resends."), "{shown}");
        assert!(shown.contains("No daemon connection (daemon exited)"), "{shown}");
    }

    #[test]
    fn a_long_draft_wraps_in_six_rows_ending_at_the_cursor() {
        let mut app = app_in_project(ready_view(vec![], vec![]));
        press(&mut app, KeyCode::Char('i'));
        for c in (1..=60).map(|n| format!("word{n} ")).collect::<String>().chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let shown = screen(&app, 40, 24);
        let rows: Vec<&str> = shown.lines().collect();
        let top = rows.iter().position(|row| row.contains("Message the coordinator")).unwrap();
        let bottom = top + rows[top..].iter().position(|row| row.starts_with('└')).unwrap();
        assert_eq!(bottom - top - 1, 6, "the composer grows to six rows and no further\n{shown}");
        assert!(!shown.contains("word1 "), "the oldest rows scroll away\n{shown}");
        let y = rows.iter().position(|row| row.contains("word60")).expect("the end of the draft is in view");
        let x = rows[y][..rows[y].find("word60").unwrap()].chars().count() + "word60 ".len();
        let cursor = terminal.get_cursor_position().unwrap();
        assert_eq!(cursor, Position::new(x as u16, y as u16), "the cursor follows the typed space\n{shown}");
    }

    #[test]
    fn an_untitled_tool_call_names_the_tool_it_ran() {
        let mut app = app_in_project(ready_view(vec![], vec![]));
        let launch = "{\n  \"toolName\": \"mcp__t3-code__t3_thread_launch\",\n  \"input\": {\n    \"title\": \"Count";
        let items = vec![item("dynamic_tool", None, launch), item("dynamic_tool", None, "{\n  \"toolNa")];
        app.update(Event::Server(ServerMsg::Timeline(Arc::new(timeline(COORDINATOR, items)))));
        let shown = screen(&app, 100, 20);
        assert!(shown.contains("· tool t3_thread_launch "), "{shown}");
        assert!(shown.contains("· tool {"), "a name T3 cut short falls back to the text\n{shown}");
    }

    #[test]
    fn tool_name_reads_the_leading_key_of_whole_or_cut_json() {
        let whole = "{\n  \"toolName\": \"mcp__t3-code__t3_thread_list\",\n  \"input\": {\n    \"limit\": 5\n  },\n  \
                     \"output\": \"{\\\"threads\\\":[]}\"\n}";
        assert_eq!(tool_name(whole), Some("t3_thread_list"));
        let cut = "{\n  \"toolName\": \"mcp__t3-code__t3_project_list\",\n  \"input\": {\n    \"limit\": 20\n  },\n  \
                   \"output\": \"{\\\"projects\\\":[{\\\"id\\\":\\\"3ff3";
        assert_eq!(tool_name(cut), Some("t3_project_list"));
        assert_eq!(tool_name(r#"{"toolName":"Read","input":{}}"#), Some("Read"), "a tool outside MCP keeps its name");
        assert_eq!(tool_name("{\n  \"toolName\": \"mcp__t3-co"), None, "cut inside the name");
        assert_eq!(tool_name(r#"{"input":{},"toolName":"Read"}"#), None, "toolName is not the first key");
        assert_eq!(tool_name(r#"{"toolName":"","input":{}}"#), None);
        assert_eq!(tool_name("plain text"), None);
    }

    #[test]
    fn help_lists_every_binding_of_the_context() {
        let mut app = pane_app();
        press(&mut app, KeyCode::Char('?'));
        let shown = screen(&app, 140, 40);
        for expected in ["Keys", "tab", "switch pane", "c", "create coordinator", "ctrl-c", "j/↓"] {
            assert!(shown.contains(expected), "missing {expected:?} in\n{shown}");
        }
    }

    #[test]
    fn the_job_screen_shows_its_timeline_full_width() {
        let mut app = pane_app();
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Enter);
        let items =
            vec![item("user_message", None, "Add thumbnails."), item("file_change", Some("src/gallery.rs"), "")];
        app.update(Event::Server(ServerMsg::Timeline(Arc::new(timeline("j1", items)))));
        let shown = screen(&app, 140, 30);
        assert!(shown.contains("seirei › seirei-code › Add a thumbnail view"), "{shown}");
        assert!(shown.contains("● running · launched · gpt-6.1-sol · 2 subtasks"), "{shown}");
        assert!(shown.contains("· edited src/gallery.rs"), "{shown}");
        assert!(!shown.contains("Jobs 2"), "{shown}");
    }

    #[test]
    fn wrap_keeps_words_whole_and_breaks_long_ones() {
        assert_eq!(wrap("the quick brown fox", 9), ["the quick", "brown fox"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("    indented\n\nx", 20), ["    indented", "", "x"]);
        assert_eq!(wrap("a\u{1b}[31mred", 20), ["a[31mred"]);
        assert_eq!(truncate("a long title", 6), "a lon…");
        assert_eq!(display_title("Add a view [job:7QK2]"), "Add a view");
    }
}
