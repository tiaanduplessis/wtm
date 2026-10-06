//! Input and hit regions come from the same layout that draws the dashboard.

use super::*;

#[derive(Default)]
pub(super) struct HitMap {
    pub table: Rect,
    pub mark: Rect,
    pub headers: Vec<(Rect, Sort)>,
    pub search: Option<EditorHit>,
    pub details: Rect,
    pub buttons: Vec<(Rect, KeyCode)>,
    modal: Option<Rect>,
    fields: Vec<(usize, EditorHit)>,
    confirm: Option<EditorHit>,
    menu: Vec<(Rect, usize)>,
}

pub(super) struct EditorHit {
    area: Rect,
    start: usize,
}

fn contains(area: Rect, mouse: MouseEvent) -> bool {
    area.contains((mouse.column, mouse.row).into())
}

impl App {
    pub(super) fn mouse(&mut self, mouse: MouseEvent) -> Intent {
        if !self.viewport_ready {
            return Intent::Continue;
        }
        let wheel = match mouse.kind {
            MouseEventKind::ScrollUp => Some(KeyCode::Up),
            MouseEventKind::ScrollDown => Some(KeyCode::Down),
            _ => None,
        };
        let click = mouse.kind == MouseEventKind::Down(MouseButton::Left);
        if self.modal.is_some() {
            // A modal owns all mouse input, including clicks outside its border.
            if let Some(key) = wheel {
                if self.hits.modal.is_some_and(|area| contains(area, mouse)) {
                    for _ in 0..3 {
                        self.modal_key(KeyEvent::new(key, KeyModifiers::NONE));
                    }
                }
            } else if click {
                if let Some((_, key)) = self
                    .hits
                    .buttons
                    .iter()
                    .find(|(area, _)| contains(*area, mouse))
                {
                    return self.modal_key(KeyEvent::new(*key, KeyModifiers::NONE));
                } else if let Some((_, index)) = self
                    .hits
                    .menu
                    .iter()
                    .find(|(area, _)| contains(*area, mouse))
                {
                    let key = menu_actions(self.select_only)[*index].1;
                    self.modal = None;
                    return self.key(KeyEvent::new(key, KeyModifiers::NONE));
                } else if let Some(Modal::Form(form)) = &mut self.modal {
                    if let Some((index, hit)) = self
                        .hits
                        .fields
                        .iter()
                        .find(|(_, hit)| contains(hit.area, mouse))
                    {
                        form.focus = *index;
                        let field = &mut form.fields[*index];
                        field.cursor = clicked_cursor(&field.value, hit, mouse.column);
                    }
                } else if let Some(Modal::Confirm { input, cursor, .. }) = &mut self.modal
                    && let Some(hit) = &self.hits.confirm
                    && contains(hit.area, mouse)
                {
                    *cursor = clicked_cursor(input, hit, mouse.column);
                }
            }
            return Intent::Continue;
        }
        if let Some(key) = wheel {
            if contains(self.hits.table, mouse) {
                self.navigate(if key == KeyCode::Up { -3 } else { 3 });
            } else if contains(self.hits.details, mouse) {
                self.key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
                self.modal_key(KeyEvent::new(key, KeyModifiers::NONE));
            }
            return Intent::Continue;
        }
        if !click && mouse.kind != MouseEventKind::Down(MouseButton::Right) {
            return Intent::Continue;
        }
        if contains(self.hits.table, mouse) {
            let row = self.table.offset() + usize::from(mouse.row - self.hits.table.y);
            if row < self.visible.len() {
                self.table.select(Some(row));
                self.searching = false;
                if mouse.kind == MouseEventKind::Down(MouseButton::Right) {
                    return self.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
                }
                if contains(self.hits.mark, mouse) {
                    return self.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
                }
            }
        } else if click {
            if let Some(hit) = &self.hits.search
                && (contains(hit.area, mouse)
                    || mouse.column + 1 == hit.area.x && mouse.row == hit.area.y)
            {
                self.searching = true;
                self.search_cursor = clicked_cursor(&self.search, hit, mouse.column);
            } else if let Some((_, sort)) = self
                .hits
                .headers
                .iter()
                .find(|(area, _)| contains(*area, mouse))
            {
                let selected = self.selected().map(|tree| tree.path.clone());
                self.sort_reversed = if self.sort == *sort {
                    !self.sort_reversed
                } else {
                    false
                };
                self.sort = *sort;
                self.rebuild(selected);
            } else if let Some((_, key)) = self
                .hits
                .buttons
                .iter()
                .find(|(area, _)| contains(*area, mouse))
            {
                let key = *key;
                self.searching = false;
                return self.key(KeyEvent::new(key, KeyModifiers::NONE));
            } else if contains(self.hits.details, mouse) {
                self.searching = false;
                return self.key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
            }
        }
        Intent::Continue
    }

    pub(super) fn paste(&mut self, text: &str) {
        // Bracketed paste edits one field; embedded newlines cannot submit a form.
        let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
        match &mut self.modal {
            Some(Modal::Form(form)) => {
                let field = &mut form.fields[form.focus];
                insert_text(&mut field.value, &mut field.cursor, &text);
            }
            Some(Modal::Confirm { input, cursor, .. }) => insert_text(input, cursor, &text),
            None if self.searching => {
                insert_text(&mut self.search, &mut self.search_cursor, &text);
                self.rebuild(None);
            }
            _ => {}
        }
    }
}

fn bounded_cursor(value: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(value.len());
    while !value.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

fn previous(value: &str, cursor: usize) -> usize {
    value[..cursor]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .chars()
        .next()
        .map_or(cursor, |ch| cursor + ch.len_utf8())
}

fn insert_text(value: &mut String, cursor: &mut usize, text: &str) {
    *cursor = bounded_cursor(value, *cursor);
    value.insert_str(*cursor, text);
    *cursor += text.len();
}

pub(super) fn edit_text(value: &mut String, cursor: &mut usize, key: KeyEvent) {
    *cursor = bounded_cursor(value, *cursor);
    match key.code {
        KeyCode::Left => *cursor = previous(value, *cursor),
        KeyCode::Right => *cursor = next(value, *cursor),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = value.len(),
        KeyCode::Backspace if *cursor > 0 => {
            let start = previous(value, *cursor);
            value.replace_range(start..*cursor, "");
            *cursor = start;
        }
        KeyCode::Delete => {
            value.replace_range(*cursor..next(value, *cursor), "");
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            value.clear();
            *cursor = 0;
        }
        KeyCode::Char(ch)
            if !ch.is_control()
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            insert_text(value, cursor, &ch.to_string());
        }
        _ => {}
    }
}

fn clipped(value: &str, width: usize) -> String {
    let mut used = 0;
    value
        .chars()
        .take_while(|ch| {
            used += ch.width().unwrap_or(0);
            used <= width
        })
        .collect()
}

pub(super) fn render_input(
    frame: &mut Frame<'_>,
    area: Rect,
    value: &str,
    cursor: usize,
    focused: bool,
) -> EditorHit {
    let cursor = bounded_cursor(value, cursor);
    let mut start = 0;
    while start < cursor && value[start..cursor].width() >= usize::from(area.width.max(1)) {
        start = next(value, start);
    }
    let shown = clipped(&safe(&value[start..]), usize::from(area.width));
    frame.render_widget(
        Paragraph::new(shown).style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::Reset
        })),
        area,
    );
    if focused && area.width > 0 && area.height > 0 {
        frame.set_cursor_position((area.x + value[start..cursor].width() as u16, area.y));
    }
    EditorHit { area, start }
}

fn clicked_cursor(value: &str, hit: &EditorHit, column: u16) -> usize {
    let mut used = 0;
    let target = usize::from(column.saturating_sub(hit.area.x));
    for (index, ch) in value[hit.start..].char_indices() {
        let width = ch.width().unwrap_or(0);
        if used + width > target {
            return hit.start + index;
        }
        used += width;
    }
    value.len()
}

pub(super) fn toolbar_actions(width: u16, select_only: bool) -> Vec<(&'static str, KeyCode)> {
    let mut buttons = vec![
        ("Choose", KeyCode::Enter),
        ("Search", KeyCode::Char('/')),
        ("Details", KeyCode::Char('i')),
        ("Refresh", KeyCode::Char('r')),
        ("Actions", KeyCode::Char('v')),
        ("Quit", KeyCode::Char('q')),
    ];
    if width >= 100 && !select_only {
        buttons.splice(
            4..4,
            [
                ("Mark", KeyCode::Char(' ')),
                ("Remove", KeyCode::Char('x')),
                ("Add", KeyCode::Char('a')),
            ],
        );
    }
    buttons
}

pub(super) fn menu_actions(select_only: bool) -> Vec<(&'static str, KeyCode)> {
    let mut actions = vec![
        ("Choose directory", KeyCode::Enter),
        ("Search", KeyCode::Char('/')),
        ("Full details", KeyCode::Char('i')),
        ("Mark / unmark", KeyCode::Char(' ')),
        ("Clear marks and filters", KeyCode::Char('c')),
        ("Changes only", KeyCode::Char('d')),
        ("Cycle sort", KeyCode::Char('s')),
        ("Reverse sort", KeyCode::Char('S')),
        ("Refresh known repositories", KeyCode::Char('r')),
        ("Full recursive discovery", KeyCode::Char('R')),
        ("Performance", KeyCode::Char('P')),
        ("Scan warnings", KeyCode::Char('w')),
        ("Last operation", KeyCode::Char('e')),
        ("Open commit in browser", KeyCode::Char('o')),
        ("Help", KeyCode::Char('?')),
    ];
    if !select_only {
        actions.extend([
            ("Add worktree", KeyCode::Char('a')),
            ("Move worktree", KeyCode::Char('m')),
            ("Lock / unlock", KeyCode::Char('l')),
            ("Review removal", KeyCode::Char('x')),
            ("Preview prune", KeyCode::Char('p')),
            ("Fetch", KeyCode::Char('f')),
        ]);
    }
    actions
}

pub(super) fn button_rows(buttons: &[(&str, KeyCode)], width: u16) -> u16 {
    let mut rows = 1;
    let mut used = 0;
    for (label, _) in buttons {
        let size = (label.width() as u16 + 3).min(width);
        if used + size > width {
            rows += 1;
            used = 0;
        }
        used += size;
    }
    rows
}

pub(super) fn draw_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    buttons: &[(&str, KeyCode)],
    hits: &mut Vec<(Rect, KeyCode)>,
) {
    let mut x = area.x;
    let mut y = area.y;
    for (label, key) in buttons {
        let width = (label.width() as u16 + 2).min(area.width);
        if x + width > area.right() {
            x = area.x;
            y += 1;
        }
        if y >= area.bottom() {
            break;
        }
        let button = Rect::new(x, y, width, 1);
        frame.render_widget(
            Paragraph::new(format!("[{label}]")).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            button,
        );
        hits.push((button, *key));
        x += width + 1;
    }
}

pub(super) fn render_modal(
    frame: &mut Frame<'_>,
    modal: &mut Modal,
    select_only: bool,
    hits: &mut HitMap,
) {
    let screen = frame.area();
    let width = screen.width.saturating_sub(4).min(100);
    let height = screen.height.saturating_sub(2).min(25);
    let area = Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    );
    hits.modal = Some(area);
    frame.render_widget(Clear, area);
    let title = match modal {
        Modal::Actions { .. } => " Actions ",
        Modal::Help { .. } => " Keyboard and mouse help ",
        Modal::Viewer { title, .. } | Modal::Confirm { title, .. } => title,
        Modal::Form(form) => match form.kind {
            FormKind::Add(_) => " Add worktree ",
            FormKind::Lock(_) => " Lock worktree ",
            FormKind::Move(_) => " Move worktree ",
        },
    };
    let block = Block::default().title(safe(title)).borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    match modal {
        Modal::Actions { selected, offset } => {
            let [content, footer] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            let actions = menu_actions(select_only);
            *offset = (*offset).min(*selected);
            if *selected >= *offset + usize::from(content.height) {
                *offset = (*selected + 1).saturating_sub(usize::from(content.height));
            }
            for (row, (index, (label, _))) in actions
                .iter()
                .enumerate()
                .skip(*offset)
                .take(usize::from(content.height))
                .enumerate()
            {
                let line = Rect::new(content.x, content.y + row as u16, content.width, 1);
                frame.render_widget(
                    Paragraph::new(*label).style(if index == *selected {
                        Style::default()
                            .bg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    }),
                    line,
                );
                hits.menu.push((line, index));
            }
            draw_buttons(frame, footer, &[("Close", KeyCode::Esc)], &mut hits.buttons);
        }
        Modal::Form(form) => {
            let [content, footer] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(inner);
            let compact = content.height < (form.fields.len() * 3) as u16;
            for (row, (index, field)) in form
                .fields
                .iter()
                .enumerate()
                .filter(|(index, _)| !compact || *index == form.focus)
                .enumerate()
            {
                let label_area =
                    Rect::new(content.x, content.y + (row * 3) as u16, content.width, 1);
                let label = format!(
                    "{} ({}/{})",
                    field.label.split(" (").next().unwrap_or(field.label),
                    index + 1,
                    form.fields.len()
                );
                frame.render_widget(
                    Paragraph::new(clipped(&label, usize::from(content.width))),
                    label_area,
                );
                let input_area = Rect::new(
                    content.x + 2,
                    label_area.y + 1,
                    content.width.saturating_sub(2),
                    1,
                );
                frame.render_widget(
                    Paragraph::new(if index == form.focus { ">" } else { " " }),
                    Rect::new(content.x, input_area.y, 1, 1),
                );
                hits.fields.push((
                    index,
                    render_input(
                        frame,
                        input_area,
                        &field.value,
                        field.cursor,
                        index == form.focus,
                    ),
                ));
            }
            frame.render_widget(
                Paragraph::new(safe(&form.error)).style(Style::default().fg(Color::Red)),
                Rect::new(footer.x, footer.y, footer.width, 1),
            );
            frame.render_widget(
                Paragraph::new("Click field | Left/Right edit | Ctrl-U clear"),
                Rect::new(footer.x, footer.y + 1, footer.width, 1),
            );
            draw_buttons(
                frame,
                Rect::new(footer.x, footer.y + 2, footer.width, 1),
                &[
                    ("Prev", KeyCode::BackTab),
                    (
                        if form.focus + 1 < form.fields.len() {
                            "Next"
                        } else {
                            "Submit"
                        },
                        KeyCode::Enter,
                    ),
                    ("Cancel", KeyCode::Esc),
                ],
                &mut hits.buttons,
            );
        }
        Modal::Confirm {
            explanation,
            required,
            input,
            cursor,
            scroll,
            ..
        } => {
            let [content, prompt] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(4)]).areas(inner);
            frame.render_widget(
                Paragraph::new(safe_multiline(explanation))
                    .wrap(Wrap { trim: false })
                    .scroll((*scroll, 0)),
                content,
            );
            frame.render_widget(
                Paragraph::new(format!("Type {required} then Enter or click Confirm.")),
                Rect::new(prompt.x, prompt.y, prompt.width, 1),
            );
            frame.render_widget(Paragraph::new(">"), Rect::new(prompt.x, prompt.y + 1, 1, 1));
            hits.confirm = Some(render_input(
                frame,
                Rect::new(
                    prompt.x + 2,
                    prompt.y + 1,
                    prompt.width.saturating_sub(2),
                    1,
                ),
                input,
                *cursor,
                true,
            ));
            draw_buttons(
                frame,
                Rect::new(prompt.x, prompt.y + 3, prompt.width, 1),
                &[("Confirm", KeyCode::Enter), ("Cancel", KeyCode::Esc)],
                &mut hits.buttons,
            );
        }
        Modal::Viewer { body, scroll, .. } => {
            let [content, footer] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            frame.render_widget(
                Paragraph::new(safe_multiline(body))
                    .wrap(Wrap { trim: false })
                    .scroll((*scroll, 0)),
                content,
            );
            draw_buttons(frame, footer, &[("Close", KeyCode::Esc)], &mut hits.buttons);
        }
        Modal::Help { scroll } => {
            let [content, footer] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            let body = "Click row: select | checkbox: mark | right-click: actions\nWheel: navigate rows or scroll an open preview\nClick Repository, Updated, Changes: sort; click again to reverse\nClick search / form field: place text cursor\nLeft/Right, Home/End, Backspace/Delete: edit text\nBracketed paste cannot submit or confirm an action\nUse --no-mouse for native terminal text selection (often Shift-drag also works)\nUp/Down or j/k: navigate | PageUp/PageDown: jump | Home/End\nEnter: choose directory and exit | q/Esc/Ctrl-C: cancel\n/: search repository, branch, path | s: cycle sort | S: reverse sort\nActivity sort is newest first; S puts oldest worktrees first. d: changes only\nSpace: mark worktree | c: clear marks and filters\nr: refresh known repositories | R: discover all nested repositories\no: open selected commit in your browser\ni: inspect full metadata | w: scan warnings | e: last operation | P: performance\nv: actions menu | a: add worktree | m: move clean linked worktree\nx: review removal of selected or all marked worktrees\nl: lock/unlock | p: preview then confirm prune | f: fetch\nPrimary, bare, dirty, locked and unknown-status worktrees are protected.\nRemoval deletes directories including ignored files and keeps branches.\nRemote fetch is explicit. Scans never fetch.\nDates distinguish commit time from local file/Git activity.\nTab/Shift-Tab: field | Ctrl-U: clear | Enter: next/submit | Esc: cancel\nRead-only selection mode disables management commands.";
            frame.render_widget(
                Paragraph::new(body)
                    .wrap(Wrap { trim: false })
                    .scroll((*scroll, 0)),
                content,
            );
            draw_buttons(frame, footer, &[("Close", KeyCode::Esc)], &mut hits.buttons);
        }
    }
}

fn safe_multiline(value: &str) -> Vec<Line<'static>> {
    value.lines().map(|line| Line::from(safe(line))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::tests::{app, press, rendered, tree};
    use tempfile::TempDir;

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(app: &mut App, area: Rect) -> Intent {
        app.mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            area.x,
            area.y,
        ))
    }

    fn button(app: &App, key: KeyCode) -> Rect {
        app.hits
            .buttons
            .iter()
            .find(|(_, code)| *code == key)
            .unwrap()
            .0
    }

    #[test]
    fn row_click_uses_scrolled_and_filtered_inventory_and_checkbox_marks_only_that_row() {
        let root = TempDir::new().unwrap();
        let trees = (0..40)
            .map(|i| tree(root.path(), &format!("topic-{i:02}")))
            .collect();
        let mut app = app(root.path(), trees);
        app.table.select(Some(25));
        rendered(&mut app, 120, 24);
        let offset = app.table.offset();
        assert!(offset > 0);
        let row = Rect::new(app.hits.table.x + 8, app.hits.table.y + 1, 1, 1);
        click(&mut app, row);
        assert_eq!(app.table.selected(), Some(offset + 1));
        let path = app.selected().unwrap().path.clone();
        let mark = Rect::new(app.hits.mark.x, row.y, 1, 1);
        click(&mut app, mark);
        assert_eq!(app.marked, BTreeSet::from([path]));
        app.search = "topic-39".into();
        app.rebuild(None);
        rendered(&mut app, 120, 24);
        let row = app.hits.table;
        click(&mut app, Rect::new(row.x + 8, row.y, 1, 1));
        assert_eq!(app.selected().unwrap().branch.as_deref(), Some("topic-39"));
        click(&mut app, Rect::new(row.x + 8, row.y + 2, 1, 1));
        assert_eq!(app.table.selected(), Some(0));
    }

    #[test]
    fn wheel_and_header_sort_preserve_correct_selection_and_ignore_unrelated_regions() {
        let root = TempDir::new().unwrap();
        let mut trees: Vec<_> = (0..12)
            .map(|i| tree(root.path(), &format!("topic-{i:02}")))
            .collect();
        trees[9].updated_at = Some(Utc::now());
        let mut app = app(root.path(), trees);
        rendered(&mut app, 140, 32);
        app.mouse(mouse(MouseEventKind::ScrollDown, 0, 0));
        assert_eq!(app.table.selected(), Some(0));
        app.mouse(mouse(
            MouseEventKind::ScrollDown,
            app.hits.table.x,
            app.hits.table.y,
        ));
        assert_eq!(app.table.selected(), Some(3));
        let path = app.selected().unwrap().path.clone();
        let header = app
            .hits
            .headers
            .iter()
            .find(|(_, sort)| *sort == Sort::Activity)
            .unwrap()
            .0;
        click(&mut app, header);
        assert_eq!(app.sort, Sort::Activity);
        assert!(!app.sort_reversed);
        assert_eq!(app.selected().unwrap().path, path);
        click(&mut app, header);
        assert!(app.sort_reversed);
    }

    #[test]
    fn modal_owns_mouse_input_and_wheel_without_changing_background_selection() {
        let root = TempDir::new().unwrap();
        let mut app = app(
            root.path(),
            vec![tree(root.path(), "a"), tree(root.path(), "b")],
        );
        press(&mut app, KeyCode::Char('i'));
        rendered(&mut app, 120, 32);
        let table = app.hits.table;
        app.mouse(mouse(MouseEventKind::ScrollDown, table.x, table.y));
        click(&mut app, Rect::new(table.x, table.y + 1, 1, 1));
        assert_eq!(app.table.selected(), Some(0));
        assert!(matches!(app.modal, Some(Modal::Viewer { scroll: 0, .. })));
        let area = app.hits.modal.unwrap();
        app.mouse(mouse(MouseEventKind::ScrollDown, area.x + 1, area.y + 1));
        assert!(matches!(app.modal, Some(Modal::Viewer { scroll: 3, .. })));
        let close = button(&app, KeyCode::Esc);
        click(&mut app, close);
        assert!(app.modal.is_none());
    }

    #[test]
    fn mouse_removal_keeps_typed_confirmation_and_paste_never_submits() {
        let root = TempDir::new().unwrap();
        let worktree = tree(root.path(), "topic");
        let path = worktree.path.clone();
        let mut app = app(root.path(), vec![worktree]);
        rendered(&mut app, 120, 32);
        let remove = button(&app, KeyCode::Char('x'));
        click(&mut app, remove);
        rendered(&mut app, 120, 32);
        let confirm = button(&app, KeyCode::Enter);
        click(&mut app, confirm);
        assert!(matches!(app.modal, Some(Modal::Confirm { .. })));
        assert!(!app.mutation_running);
        app.paste("REMOVE\r\n");
        assert!(matches!(&app.modal, Some(Modal::Confirm { input, .. }) if input == "REMOVE"));
        assert!(!app.mutation_running);
        let cancel = button(&app, KeyCode::Esc);
        click(&mut app, cancel);
        assert!(app.modal.is_none());
        assert!(path.exists());
    }

    #[test]
    fn dirty_pending_readonly_and_busy_entries_keep_mouse_protections() {
        let root = TempDir::new().unwrap();
        let mut worktree = tree(root.path(), "topic");
        worktree.status.untracked = 1;
        let mut app = app(root.path(), vec![worktree]);
        rendered(&mut app, 120, 32);
        let remove = button(&app, KeyCode::Char('x'));
        click(&mut app, remove);
        assert!(app.message.starts_with("Removal blocked"));
        app.worktrees[0].status.untracked = 0;
        app.pending_repos
            .insert(app.worktrees[0].repo.common_dir.clone());
        click(&mut app, remove);
        assert!(app.message.contains("awaiting refresh"));
        app.mutation_running = true;
        let quit = button(&app, KeyCode::Char('q'));
        assert!(matches!(click(&mut app, quit), Intent::Continue));
        app.select_only = true;
        rendered(&mut app, 120, 32);
        assert!(
            app.hits
                .buttons
                .iter()
                .all(|(_, key)| *key != KeyCode::Char('x'))
        );
        assert!(
            menu_actions(true)
                .iter()
                .all(|(_, key)| !matches!(key, KeyCode::Char('a' | 'x' | 'm' | 'f' | 'p' | 'l')))
        );
    }

    #[test]
    fn context_menu_is_clickable_scrollable_and_keyboard_choose_returns_intent() {
        let root = TempDir::new().unwrap();
        let mut app = app(root.path(), vec![tree(root.path(), "topic")]);
        rendered(&mut app, 48, 16);
        app.mouse(mouse(
            MouseEventKind::Down(MouseButton::Right),
            app.hits.table.x + 8,
            app.hits.table.y,
        ));
        assert!(matches!(app.modal, Some(Modal::Actions { .. })));
        rendered(&mut app, 48, 16);
        let area = app.hits.modal.unwrap();
        app.mouse(mouse(MouseEventKind::ScrollDown, area.x + 1, area.y + 1));
        rendered(&mut app, 48, 16);
        let mark = app
            .hits
            .menu
            .iter()
            .find(|(_, index)| *index == 3)
            .unwrap()
            .0;
        click(&mut app, mark);
        assert_eq!(app.marked.len(), 1);
        press(&mut app, KeyCode::Char('v'));
        assert!(matches!(press(&mut app, KeyCode::Enter), Intent::Select(_)));
    }

    #[test]
    fn form_field_click_places_cursor_and_unicode_edits_remain_valid() {
        let root = TempDir::new().unwrap();
        let mut app = app(root.path(), vec![tree(root.path(), "topic")]);
        press(&mut app, KeyCode::Char('a'));
        rendered(&mut app, 120, 32);
        let branch_area = app
            .hits
            .fields
            .iter()
            .find(|(index, _)| *index == 1)
            .unwrap()
            .1
            .area;
        click(&mut app, branch_area);
        app.paste("ab界cd");
        rendered(&mut app, 120, 32);
        click(&mut app, Rect::new(branch_area.x + 4, branch_area.y, 1, 1));
        press(&mut app, KeyCode::Char('X'));
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Backspace);
        assert!(
            matches!(&app.modal, Some(Modal::Form(form)) if form.focus == 1 && form.fields[1].value == "abXcd")
        );
        rendered(&mut app, 30, 14);
        assert_eq!(app.hits.fields.len(), 1);
        let cancel = button(&app, KeyCode::Esc);
        click(&mut app, cancel);
        assert!(app.modal.is_none());
    }

    #[test]
    fn search_cursor_editing_and_long_unicode_inputs_stay_in_bounds() {
        let root = TempDir::new().unwrap();
        let mut app = app(
            root.path(),
            vec![tree(root.path(), "abXcd"), tree(root.path(), "other")],
        );
        rendered(&mut app, 120, 32);
        let area = app.hits.search.as_ref().unwrap().area;
        click(&mut app, area);
        app.paste("abcd");
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Char('X'));
        assert_eq!(app.search, "abXcd");
        assert_eq!(app.visible.len(), 1);
        app.paste(&"界".repeat(200));
        rendered(&mut app, 30, 14);
        let hit = app.hits.search.as_ref().unwrap();
        assert!(hit.start > 0);
        assert!(app.search[hit.start..app.search_cursor].width() < usize::from(hit.area.width));
        rendered(&mut app, 20, 8);
        assert!(app.hits.buttons.is_empty());
        let selected = app.table.selected();
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 2));
        assert_eq!(app.table.selected(), selected);
    }
}
