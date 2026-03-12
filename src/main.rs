use std::io::{self, stdout};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Margin};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};
use ratatui::Terminal;

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
}

#[derive(Debug, Clone)]
struct ChangedFile {
    path: String,
    status: FileStatus,
    staged: bool,
    added: i32,
    removed: i32,
}

#[derive(Debug, Clone, Default)]
struct GitState {
    staged: Vec<ChangedFile>,
    unstaged: Vec<ChangedFile>,
    untracked: Vec<ChangedFile>,
}

fn fetch_branch() -> String {
    run_git(&["branch", "--show-current"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            run_git(&["rev-parse", "--short", "HEAD"])
                .map(|s| format!("({})", s.trim()))
                .unwrap_or_else(|| "(detached)".to_string())
        })
}

impl GitState {
    fn all_files(&self) -> Vec<&ChangedFile> {
        self.staged
            .iter()
            .chain(self.unstaged.iter())
            .chain(self.untracked.iter())
            .collect()
    }

    fn file_at_index(&self, idx: usize) -> Option<&ChangedFile> {
        self.all_files().into_iter().nth(idx)
    }

    fn find_index(&self, path: &str) -> Option<usize> {
        self.all_files().into_iter().position(|f| f.path == path)
    }
}

enum AppView {
    List,
    Diff,
}

struct App {
    git_state: GitState,
    branch: String,
    list_state: ListState,
    view: AppView,
    diff_lines: Vec<String>,
    diff_scroll: usize,
    diff_viewport_height: usize,
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

fn run_git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}

fn parse_status_char(c: char) -> FileStatus {
    match c {
        'M' => FileStatus::Modified,
        'A' => FileStatus::Added,
        'D' => FileStatus::Deleted,
        'R' => FileStatus::Renamed,
        _ => FileStatus::Modified,
    }
}

/// Parse `git diff --stat` output to extract per-file +/- counts.
fn parse_diff_stat(stat_output: &str) -> std::collections::HashMap<String, (i32, i32)> {
    let mut map = std::collections::HashMap::new();
    for line in stat_output.lines() {
        let parts: Vec<&str> = line.splitn(2, '|').collect();
        if parts.len() != 2 {
            continue;
        }
        let path = parts[0].trim().to_string();
        let right = parts[1].trim();
        let tokens: Vec<&str> = right.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        if let Ok(total) = tokens[0].parse::<i32>() {
            let markers = if tokens.len() > 1 { tokens[1] } else { "" };
            let plus = markers.chars().filter(|&c| c == '+').count() as i32;
            let minus = markers.chars().filter(|&c| c == '-').count() as i32;
            let (added, removed) = if plus + minus > 0 {
                let a = (total as f64 * plus as f64 / (plus + minus) as f64).round() as i32;
                (a, total - a)
            } else {
                (0, 0)
            };
            map.insert(path, (added, removed));
        }
    }
    map
}

fn fetch_git_state() -> GitState {
    let mut state = GitState::default();

    // Staged files
    let staged_stat = run_git(&["diff", "--cached", "--stat"]).unwrap_or_default();
    let staged_stat_map = parse_diff_stat(&staged_stat);

    if let Some(output) = run_git(&["diff", "--cached", "--name-status"]) {
        for line in output.lines() {
            if line.is_empty() {
                continue;
            }
            let status_char = line.chars().next().unwrap_or(' ');
            let path = line[1..].trim().to_string();
            if path.is_empty() {
                continue;
            }
            let (added, removed) = staged_stat_map.get(&path).copied().unwrap_or((0, 0));
            state.staged.push(ChangedFile {
                path,
                status: parse_status_char(status_char),
                staged: true,
                added,
                removed,
            });
        }
    }

    // Unstaged files
    let unstaged_stat = run_git(&["diff", "--stat"]).unwrap_or_default();
    let unstaged_stat_map = parse_diff_stat(&unstaged_stat);

    if let Some(output) = run_git(&["diff", "--name-status"]) {
        for line in output.lines() {
            if line.is_empty() {
                continue;
            }
            let status_char = line.chars().next().unwrap_or(' ');
            let path = line[1..].trim().to_string();
            if path.is_empty() {
                continue;
            }
            let (added, removed) = unstaged_stat_map.get(&path).copied().unwrap_or((0, 0));
            state.unstaged.push(ChangedFile {
                path,
                status: parse_status_char(status_char),
                staged: false,
                added,
                removed,
            });
        }
    }

    // Untracked files
    if let Some(output) = run_git(&["ls-files", "--others", "--exclude-standard"]) {
        for line in output.lines() {
            let path = line.trim().to_string();
            if path.is_empty() {
                continue;
            }
            state.untracked.push(ChangedFile {
                path,
                status: FileStatus::Untracked,
                staged: false,
                added: 0,
                removed: 0,
            });
        }
    }

    state
}

fn fetch_diff(file: &ChangedFile) -> Vec<String> {
    let args: Vec<&str> = if file.staged {
        vec!["diff", "--cached", "--", &file.path]
    } else {
        vec!["diff", "--", &file.path]
    };
    let output = run_git(&args).unwrap_or_default();
    output.lines().map(|l| l.to_string()).collect()
}

// ---------------------------------------------------------------------------
// UI helpers
// ---------------------------------------------------------------------------

fn status_style(status: &FileStatus) -> Style {
    match status {
        FileStatus::Modified => Style::default().fg(Color::Yellow),
        FileStatus::Added => Style::default().fg(Color::Green),
        FileStatus::Deleted => Style::default().fg(Color::Red),
        FileStatus::Renamed => Style::default().fg(Color::Cyan),
        FileStatus::Untracked => Style::default().fg(Color::Magenta),
    }
}

fn status_char(status: &FileStatus) -> &'static str {
    match status {
        FileStatus::Modified => "M",
        FileStatus::Added => "A",
        FileStatus::Deleted => "D",
        FileStatus::Renamed => "R",
        FileStatus::Untracked => "?",
    }
}

fn build_list_items(state: &GitState) -> Vec<ListItem<'static>> {
    let mut items: Vec<ListItem<'static>> = Vec::new();

    let make_header = |label: &str, color: Color| {
        ListItem::new(Line::from(Span::styled(
            format!("── {} ", label),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )))
    };

    if !state.staged.is_empty() {
        items.push(make_header("Staged", Color::Green));
        for f in &state.staged {
            items.push(build_file_item(f));
        }
    }

    if !state.unstaged.is_empty() {
        items.push(make_header("Unstaged", Color::Yellow));
        for f in &state.unstaged {
            items.push(build_file_item(f));
        }
    }

    if !state.untracked.is_empty() {
        items.push(make_header("Untracked", Color::Magenta));
        for f in &state.untracked {
            items.push(build_file_item(f));
        }
    }

    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  No changes detected",
            Style::default().fg(Color::DarkGray),
        ))));
    }

    items
}

fn build_file_item(f: &ChangedFile) -> ListItem<'static> {
    let sc = status_char(&f.status);
    let style = status_style(&f.status);

    let mut spans = vec![
        Span::styled(format!("  {} ", sc), style),
        Span::styled(f.path.clone(), Style::default()),
    ];

    if f.added > 0 || f.removed > 0 {
        spans.push(Span::styled(
            format!("  +{} -{}", f.added, f.removed),
            Style::default().fg(Color::DarkGray),
        ));
    }

    ListItem::new(Line::from(spans))
}

/// Returns None if the visual index points at a section header.
fn visual_to_file_index(state: &GitState, visual: usize) -> Option<usize> {
    let mut vi = 0usize;
    let mut fi = 0usize;

    if !state.staged.is_empty() {
        if vi == visual {
            return None;
        }
        vi += 1;
        for _ in &state.staged {
            if vi == visual {
                return Some(fi);
            }
            vi += 1;
            fi += 1;
        }
    }

    if !state.unstaged.is_empty() {
        if vi == visual {
            return None;
        }
        vi += 1;
        for _ in &state.unstaged {
            if vi == visual {
                return Some(fi);
            }
            vi += 1;
            fi += 1;
        }
    }

    if !state.untracked.is_empty() {
        if vi == visual {
            return None;
        }
        vi += 1;
        for _ in &state.untracked {
            if vi == visual {
                return Some(fi);
            }
            vi += 1;
            fi += 1;
        }
    }

    None
}

fn visual_len(state: &GitState) -> usize {
    let mut count = 0;
    if !state.staged.is_empty() {
        count += 1 + state.staged.len();
    }
    if !state.unstaged.is_empty() {
        count += 1 + state.unstaged.len();
    }
    if !state.untracked.is_empty() {
        count += 1 + state.untracked.len();
    }
    if count == 0 { 1 } else { count }
}

/// Convert file index to visual row index.
fn file_to_visual_index(state: &GitState, fi: usize) -> usize {
    let mut vi = 0usize;
    let mut cur_fi = 0usize;

    if !state.staged.is_empty() {
        vi += 1;
        for _ in &state.staged {
            if cur_fi == fi {
                return vi;
            }
            vi += 1;
            cur_fi += 1;
        }
    }

    if !state.unstaged.is_empty() {
        vi += 1;
        for _ in &state.unstaged {
            if cur_fi == fi {
                return vi;
            }
            vi += 1;
            cur_fi += 1;
        }
    }

    if !state.untracked.is_empty() {
        vi += 1;
        for _ in &state.untracked {
            if cur_fi == fi {
                return vi;
            }
            vi += 1;
            cur_fi += 1;
        }
    }

    vi
}

fn render_diff_line(line: &str) -> Line<'static> {
    let owned = line.to_string();
    if owned.starts_with('+') && !owned.starts_with("+++") {
        Line::from(Span::styled(owned, Style::default().fg(Color::Green)))
    } else if owned.starts_with('-') && !owned.starts_with("---") {
        Line::from(Span::styled(owned, Style::default().fg(Color::Red)))
    } else if owned.starts_with("@@") {
        Line::from(Span::styled(owned, Style::default().fg(Color::Cyan)))
    } else {
        Line::from(owned)
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn draw(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> io::Result<()> {
    terminal.draw(|frame| {
        let size = frame.area();

        match app.view {
            AppView::List => {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(1), Constraint::Length(1)])
                    .split(size);

                let items = build_list_items(&app.git_state);
                let list = List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(Line::from(vec![
                        Span::raw(" watchdiff  "),
                        Span::styled(app.branch.clone(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                        Span::raw(" "),
                    ])))
                    .highlight_style(
                        Style::default()
                            .bg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD),
                    );

                frame.render_stateful_widget(list, chunks[0], &mut app.list_state);

                let help = Paragraph::new(Line::from(vec![
                    Span::styled(" q", Style::default().fg(Color::Yellow)),
                    Span::raw(" quit  "),
                    Span::styled("↑↓/jk", Style::default().fg(Color::Yellow)),
                    Span::raw(" nav  "),
                    Span::styled("Enter", Style::default().fg(Color::Yellow)),
                    Span::raw(" diff  "),
                    Span::styled("e", Style::default().fg(Color::Yellow)),
                    Span::raw(" edit"),
                ]));
                frame.render_widget(help, chunks[1]);
            }

            AppView::Diff => {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(1), Constraint::Length(1)])
                    .split(size);

                let title = app
                    .list_state
                    .selected()
                    .and_then(|vi| visual_to_file_index(&app.git_state, vi))
                    .and_then(|fi| app.git_state.file_at_index(fi))
                    .map(|f| format!(" {} ", f.path))
                    .unwrap_or_else(|| " diff ".to_string());

                let diff_text: Vec<Line<'static>> = app
                    .diff_lines
                    .iter()
                    .skip(app.diff_scroll)
                    .map(|l| render_diff_line(l))
                    .collect();

                let para = Paragraph::new(diff_text)
                    .block(Block::default().borders(Borders::ALL).title(title));
                frame.render_widget(para, chunks[0]);

                // Scrollbar
                let content_len = app.diff_lines.len();
                let view_h = chunks[0]
                    .inner(Margin {
                        vertical: 1,
                        horizontal: 1,
                    })
                    .height as usize;
                app.diff_viewport_height = view_h.max(1);
                if content_len > view_h {
                    let mut sb_state =
                        ScrollbarState::new(content_len.saturating_sub(view_h))
                            .position(app.diff_scroll);
                    let sb = Scrollbar::new(ScrollbarOrientation::VerticalRight);
                    frame.render_stateful_widget(sb, chunks[0], &mut sb_state);
                }

                let help = Paragraph::new(Line::from(vec![
                    Span::styled(" Esc", Style::default().fg(Color::Yellow)),
                    Span::raw(" back  "),
                    Span::styled("jk", Style::default().fg(Color::Yellow)),
                    Span::raw(" line  "),
                    Span::styled("^d^u", Style::default().fg(Color::Yellow)),
                    Span::raw(" half  "),
                    Span::styled("^f^b", Style::default().fg(Color::Yellow)),
                    Span::raw(" page  "),
                    Span::styled("gG", Style::default().fg(Color::Yellow)),
                    Span::raw(" top/bot  "),
                    Span::styled("e", Style::default().fg(Color::Yellow)),
                    Span::raw(" edit"),
                ]));
                frame.render_widget(help, chunks[1]);
            }
        }
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

fn open_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    path: &str,
) -> io::Result<()> {
    let editor = std::env::var("EDITOR").ok();
    let fallbacks = ["nvim", "vim", "vi"];

    // Build candidate list: $EDITOR first, then fallbacks (deduped)
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(ref e) = editor {
        candidates.push(e.as_str());
    }
    for fb in &fallbacks {
        if !candidates.contains(fb) {
            candidates.push(fb);
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    let mut launched = false;
    for candidate in candidates {
        if let Ok(status) = Command::new(candidate).arg(path).status() {
            let _ = status; // exit code is the editor's business
            launched = true;
            break;
        }
    }

    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    terminal.clear()?;

    if !launched {
        // Nothing we can do — silently continue; user will see watchdiff again
    }

    Ok(())
}

fn main() -> io::Result<()> {
    let repo_root = run_git(&["rev-parse", "--show-toplevel"])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| {
            eprintln!("watchdiff: not a git repository");
            std::process::exit(1);
        });

    let git_dir = run_git(&["rev-parse", "--git-dir"])
        .map(|s| s.trim().to_string());

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let git_state = fetch_git_state();
    let branch = fetch_branch();
    let initial_selection = first_file_visual(&git_state);

    let mut app = App {
        git_state,
        branch,
        list_state: ListState::default().with_selected(initial_selection),
        view: AppView::List,
        diff_lines: Vec::new(),
        diff_scroll: 0,
        diff_viewport_height: 24,
    };

    // FS watcher sends on a channel; key events are polled directly in the main loop
    let (fs_tx, fs_rx) = mpsc::channel::<()>();
    let mut watcher: RecommendedWatcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                match ev.kind {
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                        let _ = fs_tx.send(());
                    }
                    _ => {}
                }
            }
        })
        .expect("failed to create watcher");

    watcher
        .watch(Path::new(&repo_root), RecursiveMode::Recursive)
        .unwrap_or_else(|e| eprintln!("watcher error: {e}"));

    // For worktrees the gitdir lives outside the repo root (e.g. .git/worktrees/<name>
    // inside the main repo), so watch it separately to catch HEAD changes there too.
    if let Some(ref gd) = git_dir {
        let gd_path = Path::new(gd);
        if !gd_path.starts_with(&repo_root) {
            watcher
                .watch(gd_path, RecursiveMode::Recursive)
                .unwrap_or_else(|e| eprintln!("watcher error (git-dir): {e}"));
        }
    }

    let mut pending_refresh = false;
    let mut last_refresh = std::time::Instant::now();

    draw(&mut terminal, &mut app)?;

    loop {
        // Drain all pending FS events
        if fs_rx.try_recv().is_ok() {
            while fs_rx.try_recv().is_ok() {}
            pending_refresh = true;
        }

        if pending_refresh && last_refresh.elapsed() > Duration::from_millis(300) {
            pending_refresh = false;
            last_refresh = std::time::Instant::now();
            refresh_state(&mut app);
            draw(&mut terminal, &mut app)?;
        }

        // Poll for a key event; short timeout so FS events stay responsive
        if event::poll(Duration::from_millis(50))?
            && let Ok(Event::Key(key)) = event::read()
        {
            match handle_key(&mut app, key) {
                KeyAction::Quit => break,
                KeyAction::OpenEditor(path) => {
                    open_editor(&mut terminal, &path)?;
                    draw(&mut terminal, &mut app)?;
                }
                KeyAction::None => {
                    draw(&mut terminal, &mut app)?;
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn first_file_visual(state: &GitState) -> Option<usize> {
    if state.staged.is_empty() && state.unstaged.is_empty() && state.untracked.is_empty() {
        return None;
    }
    // First file row is always visual index 1 (after the first section header)
    Some(1)
}

fn refresh_state(app: &mut App) {
    let current_path = app
        .list_state
        .selected()
        .and_then(|vi| visual_to_file_index(&app.git_state, vi))
        .and_then(|fi| app.git_state.file_at_index(fi))
        .map(|f| f.path.clone());

    app.git_state = fetch_git_state();
    app.branch = fetch_branch();

    // Keep cursor on same file if still present
    if let Some(path) = current_path
        && let Some(fi) = app.git_state.find_index(&path)
    {
        let vi = file_to_visual_index(&app.git_state, fi);
        app.list_state.select(Some(vi));
    }

    // Refresh diff content if in diff view
    if matches!(app.view, AppView::Diff)
        && let Some(vi) = app.list_state.selected()
        && let Some(fi) = visual_to_file_index(&app.git_state, vi)
        && let Some(file) = app.git_state.file_at_index(fi)
    {
        let scroll = app.diff_scroll;
        app.diff_lines = fetch_diff(file);
        app.diff_scroll = scroll.min(app.diff_lines.len().saturating_sub(1));
    }
}

enum KeyAction {
    Quit,
    OpenEditor(String),
    None,
}

fn handle_key(app: &mut App, key: KeyEvent) -> KeyAction {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return KeyAction::Quit;
    }

    match app.view {
        AppView::List => match key.code {
            KeyCode::Char('q') => return KeyAction::Quit,

            KeyCode::Up | KeyCode::Char('k') => move_cursor(app, -1),
            KeyCode::Down | KeyCode::Char('j') => move_cursor(app, 1),

            KeyCode::Enter => {
                if let Some(file) = selected_file(app) {
                    let file = file.clone();
                    app.diff_lines = fetch_diff(&file);
                    app.diff_scroll = 0;
                    app.view = AppView::Diff;
                }
            }

            KeyCode::Char('e') => {
                if let Some(file) = selected_file(app) {
                    return KeyAction::OpenEditor(file.path.clone());
                }
            }

            _ => {}
        },

        AppView::Diff => {
            let max_scroll = app.diff_lines.len().saturating_sub(1);
            let half = (app.diff_viewport_height / 2).max(1);
            let full = app.diff_viewport_height.max(1);

            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    app.view = AppView::List;
                }

                KeyCode::Up | KeyCode::Char('k') => {
                    app.diff_scroll = app.diff_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.diff_scroll = (app.diff_scroll + 1).min(max_scroll);
                }

                // Half page
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.diff_scroll = app.diff_scroll.saturating_sub(half);
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.diff_scroll = (app.diff_scroll + half).min(max_scroll);
                }

                // Full page
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.diff_scroll = app.diff_scroll.saturating_sub(full);
                }
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.diff_scroll = (app.diff_scroll + full).min(max_scroll);
                }
                KeyCode::PageUp => {
                    app.diff_scroll = app.diff_scroll.saturating_sub(full);
                }
                KeyCode::PageDown => {
                    app.diff_scroll = (app.diff_scroll + full).min(max_scroll);
                }

                // Top / bottom
                KeyCode::Char('g') => {
                    app.diff_scroll = 0;
                }
                KeyCode::Char('G') => {
                    app.diff_scroll = max_scroll;
                }

                KeyCode::Char('e') => {
                    if let Some(file) = selected_file(app) {
                        return KeyAction::OpenEditor(file.path.clone());
                    }
                }

                _ => {}
            }
        }
    }

    KeyAction::None
}

fn selected_file(app: &App) -> Option<&ChangedFile> {
    let vi = app.list_state.selected()?;
    let fi = visual_to_file_index(&app.git_state, vi)?;
    app.git_state.file_at_index(fi)
}

fn move_cursor(app: &mut App, delta: i32) {
    let vlen = visual_len(&app.git_state);
    if vlen == 0 {
        return;
    }
    let current = app.list_state.selected().unwrap_or(0) as i32;
    let mut next = (current + delta).clamp(0, (vlen as i32) - 1);

    // Skip header rows in the direction of movement
    let mut attempts = 0;
    while visual_to_file_index(&app.git_state, next as usize).is_none() && attempts < vlen {
        next = (next + delta.signum()).clamp(0, (vlen as i32) - 1);
        attempts += 1;
    }

    app.list_state.select(Some(next as usize));
}
