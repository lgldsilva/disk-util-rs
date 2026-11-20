//! Estado do navegador TUI: sort, filtro, busca incremental e âncora de seleção.
//!
//! Port de `get_sorted_items` + lógica de âncora (`selected_name`) do Python:
//! a seleção segue o NOME do item entre reordenações do scan progressivo.

use crate::scan::ScanItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Physical,
    Logical,
    Name,
    Type,
}

impl SortBy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "physical" => Some(Self::Physical),
            "logical" => Some(Self::Logical),
            "name" => Some(Self::Name),
            "type" => Some(Self::Type),
            _ => None,
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Physical => Self::Logical,
            Self::Logical => Self::Name,
            Self::Name => Self::Type,
            Self::Type => Self::Physical,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Physical => "physical",
            Self::Logical => "logical",
            Self::Name => "name",
            Self::Type => "type",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Browser {
    pub sort_by: SortBy,
    pub sort_desc: bool,
    pub filter: String,
    pub search_mode: bool,
    pub search_query: String,
    pub filter_mode: bool,
    pub filter_query: String,
    pub selected_idx: usize,
    pub selected_name: Option<String>,
    pub show_help: bool,
}

impl Browser {
    pub fn new(sort_by: SortBy, sort_desc: bool, filter: String) -> Self {
        Self {
            sort_by,
            sort_desc,
            filter,
            search_mode: false,
            search_query: String::new(),
            filter_mode: false,
            filter_query: String::new(),
            selected_idx: 0,
            selected_name: None,
            show_help: false,
        }
    }

    /// Itens filtrados + ordenados (port de `get_sorted_items`).
    pub fn visible<'a>(&self, items: &'a [ScanItem]) -> Vec<&'a ScanItem> {
        let mut out: Vec<&ScanItem> = items.iter().collect();
        if !self.filter.is_empty() {
            let f = self.filter.to_lowercase();
            out.retain(|i| i.name.to_lowercase().contains(&f));
        }
        match self.sort_by {
            SortBy::Physical => {
                out.sort_by(|a, b| cmp_by(a.size_physical, b.size_physical, self.sort_desc))
            }
            SortBy::Logical => {
                out.sort_by(|a, b| cmp_by(a.size_logical, b.size_logical, self.sort_desc))
            }
            SortBy::Name => out.sort_by(|a, b| {
                let (x, y) = (a.name.to_lowercase(), b.name.to_lowercase());
                if self.sort_desc {
                    y.cmp(&x)
                } else {
                    x.cmp(&y)
                }
            }),
            SortBy::Type => out.sort_by(|a, b| {
                // Port do Python: key=(not is_dir, name), reverse=sort_desc.
                let ka = (!a.is_dir, a.name.to_lowercase());
                let kb = (!b.is_dir, b.name.to_lowercase());
                if self.sort_desc {
                    kb.cmp(&ka)
                } else {
                    ka.cmp(&kb)
                }
            }),
        }
        out
    }

    /// Reancora a seleção pelo nome após reordenação; clamp no fim.
    pub fn anchor(&mut self, visible: &[&ScanItem]) {
        if visible.is_empty() {
            self.selected_idx = 0;
            self.selected_name = None;
            return;
        }
        if let Some(ref name) = self.selected_name.clone() {
            if let Some(pos) = visible.iter().position(|i| &i.name == name) {
                self.selected_idx = pos;
            }
        }
        self.selected_idx = self.selected_idx.min(visible.len() - 1);
        self.selected_name = Some(visible[self.selected_idx].name.clone());
    }

    pub fn move_up(&mut self, visible: &[&ScanItem]) {
        if visible.is_empty() {
            return;
        }
        self.selected_idx = self.selected_idx.saturating_sub(1).min(visible.len() - 1);
        self.selected_name = Some(visible[self.selected_idx].name.clone());
    }

    pub fn move_down(&mut self, visible: &[&ScanItem]) {
        if visible.is_empty() {
            return;
        }
        self.selected_idx = (self.selected_idx + 1).min(visible.len() - 1);
        self.selected_name = Some(visible[self.selected_idx].name.clone());
    }

    pub fn cycle_sort(&mut self) {
        self.sort_by = self.sort_by.cycle();
    }

    /// Busca incremental: pula p/ o 1º item que contém a query.
    pub fn search_jump(&mut self, visible: &[&ScanItem]) {
        if self.search_query.is_empty() || visible.is_empty() {
            return;
        }
        let q = self.search_query.to_lowercase();
        if let Some(pos) = visible
            .iter()
            .position(|i| i.name.to_lowercase().contains(&q))
        {
            self.selected_idx = pos;
            self.selected_name = Some(visible[pos].name.clone());
        }
    }
}

fn cmp_by(a: u64, b: u64, desc: bool) -> std::cmp::Ordering {
    if desc {
        b.cmp(&a)
    } else {
        a.cmp(&b)
    }
}

/// Ação decidida por tecla no modo normal (fora de busca/filtro).
/// Mantém o `match` de teclas testável sem TTY (cobertura headless).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    Quit,
    MoveUp,
    MoveDown,
    EnterDir,
    GoParent,
    StartSearch,
    StartFilter,
    CycleSort,
    ToggleOrder,
    Rescan,
    ToggleBoot,
    ToggleHelp,
    CloseHelp,
    Reload,
    /// Mostra/oculta o popup de detalhes das falhas de conexão SSH.
    SshErrors,
    Unhandled,
}

/// Resultado de uma tecla no modo busca incremental.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOutcome {
    /// Continua buscando (cursor já reposicionado).
    Continue,
    /// Enter/Esc: encerra a busca.
    Done,
    /// Seta: encerra a busca e reprocessa como navegação.
    Navigate,
}

/// Resultado de uma tecla no modo filtro inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOutcome {
    Continue,
    /// Enter: confirma.
    Done,
    /// Esc: cancela e restaura.
    Cancelled,
}

use crate::config::Keybindings;
use crossterm::event::KeyCode;

impl Browser {
    /// Classifica tecla no modo normal segundo `keybindings` do TOML.
    pub fn classify(&self, code: KeyCode, kb: &Keybindings) -> KeyAction {
        if let KeyCode::Char(c) = code {
            let s = c.to_string();
            if s == kb.quit {
                return KeyAction::Quit;
            }
            if s == kb.search {
                return KeyAction::StartSearch;
            }
            if s == kb.filter {
                return KeyAction::StartFilter;
            }
            if s == kb.sort_cycle {
                return KeyAction::CycleSort;
            }
            if s == kb.toggle_order {
                return KeyAction::ToggleOrder;
            }
            if s == kb.rescan {
                return KeyAction::Rescan;
            }
            if s == kb.show_boot {
                return KeyAction::ToggleBoot;
            }
            if s == kb.help {
                return KeyAction::ToggleHelp;
            }
            if s == kb.reload_config {
                return KeyAction::Reload;
            }
            if s == kb.ssh_errors {
                return KeyAction::SshErrors;
            }
            return KeyAction::Unhandled;
        }
        match code {
            KeyCode::Up => KeyAction::MoveUp,
            KeyCode::Down => KeyAction::MoveDown,
            KeyCode::Enter if kb.enter == "enter" => KeyAction::EnterDir,
            KeyCode::Backspace if kb.back == "backspace" => KeyAction::GoParent,
            KeyCode::Left => KeyAction::GoParent,
            KeyCode::Esc if self.show_help => KeyAction::CloseHelp,
            _ => KeyAction::Unhandled,
        }
    }

    /// Consome tecla em `search_mode` (type-to-jump do Python).
    pub fn search_key(&mut self, code: KeyCode, visible: &[&ScanItem]) -> SearchOutcome {
        match code {
            KeyCode::Enter | KeyCode::Esc => {
                self.search_mode = false;
                SearchOutcome::Done
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                self.search_mode = false;
                SearchOutcome::Navigate
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.search_jump(visible);
                SearchOutcome::Continue
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.search_query.push(c);
                self.search_jump(visible);
                SearchOutcome::Continue
            }
            _ => SearchOutcome::Continue,
        }
    }

    /// Consome tecla em `filter_mode` (filtro inline, confirma/cancela).
    pub fn filter_key(&mut self, code: KeyCode, saved: &str) -> FilterOutcome {
        match code {
            KeyCode::Enter => {
                self.filter_mode = false;
                self.filter_query.clear();
                FilterOutcome::Done
            }
            KeyCode::Esc => {
                self.filter_mode = false;
                self.filter = saved.to_string();
                self.filter_query.clear();
                FilterOutcome::Cancelled
            }
            KeyCode::Backspace => {
                self.filter_query.pop();
                self.filter.clone_from(&self.filter_query);
                FilterOutcome::Continue
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.filter_query.push(c);
                self.filter.clone_from(&self.filter_query);
                FilterOutcome::Continue
            }
            _ => FilterOutcome::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ItemStatus, ScanItem};
    use std::path::PathBuf;

    fn item(name: &str, is_dir: bool, phys: u64, log: u64) -> ScanItem {
        ScanItem {
            name: name.to_string(),
            is_dir,
            path: PathBuf::from(name),
            size_logical: log,
            size_physical: phys,
            status: ItemStatus::Ok,
        }
    }

    fn items() -> Vec<ScanItem> {
        vec![
            item("b.txt", false, 300, 300),
            item("adir", true, 500, 400),
            item("c.txt", false, 100, 900),
        ]
    }

    #[test]
    fn sort_physical_desc_default() {
        let b = Browser::new(SortBy::Physical, true, String::new());
        let items = items();
        let v = b.visible(&items);
        assert_eq!(v[0].name, "adir");
    }

    #[test]
    fn filter_narrows_list() {
        let b = Browser::new(SortBy::Physical, true, "txt".to_string());
        let items = items();
        assert_eq!(b.visible(&items).len(), 2);
    }

    #[test]
    fn anchor_follows_name_after_reorder() {
        let mut b = Browser::new(SortBy::Name, true, String::new());
        let items = items();
        let v = b.visible(&items);
        b.selected_name = Some("c.txt".to_string());
        b.anchor(&v);
        let v2 = b.visible(&items);
        assert_eq!(v2[b.selected_idx].name, "c.txt");
    }

    #[test]
    fn search_jumps_to_match() {
        let mut b = Browser::new(SortBy::Physical, true, String::new());
        b.search_query = "c.".to_string();
        let items = items();
        let v = b.visible(&items);
        b.search_jump(&v);
        assert_eq!(v[b.selected_idx].name, "c.txt");
    }

    #[test]
    fn sort_cycle_rotates() {
        let mut b = Browser::new(SortBy::Physical, true, String::new());
        b.cycle_sort();
        assert_eq!(b.sort_by, SortBy::Logical);
    }

    #[test]
    fn sort_parse_and_labels() {
        assert_eq!(SortBy::parse("physical"), Some(SortBy::Physical));
        assert_eq!(SortBy::parse("logical"), Some(SortBy::Logical));
        assert_eq!(SortBy::parse("name"), Some(SortBy::Name));
        assert_eq!(SortBy::parse("type"), Some(SortBy::Type));
        assert_eq!(SortBy::parse("x"), None);
        assert_eq!(SortBy::Physical.label(), "physical");
        assert_eq!(SortBy::Type.cycle(), SortBy::Physical);
    }

    #[test]
    fn anchor_empty_and_move_bounds() {
        let mut b = Browser::new(SortBy::Physical, true, String::new());
        b.anchor(&[]);
        assert_eq!(b.selected_idx, 0);
        assert!(b.selected_name.is_none());
        b.move_up(&[]);
        b.move_down(&[]);
        let items = items();
        let v = b.visible(&items);
        b.move_down(&v);
        assert_eq!(v[b.selected_idx].name, v[1].name);
        b.move_up(&v);
        assert_eq!(v[b.selected_idx].name, v[0].name);
    }

    #[test]
    fn type_and_name_sorts() {
        let items = items();
        let b = Browser::new(SortBy::Type, true, String::new());
        let names: Vec<_> = b.visible(&items).iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["c.txt", "b.txt", "adir"]);
        let b = Browser::new(SortBy::Type, false, String::new());
        let names: Vec<_> = b.visible(&items).iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["adir", "b.txt", "c.txt"]);
        let contrasting = vec![item("afile", false, 1, 1), item("zdir", true, 1, 1)];
        let b = Browser::new(SortBy::Type, false, String::new());
        let names: Vec<_> = b
            .visible(&contrasting)
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(names, vec!["zdir", "afile"]);
        let b = Browser::new(SortBy::Name, false, String::new());
        let names: Vec<_> = b.visible(&items).iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["adir", "b.txt", "c.txt"]);
        let b = Browser::new(SortBy::Logical, true, String::new());
        assert_eq!(b.visible(&items)[0].name, "c.txt");
    }

    #[test]
    fn search_empty_query_keeps_cursor() {
        let mut b = Browser::new(SortBy::Physical, true, String::new());
        let items = items();
        let v = b.visible(&items);
        b.search_query.clear();
        b.search_jump(&v);
        b.search_jump(&[]);
        assert_eq!(b.selected_idx, 0);
    }

    #[test]
    fn classify_matches_keybindings() {
        use crate::config::Keybindings;
        use crossterm::event::KeyCode;
        let kb = Keybindings::default();
        let b = Browser::new(SortBy::Physical, true, String::new());
        assert_eq!(b.classify(KeyCode::Char('q'), &kb), KeyAction::Quit);
        assert_eq!(b.classify(KeyCode::Char('/'), &kb), KeyAction::StartSearch);
        assert_eq!(b.classify(KeyCode::Char('f'), &kb), KeyAction::StartFilter);
        assert_eq!(b.classify(KeyCode::Char('o'), &kb), KeyAction::CycleSort);
        assert_eq!(b.classify(KeyCode::Char('s'), &kb), KeyAction::ToggleOrder);
        assert_eq!(b.classify(KeyCode::Char('r'), &kb), KeyAction::Rescan);
        assert_eq!(b.classify(KeyCode::Char('b'), &kb), KeyAction::ToggleBoot);
        assert_eq!(b.classify(KeyCode::Char('h'), &kb), KeyAction::ToggleHelp);
        assert_eq!(b.classify(KeyCode::Char('R'), &kb), KeyAction::Reload);
        assert_eq!(b.classify(KeyCode::Char('d'), &kb), KeyAction::SshErrors);
        assert_eq!(b.classify(KeyCode::Char('z'), &kb), KeyAction::Unhandled);
        assert_eq!(b.classify(KeyCode::Up, &kb), KeyAction::MoveUp);
        assert_eq!(b.classify(KeyCode::Down, &kb), KeyAction::MoveDown);
        assert_eq!(b.classify(KeyCode::Enter, &kb), KeyAction::EnterDir);
        assert_eq!(b.classify(KeyCode::Backspace, &kb), KeyAction::GoParent);
        assert_eq!(b.classify(KeyCode::Left, &kb), KeyAction::GoParent);
        assert_eq!(b.classify(KeyCode::Esc, &kb), KeyAction::Unhandled);
        assert_eq!(b.classify(KeyCode::Right, &kb), KeyAction::Unhandled);
        let mut b2 = b.clone();
        b2.show_help = true;
        assert_eq!(b2.classify(KeyCode::Esc, &kb), KeyAction::CloseHelp);
    }

    #[test]
    fn search_key_outcomes() {
        use crossterm::event::KeyCode;
        let mut b = Browser::new(SortBy::Physical, true, String::new());
        b.search_mode = true;
        let items = items();
        let v = b.visible(&items);
        assert_eq!(
            b.search_key(KeyCode::Char('c'), &v),
            SearchOutcome::Continue
        );
        assert_eq!(b.search_query, "c");
        assert_eq!(
            b.search_key(KeyCode::Backspace, &v),
            SearchOutcome::Continue
        );
        assert_eq!(b.search_query, "");
        assert_eq!(b.search_key(KeyCode::Right, &v), SearchOutcome::Navigate);
        assert!(!b.search_mode);
        b.search_mode = true;
        assert_eq!(b.search_key(KeyCode::Enter, &v), SearchOutcome::Done);
        assert!(!b.search_mode);
        b.search_mode = true;
        assert_eq!(b.search_key(KeyCode::Esc, &v), SearchOutcome::Done);
        assert_eq!(b.search_key(KeyCode::F(1), &v), SearchOutcome::Continue);
    }

    #[test]
    fn filter_key_outcomes() {
        use crossterm::event::KeyCode;
        let mut b = Browser::new(SortBy::Physical, true, "old".to_string());
        b.filter_mode = true;
        b.filter_query = "old".to_string();
        assert_eq!(
            b.filter_key(KeyCode::Char('x'), "old"),
            FilterOutcome::Continue
        );
        assert_eq!(b.filter, "oldx");
        assert_eq!(
            b.filter_key(KeyCode::Backspace, "old"),
            FilterOutcome::Continue
        );
        assert_eq!(b.filter, "old");
        assert_eq!(b.filter_key(KeyCode::F(1), "old"), FilterOutcome::Continue);
        assert_eq!(b.filter_key(KeyCode::Enter, "old"), FilterOutcome::Done);
        assert!(!b.filter_mode);
        b.filter_mode = true;
        b.filter = "changed".to_string();
        b.filter_query = "changed".to_string();
        assert_eq!(b.filter_key(KeyCode::Esc, "old"), FilterOutcome::Cancelled);
        assert_eq!(b.filter, "old");
        assert!(!b.filter_mode);
    }

    #[test]
    fn browser_boundary_cases_cover_sort_anchor_navigation_and_key_guards() {
        use crate::config::Keybindings;
        use crossterm::event::KeyCode;

        let items = items();
        for sort in [
            SortBy::Physical,
            SortBy::Logical,
            SortBy::Name,
            SortBy::Type,
        ] {
            for descending in [false, true] {
                let browser = Browser::new(sort, descending, String::new());
                assert_eq!(browser.visible(&items).len(), items.len());
            }
        }

        let mut browser = Browser::new(SortBy::Name, false, String::new());
        let visible = browser.visible(&items);
        browser.selected_idx = usize::MAX;
        browser.selected_name = Some("missing".into());
        browser.anchor(&visible);
        assert_eq!(browser.selected_idx, visible.len() - 1);
        browser.selected_name = None;
        browser.selected_idx = 1;
        browser.anchor(&visible);
        assert_eq!(
            browser.selected_name.as_deref(),
            Some(visible[1].name.as_str())
        );
        browser.move_up(&visible);
        browser.move_down(&visible);
        browser.selected_idx = visible.len() - 1;
        browser.move_down(&visible);
        assert_eq!(browser.selected_idx, visible.len() - 1);
        browser.selected_idx = visible.len() + 2;
        browser.move_up(&visible);
        assert_eq!(browser.selected_idx, visible.len() - 1);

        browser.selected_idx = 2;
        browser.search_query.clear();
        browser.search_jump(&visible);
        assert_eq!(browser.selected_idx, 2);
        browser.search_query = "not-found".into();
        let selected = browser.selected_idx;
        browser.search_jump(&visible);
        assert_eq!(browser.selected_idx, selected);
        browser.search_query = "\n".into();
        assert_eq!(
            browser.search_key(KeyCode::Char('\n'), &visible),
            SearchOutcome::Continue
        );
        assert_eq!(browser.search_query, "\n");
        assert_eq!(
            browser.filter_key(KeyCode::Char('\n'), "saved"),
            FilterOutcome::Continue
        );
        assert!(browser.filter_query.is_empty());

        let keybindings = Keybindings {
            enter: "return".into(),
            back: "delete".into(),
            ..Keybindings::default()
        };
        assert_eq!(
            browser.classify(KeyCode::Enter, &keybindings),
            KeyAction::Unhandled
        );
        assert_eq!(
            browser.classify(KeyCode::Backspace, &keybindings),
            KeyAction::Unhandled
        );
    }
}
