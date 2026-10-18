// In `cargo test` the plugin entry points are gated out (see below), so the
// methods they call look unused — silence that only for test builds.
#![cfg_attr(test, allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet};
use zellij_tile::prelude::*;

/// Blank lines above the tree / spaces to the left of each row (breathing room).
const TOP_PAD: usize = 1;
const LEFT_PAD: usize = 1;

/// Attention is encoded as a suffix on the tab NAME — global state that every
/// sidebar instance reads identically (no per-instance divergence). We add it on
/// the attention pipe and strip it when the tab is focused; both are `rename_tab`
/// calls that mutate the shared tab name.
const MARK_WAITING: &str = " ⏳";
const MARK_COMPLETED: &str = " ✅";
/// Ongoing-work state (Claude is running): unlike the attention marks above it
/// is NOT cleared on focus — it ends when a waiting/completed/clear pipe arrives.
const MARK_WORKING: &str = " ⚙";

/// Default sidebar animation frames for `MARK_WORKING` (the tab name carries
/// only the static marker; the spinner lives purely in the render). Dense
/// braille (7 of 8 dots lit) fills the cell evenly, unlike the sparse ⠋⠙⠹ set
/// which sits visibly high-and-left next to ◆/✓. Override with the `spinner`
/// config key (each char = one frame; width-1 glyphs only).
const SPINNER_DEFAULT: &str = "⣾⣽⣻⢿⡿⣟⣯⣷";
const SPINNER_INTERVAL: f64 = 0.15;
/// Pane titles set by programs (OSC, e.g. Claude Code) don't trigger a
/// PaneUpdate, so the visible sidebar re-reads them this often (seconds).
const TITLE_INTERVAL: f64 = 2.0;

/// Each char of the `spinner` config value is one animation frame.
fn parse_spinner(config: Option<&String>) -> Vec<String> {
    let frames: Vec<String> = config
        .map(String::as_str)
        .unwrap_or(SPINNER_DEFAULT)
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(String::from)
        .collect();
    if frames.is_empty() {
        SPINNER_DEFAULT.chars().map(String::from).collect()
    } else {
        frames
    }
}

/// Group order + collapse state, shared across the per-tab plugin instances and
/// across restarts. `/cache` is mounted per plugin *location* (host side:
/// `~/.cache/zellij/<location>/plugin_cache`), so every instance sees the same
/// files; each re-reads its file on `TabUpdate`, which fires on every tab switch.
/// One file per *session* (suffix = session name, learned from `ModeUpdate`) —
/// different sessions have different groups, and a single shared file would let
/// each session's save wipe the others' state.
const STATE_FILE_PREFIX: &str = "/cache/vtabs-state-";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Attention {
    Waiting,
    Working,
    Completed,
}

fn marker_of(att: Attention) -> &'static str {
    match att {
        Attention::Waiting => MARK_WAITING,
        Attention::Working => MARK_WORKING,
        Attention::Completed => MARK_COMPLETED,
    }
}

/// Fallback grouping for auto-named tabs when no `autogroup_N` rule matches.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum AutoDefault {
    /// group = owning git repo's name (worktree-aware); non-repo dirs are not renamed
    Repo,
    /// plain name = cwd basename (no group)
    Dir,
    /// only rename when a rule matches
    #[default]
    Off,
}

/// Facts about a pane's shell, piped in by `shell/vtabs.zsh` (`k=v` lines).
/// Empty string = unknown/not applicable.
#[derive(Default, PartialEq, Debug)]
struct CwdFacts {
    cwd: String,
    /// `git rev-parse --path-format=absolute --show-toplevel`
    toplevel: String,
    /// `git rev-parse --path-format=absolute --git-common-dir` (main repo's .git)
    common: String,
    branch: String,
}

fn parse_facts(payload: &str) -> CwdFacts {
    let mut f = CwdFacts::default();
    for line in payload.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim_end_matches('/').to_string();
            match k {
                "cwd" => f.cwd = v,
                "toplevel" => f.toplevel = v,
                "common" => f.common = v,
                "branch" => f.branch = v,
                _ => {}
            }
        }
    }
    f
}

fn basename(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// Zellij's default names for unnamed tabs ("Tab #1", …) — the only names
/// auto-grouping is allowed to overwrite (manual names always win).
fn is_default_tab_name(name: &str) -> bool {
    name.strip_prefix("Tab #")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// `autogroup_N "glob -> group"` config keys, in numeric order.
fn parse_autogroup_rules(configuration: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut numbered: Vec<(u32, (String, String))> = configuration
        .iter()
        .filter_map(|(k, v)| {
            let n: u32 = k.strip_prefix("autogroup_")?.parse().ok()?;
            let (pattern, group) = v.split_once(" -> ")?;
            Some((n, (pattern.trim().to_string(), group.trim().to_string())))
        })
        .collect();
    numbered.sort_by_key(|(n, _)| *n);
    numbered.into_iter().map(|(_, rule)| rule).collect()
}

/// Derive `group:label` (or a plain name) for a pane's cwd facts.
/// None = leave the tab alone.
fn derive_auto_name(
    default: AutoDefault,
    rules: &[(String, String)],
    f: &CwdFacts,
) -> Option<String> {
    if f.cwd.is_empty() {
        return None;
    }
    // main repo's root dir, from its .git dir (worktrees share it)
    let owner_path = f.common.strip_suffix("/.git").unwrap_or("");
    let label = if !f.toplevel.is_empty() && !owner_path.is_empty() && f.toplevel != owner_path {
        basename(&f.toplevel) // linked worktree: its dir name (e.g. fix-auth)
    } else if !f.toplevel.is_empty() && f.cwd != f.toplevel {
        basename(&f.cwd)
    } else if !f.branch.is_empty() {
        &f.branch
    } else {
        basename(&f.cwd)
    };
    if label.is_empty() {
        return None; // e.g. cwd = "/" — never produce an empty name
    }
    let rule_group = rules.iter().find_map(|(pattern, group)| {
        (f.cwd == pattern.trim_end_matches("/**") || glob_match::glob_match(pattern, &f.cwd))
            .then_some(group.as_str())
    });
    match (rule_group, default) {
        (Some(g), _) => Some(format!("{}:{}", g, label)),
        (None, AutoDefault::Repo) => {
            let owner = if !owner_path.is_empty() {
                owner_path
            } else if !f.toplevel.is_empty() {
                &f.toplevel
            } else {
                return None; // not in a git repo: leave the tab alone
            };
            Some(format!("{}:{}", basename(owner), label))
        }
        (None, AutoDefault::Dir) => {
            let name = basename(&f.cwd);
            (!name.is_empty()).then(|| name.to_string())
        }
        (None, AutoDefault::Off) => None,
    }
}

/// Rollup priority for collapsed group headers: waiting > working > completed.
fn merge(acc: Option<Attention>, x: Attention) -> Attention {
    match (acc, x) {
        (Some(Attention::Waiting), _) | (_, Attention::Waiting) => Attention::Waiting,
        (Some(Attention::Working), _) | (_, Attention::Working) => Attention::Working,
        _ => Attention::Completed,
    }
}

/// Persisted sidebar state: group display order, collapsed groups, and — only
/// for groups the user explicitly reordered tabs in — per-group tab label order.
/// Groups absent from `tab_order` keep following Zellij's native tab positions.
#[derive(Default, PartialEq, Debug)]
struct Persisted {
    order: Vec<String>,
    collapsed: BTreeSet<String>,
    tab_order: BTreeMap<String, Vec<String>>,
}

/// One line per entry: `order <group>` / `collapsed <group>` /
/// `taborder <group>\t<label>` (labels of one group in order, one per line).
/// Names may contain anything but a newline (and, for groups, a tab).
/// Unknown lines are ignored.
fn parse_state(s: &str) -> Persisted {
    let mut p = Persisted::default();
    for line in s.lines() {
        if let Some(g) = line.strip_prefix("order ") {
            p.order.push(g.to_string());
        } else if let Some(g) = line.strip_prefix("collapsed ") {
            p.collapsed.insert(g.to_string());
        } else if let Some(rest) = line.strip_prefix("taborder ") {
            if let Some((g, label)) = rest.split_once('\t') {
                p.tab_order
                    .entry(g.to_string())
                    .or_default()
                    .push(label.to_string());
            }
        }
    }
    p
}

fn serialize_state(p: &Persisted) -> String {
    let mut out = String::new();
    for g in &p.order {
        out.push_str("order ");
        out.push_str(g);
        out.push('\n');
    }
    for g in &p.collapsed {
        out.push_str("collapsed ");
        out.push_str(g);
        out.push('\n');
    }
    for (g, labels) in &p.tab_order {
        for label in labels {
            out.push_str("taborder ");
            out.push_str(g);
            out.push('\t');
            out.push_str(label);
            out.push('\n');
        }
    }
    out
}

/// Stable-sort items into saved label order; labels not in `saved` keep their
/// native relative order after the saved ones (and duplicates stay stable).
fn sort_by_saved(items: &mut [TabItem], saved: &[String]) {
    items.sort_by_key(|it| {
        saved
            .iter()
            .position(|l| l == &it.label)
            .unwrap_or(usize::MAX)
    });
}

/// Saved order first (dropping groups that no longer exist), then any new
/// groups in first-appearance order.
fn merge_order(saved: &[String], appearance: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = saved
        .iter()
        .filter(|g| appearance.contains(g))
        .cloned()
        .collect();
    for g in appearance {
        if !out.contains(&g) {
            out.push(g);
        }
    }
    out
}

/// Rename the first occurrence of `old` in a saved label order.
fn rename_label(order: &mut [String], old: &str, new: &str) {
    if let Some(l) = order.iter_mut().find(|l| l.as_str() == old) {
        *l = new.to_string();
    }
}

/// Swap `name` with its neighbor `delta` steps away; false if absent or at the edge.
fn move_in(order: &mut [String], name: &str, delta: isize) -> bool {
    let Some(i) = order.iter().position(|g| g == name) else {
        return false;
    };
    let j = i as isize + delta;
    if j < 0 || j as usize >= order.len() {
        return false;
    }
    order.swap(i, j as usize);
    true
}

/// Split the attention marker off a tab name: "work:api ⏳" -> (Waiting, "work:api").
fn parse_attention(name: &str) -> (Option<Attention>, &str) {
    if let Some(base) = name.strip_suffix(MARK_WAITING) {
        (Some(Attention::Waiting), base)
    } else if let Some(base) = name.strip_suffix(MARK_COMPLETED) {
        (Some(Attention::Completed), base)
    } else if let Some(base) = name.strip_suffix(MARK_WORKING) {
        (Some(Attention::Working), base)
    } else {
        (None, name)
    }
}

#[derive(Default)]
struct State {
    tabs: Vec<TabInfo>,
    /// terminal pane id -> tab position (rebuilt on each PaneUpdate)
    pane_tab: BTreeMap<u32, usize>,
    collapsed: BTreeSet<String>,
    /// saved group display order; groups not listed here follow in first-appearance order
    group_order: Vec<String>,
    /// per-group saved tab label order; groups not present follow native tab positions
    tab_order: BTreeMap<String, Vec<String>>,
    /// session name (from ModeUpdate) — state persistence is a no-op until known
    session: Option<String>,
    selected: usize,
    separator: char,
    waiting_icon: String,
    completed_icon: String,
    autogroup_default: AutoDefault,
    autogroup_rules: Vec<(String, String)>,
    /// active inline rename edit: (target, input buffer)
    renaming: Option<(RenameTarget, String)>,
    /// animation frames (from the `spinner` config key), frame counter, and
    /// whether a Timer event is already scheduled
    spinner: Vec<String>,
    spin: usize,
    timer_running: bool,
    /// `pinned` config key (default true): once permissions are granted the pane
    /// turns non-selectable like tab-bar/status-bar — MovePane and focus moves skip
    /// it, mouse clicks still work. "false" keeps the pane focusable for keys.
    pinned: bool,
    /// Keyboard mode of a pinned sidebar (`vtabs-focus` pipe, e.g. from a
    /// MessagePlugin keybind): the pane is made selectable and focused until
    /// Esc, Enter on a tab, or focus leaving it; then it is pinned again.
    my_id: u32,
    my_tab: Option<usize>,
    /// terminal pane focused in this tab before keyboard mode — focus returns there
    prev_focus: Option<u32>,
    keys_on: bool,
    keys_focus_seen: bool,
    /// tab position -> plugin id of the vtabs instance in that tab (from PaneUpdate),
    /// to hand keyboard mode over when browsing switches the visible tab
    sidebar_by_tab: BTreeMap<usize, u32>,
    /// tab where keyboard mode started — Esc goes back there
    keys_origin: Option<usize>,
    /// tab position -> its tiled terminal panes (id, title, focused) in screen order
    tab_panes: BTreeMap<usize, Vec<(u32, String, bool)>>,
    /// interval of the scheduled Timer and time since the last title refresh
    timer_interval: f64,
    title_elapsed: f64,
    /// keyboard mode: pane row handed over with the keys — re-selected after the
    /// TabUpdate that follows the switch (which would select the tab row)
    pending_pane: Option<u32>,
    /// pane whose frame is highlighted while its row is selected in keyboard mode
    highlighted: Option<u32>,
    /// own tab has had a terminal pane — guards auto-close against a tab that
    /// is still being created (plugins first, terminal a moment later)
    had_terminal: bool,
    /// own tab looked empty in the last PaneUpdate; closed on the next Timer if
    /// still so (a manifest mid tab-close can show a neighbour's panes split
    /// across old and new positions — closing at once hit the wrong tab)
    close_pending: bool,
    /// `show_single_pane` config key: list the pane under a one-pane tab too
    show_single_pane: bool,
    /// plugin pane id -> tab position (tab-bar, floating plugins like the startup
    /// tip…), so a focused plugin pane also tells which tab is on screen
    plugin_tab: BTreeMap<u32, usize>,
    /// tab id -> name the sidebar gave it from its only pane's title; a tab whose
    /// name no longer matches was renamed by hand and is left alone
    auto_named: BTreeMap<usize, String>,
}

enum RenameTarget {
    Group(String),
    /// tab position; edits the tab's label (group prefix is kept)
    Tab(usize),
}

// The plugin entry points call zellij-tile's wasm host imports, which don't exist
// on the host target — gate them out of `cargo test` so the pure logic can be tested.
#[cfg(not(test))]
register_plugin!(State);

/// A tab within a group, while building the tree.
struct TabItem {
    position: usize,
    label: String,
    active: bool,
    attention: Option<Attention>,
}

enum Row {
    Group { name: String, collapsed: bool, count: usize, attention: Option<Attention> },
    Tab { position: usize, label: String, active: bool, attention: Option<Attention> },
    /// a terminal pane of the tab above, listed when the tab has 2+ panes;
    /// keyboard mode browses onto it (frame highlighted), Enter/click focuses it
    Pane { id: u32, title: String, focused: bool, last: bool },
}

impl State {
    fn group_of<'a>(&self, name: &'a str) -> (String, &'a str) {
        match name.find(self.separator) {
            Some(i) => (
                name[..i].to_string(),
                name[i + self.separator.len_utf8()..].trim_start(),
            ),
            None => ("General".to_string(), name),
        }
    }

    /// Groups in first-appearance (tab position) order.
    fn appearance_groups(&self) -> Vec<String> {
        let mut out = Vec::new();
        for t in &self.tabs {
            let (_, base) = parse_attention(&t.name);
            let (g, _) = self.group_of(base);
            if !out.contains(&g) {
                out.push(g);
            }
        }
        out
    }

    fn display_group_order(&self) -> Vec<String> {
        merge_order(&self.group_order, self.appearance_groups())
    }

    fn state_file(&self) -> Option<String> {
        self.session
            .as_ref()
            .map(|s| format!("{}{}", STATE_FILE_PREFIX, s))
    }

    fn load_state(&mut self) {
        let Some(path) = self.state_file() else {
            return;
        };
        if let Ok(s) = std::fs::read_to_string(path) {
            let p = parse_state(&s);
            self.group_order = p.order;
            self.collapsed = p.collapsed;
            self.tab_order = p.tab_order;
        }
    }

    fn save_state(&self) {
        let Some(path) = self.state_file() else {
            return;
        };
        let p = Persisted {
            order: self.display_group_order(),
            collapsed: self.collapsed.clone(),
            tab_order: self.tab_order.clone(),
        };
        let _ = std::fs::write(path, serialize_state(&p));
    }

    /// Groups in display order, each with its tabs in display order.
    fn grouped_items(&self) -> Vec<(String, Vec<TabItem>)> {
        let mut groups: BTreeMap<String, Vec<TabItem>> = BTreeMap::new();
        for t in &self.tabs {
            let (attention, base) = parse_attention(&t.name);
            let (g, label) = self.group_of(base);
            groups.entry(g).or_default().push(TabItem {
                position: t.position,
                label: label.to_string(),
                active: t.active,
                attention,
            });
        }
        self.display_group_order()
            .iter()
            .filter_map(|g| {
                let mut items = groups.remove(g)?;
                if let Some(saved) = self.tab_order.get(g) {
                    sort_by_saved(&mut items, saved);
                }
                Some((g.clone(), items))
            })
            .collect()
    }

    fn build_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (g, items) in self.grouped_items() {
            let g = &g;
            let collapsed = self.collapsed.contains(g);
            let group_att = items.iter().fold(None, |acc, it| match it.attention {
                Some(x) => Some(merge(acc, x)),
                None => acc,
            });
            rows.push(Row::Group {
                name: g.clone(),
                collapsed,
                count: items.len(),
                attention: group_att,
            });
            if !collapsed {
                for it in items {
                    let panes = self.tab_panes.get(&it.position);
                    rows.push(Row::Tab {
                        position: it.position,
                        label: it.label,
                        active: it.active,
                        attention: it.attention,
                    });
                    if let Some(panes) = panes.filter(|p| p.len() > 1 || self.show_single_pane && !p.is_empty()) {
                        for (i, (id, title, focused)) in panes.iter().enumerate() {
                            rows.push(Row::Pane {
                                id: *id,
                                title: title.clone(),
                                focused: *focused,
                                last: i + 1 == panes.len(),
                            });
                        }
                    }
                }
            }
        }
        rows
    }

    fn active_row_index(&self) -> Option<usize> {
        self.build_rows()
            .iter()
            .position(|r| matches!(r, Row::Tab { active: true, .. }))
    }

    fn icon(&self, att: Option<Attention>) -> String {
        match att {
            Some(Attention::Waiting) => format!("\u{1b}[33m{}\u{1b}[39m ", self.waiting_icon),
            Some(Attention::Working) => {
                let frame = self
                    .spinner
                    .get(self.spin % self.spinner.len().max(1))
                    .map(String::as_str)
                    .unwrap_or("⣾");
                format!("\u{1b}[36m{}\u{1b}[39m ", frame)
            }
            Some(Attention::Completed) => format!("\u{1b}[32m{}\u{1b}[39m ", self.completed_icon),
            None => String::new(),
        }
    }

    fn any_working(&self) -> bool {
        self.tabs
            .iter()
            .any(|t| parse_attention(&t.name).0 == Some(Attention::Working))
    }

    /// Arm the animation timer if something is working and none is scheduled.
    fn ensure_timer(&mut self) {
        if !self.timer_running {
            self.timer_interval = if self.any_working() { SPINNER_INTERVAL } else { TITLE_INTERVAL };
            set_timeout(self.timer_interval);
            self.timer_running = true;
        }
    }

    /// Re-read pane titles if this instance is the visible one; true if any changed.
    fn refresh_titles(&mut self) -> bool {
        let visible = match get_focused_pane_info() {
            Ok((_, PaneId::Terminal(id))) => self.pane_tab.get(&id) == self.my_tab.as_ref(),
            Ok((_, PaneId::Plugin(id))) => self.plugin_tab.get(&id) == self.my_tab.as_ref(),
            Err(_) => false,
        };
        if !visible {
            return false;
        }
        let mut changed = false;
        for panes in self.tab_panes.values_mut() {
            for (id, title, _) in panes.iter_mut() {
                if let Some(info) = get_pane_info(PaneId::Terminal(*id)) {
                    let t = short_title(&info.title);
                    if *title != t {
                        *title = t;
                        changed = true;
                    }
                }
            }
        }
        self.auto_name_tabs();
        changed
    }

    /// A one-pane tab is named after its pane (Claude's session name, the shell's
    /// directory) and follows it — unless the tab was renamed by hand. Run by the
    /// visible instance only: its tab list is fresh (hidden ones miss TabUpdates).
    fn auto_name_tabs(&mut self) {
        let mut renames = Vec::new();
        for t in &self.tabs {
            let Some(panes) = self.tab_panes.get(&t.position) else { continue };
            if panes.len() != 1 {
                continue;
            }
            let want = tab_name_from_pane(&panes[0].1);
            let (att, base) = parse_attention(&t.name);
            let ours = self.auto_named.get(&t.tab_id).map(String::as_str) == Some(base);
            if want.is_empty() || base == want {
                // equal names (e.g. after a session restore) re-adopt the tab
                if !want.is_empty() {
                    self.auto_named.insert(t.tab_id, want);
                }
                continue;
            }
            if is_default_tab_name(base) || ours {
                let marker = att.map(marker_of).unwrap_or("");
                renames.push((t.tab_id, want, marker));
            } else {
                self.auto_named.remove(&t.tab_id);
            }
        }
        for (tab_id, want, marker) in renames {
            rename_tab_with_id(tab_id as u64, format!("{}{}", want, marker));
            self.auto_named.insert(tab_id, want);
        }
    }

    /// Server-fresh info for the tab containing `pane_id`. Pipe-driven renames
    /// must never derive a tab's name from this instance's cached tab list: the
    /// caches of hidden instances diverge, pipes broadcast to every instance,
    /// and one stale cache writing `cached name + marker` resurrects an old tab
    /// name (user renames kept reverting). Only the pane→tab-id mapping comes
    /// from the cache (ids are stable); name and active flags are queried live,
    /// so any number of instances can handle the same pipe idempotently — which
    /// is also required, because no single instance reliably receives pipes.
    fn fresh_tab(&self, pane_id: u32) -> Option<TabInfo> {
        let tab_pos = *self.pane_tab.get(&pane_id)?;
        let tab_id = self.tabs.iter().find(|t| t.position == tab_pos)?.tab_id;
        get_tab_info(tab_id)
    }

    /// Add an attention marker to the tab containing `pane_id` (global rename).
    fn set_attention(&self, pane_id: u32, attention: Attention) {
        let Some(t) = self.fresh_tab(pane_id) else {
            return;
        };
        let (att, base) = parse_attention(&t.name);
        // Never mark the tab you're already looking at — you don't need an
        // attention cue for it, and it keeps set/clear free of any race.
        // But a working spinner on it still has to END now.
        if t.active {
            if att == Some(Attention::Working) {
                rename_tab_with_id(t.tab_id as u64, base.to_string());
            }
            return;
        }
        if att == Some(attention) {
            return;
        }
        rename_tab_with_id(t.tab_id as u64, format!("{}{}", base, marker_of(attention)));
    }

    /// Mark the tab containing `pane_id` as working. Unlike attention marks
    /// this applies to the active tab too (it is not cleared on focus, so
    /// there is no set/clear race — only pipes ever end it).
    fn set_working(&self, pane_id: u32) {
        if let Some(t) = self.fresh_tab(pane_id) {
            let (att, base) = parse_attention(&t.name);
            if att != Some(Attention::Working) {
                rename_tab_with_id(t.tab_id as u64, format!("{}{}", base, MARK_WORKING));
            }
        }
    }

    /// Strip a working marker (Claude exited without a Stop) — attention marks stay.
    fn clear_working(&self, pane_id: u32) {
        if let Some(t) = self.fresh_tab(pane_id) {
            let (att, base) = parse_attention(&t.name);
            if att == Some(Attention::Working) {
                rename_tab_with_id(t.tab_id as u64, base.to_string());
            }
        }
    }

    /// Auto-name the tab containing `pane_id` from its shell's cwd facts.
    /// Unless `force`, only tabs still carrying a default "Tab #N" name are touched.
    fn auto_rename(&self, pane_id: u32, payload: &str, force: bool) {
        let Some(t) = self.fresh_tab(pane_id) else {
            return;
        };
        let (att, base) = parse_attention(&t.name);
        if !force && !is_default_tab_name(base) {
            return;
        }
        let facts = parse_facts(payload);
        let Some(name) = derive_auto_name(self.autogroup_default, &self.autogroup_rules, &facts)
        else {
            return;
        };
        if base == name {
            return;
        }
        let marker = att.map(marker_of).unwrap_or("");
        rename_tab_with_id(t.tab_id as u64, format!("{}{}", name, marker));
    }

    /// Strip a *seen* attention marker from the tab at `pos` (global rename).
    /// A working spinner survives focus — peeking at a running task must not
    /// kill its indicator; only waiting/completed are "acknowledged by looking".
    fn clear_tab(&self, pos: usize) {
        if let Some(t) = self.tabs.iter().find(|t| t.position == pos) {
            let (att, base) = parse_attention(&t.name);
            if matches!(att, Some(Attention::Waiting) | Some(Attention::Completed)) {
                rename_tab_with_id(t.tab_id as u64, base.to_string());
            }
        }
    }

    fn activate(&mut self, idx: usize) -> bool {
        let rows = self.build_rows();
        if idx >= rows.len() {
            return false;
        }
        match &rows[idx] {
            Row::Group { name, .. } => {
                if self.collapsed.contains(name) {
                    self.collapsed.remove(name);
                } else {
                    self.collapsed.insert(name.clone());
                }
                self.save_state();
                true
            }
            Row::Tab { position, .. } => {
                switch_tab_to(*position as u32 + 1);
                true
            }
            Row::Pane { id, .. } => {
                focus_terminal_pane(*id, false, false);
                true
            }
        }
    }

    /// Move the row under the selection up/down (persisted): a group header
    /// reorders the group, a tab row reorders the tab within its group.
    fn move_selected(&mut self, delta: isize) -> bool {
        let rows = self.build_rows();
        match rows.get(self.selected) {
            Some(Row::Group { name, .. }) => {
                let name = name.clone();
                let mut order = self.display_group_order();
                if !move_in(&mut order, &name, delta) {
                    return false;
                }
                self.group_order = order;
                self.save_state();
                // keep the selection on the header we just moved
                if let Some(idx) = self
                    .build_rows()
                    .iter()
                    .position(|r| matches!(r, Row::Group { name: n, .. } if *n == name))
                {
                    self.selected = idx;
                }
                true
            }
            Some(Row::Tab { position, label, .. }) => {
                let (position, label) = (*position, label.clone());
                let Some((group, items)) = self
                    .grouped_items()
                    .into_iter()
                    .find(|(_, items)| items.iter().any(|it| it.position == position))
                else {
                    return false;
                };
                let mut labels: Vec<String> =
                    items.into_iter().map(|it| it.label).collect();
                if !move_in(&mut labels, &label, delta) {
                    return false;
                }
                let present = self.appearance_groups();
                self.tab_order.retain(|g, _| present.contains(g));
                self.tab_order.insert(group, labels);
                self.save_state();
                // keep the selection on the tab we just moved
                if let Some(idx) = self
                    .build_rows()
                    .iter()
                    .position(|r| matches!(r, Row::Tab { position: p, .. } if *p == position))
                {
                    self.selected = idx;
                }
                true
            }
            Some(Row::Pane { .. }) | None => false,
        }
    }

    /// Move `old` group's saved order/collapse/tab-order entries to `new`.
    fn migrate_group_state(&mut self, old: &str, new: &str) {
        for g in self.group_order.iter_mut() {
            if g == old {
                *g = new.to_string();
            }
        }
        // renaming into an existing group must not leave a duplicate entry
        let mut seen = BTreeSet::new();
        self.group_order.retain(|g| seen.insert(g.clone()));
        if self.collapsed.remove(old) {
            self.collapsed.insert(new.to_string());
        }
        if let Some(order) = self.tab_order.remove(old) {
            self.tab_order.entry(new.to_string()).or_insert(order);
        }
    }

    /// Rename group `old` to `new`: re-prefix every member tab (preserving
    /// attention marks) and migrate the group's saved state.
    fn commit_rename(&mut self, old: &str, new: &str) {
        let new = new.trim();
        if new.is_empty() || new == old {
            return;
        }
        for t in &self.tabs {
            let (att, base) = parse_attention(&t.name);
            let (g, label) = self.group_of(base);
            if g != old {
                continue;
            }
            let marker = att.map(marker_of).unwrap_or("");
            rename_tab(
                t.position as u32 + 1,
                format!("{}{}{}{}", new, self.separator, label, marker),
            );
        }
        self.migrate_group_state(old, new);
        self.save_state();
    }

    /// Rename the label of the tab at `position`, keeping its group prefix
    /// (ungrouped tabs stay ungrouped) and attention mark.
    fn commit_tab_rename(&mut self, position: usize, new_label: &str) {
        let new_label = new_label.trim();
        let Some(t) = self.tabs.iter().find(|t| t.position == position) else {
            return;
        };
        let (att, base) = parse_attention(&t.name);
        let (g, old_label) = self.group_of(base);
        if new_label.is_empty() || new_label == old_label {
            return;
        }
        let marker = att.map(marker_of).unwrap_or("");
        let name = if base.find(self.separator).is_none() {
            new_label.to_string()
        } else {
            format!("{}{}{}", g, self.separator, new_label)
        };
        rename_tab(position as u32 + 1, format!("{}{}", name, marker));
        if let Some(order) = self.tab_order.get_mut(&g) {
            rename_label(order, old_label, new_label);
            self.save_state();
        }
    }

    fn handle_rename_key(&mut self, key: KeyWithModifier) -> bool {
        let Some((target, mut buf)) = self.renaming.take() else {
            return false;
        };
        let plain = key.key_modifiers.is_empty()
            || (key.key_modifiers.len() == 1 && key.key_modifiers.contains(&KeyModifier::Shift));
        match key.bare_key {
            BareKey::Enter => {
                match &target {
                    RenameTarget::Group(old) => self.commit_rename(&old.clone(), &buf),
                    RenameTarget::Tab(pos) => self.commit_tab_rename(*pos, &buf),
                }
                return true;
            }
            BareKey::Esc => return true, // buffer dropped = cancelled
            BareKey::Backspace => {
                buf.pop();
            }
            // separator/tab would corrupt group parsing / the state file format
            BareKey::Char(c) if plain && !c.is_control() && c != self.separator => {
                buf.push(c);
            }
            _ => {}
        }
        self.renaming = Some((target, buf));
        true
    }

    /// Next row from `from` in direction `down`; stays put at the ends.
    fn step(rows: &[Row], from: usize, down: bool) -> usize {
        if down {
            (from + 1).min(rows.len().saturating_sub(1))
        } else {
            from.saturating_sub(1)
        }
    }

    /// Frame-highlight `pane` (None: just clear the previous highlight).
    fn highlight(&mut self, pane: Option<u32>) {
        if self.highlighted == pane {
            return;
        }
        let off: Vec<PaneId> = self.highlighted.map(PaneId::Terminal).into_iter().collect();
        let on: Vec<PaneId> = pane.map(PaneId::Terminal).into_iter().collect();
        highlight_and_unhighlight_panes(on, off);
        self.highlighted = pane;
    }

    fn select_pending_pane(&mut self) {
        let Some(id) = self.pending_pane else { return };
        if let Some(i) = self
            .build_rows()
            .iter()
            .position(|r| matches!(r, Row::Pane { id: p, .. } if *p == id))
        {
            self.selected = i;
            self.highlight(Some(id));
        }
    }

    fn keys_on_here(&mut self, origin: Option<usize>) {
        self.keys_on = true;
        self.keys_focus_seen = false;
        self.keys_origin = origin;
        set_selectable(true);
        focus_plugin_pane(self.my_id, false, false);
    }

    /// Browsing onto another tab shows it right away; keyboard mode moves to
    /// that tab's sidebar instance (only one plugin pane can hold the keys).
    fn browse_to(&mut self, position: usize, pane: Option<u32>) {
        let Some(&target) = self.sidebar_by_tab.get(&position) else {
            return;
        };
        let origin = self.keys_origin;
        self.keys_off(false);
        switch_tab_to(position as u32 + 1);
        // payload "origin_tab:pane_id", either part may be empty
        let payload = format!(
            "{}:{}",
            origin.map(|o| o.to_string()).unwrap_or_default(),
            pane.map(|p| p.to_string()).unwrap_or_default()
        );
        pipe_message_to_plugin(
            MessageToPlugin::new("vtabs-take-keys")
                .with_destination_plugin_id(target)
                .with_payload(payload),
        );
    }

    /// Keyboard mode moved the selection to row `idx`: show its tab, and for a
    /// pane row highlight that pane's frame.
    fn browse_row(&mut self, rows: &[Row], idx: usize) {
        self.pending_pane = None;
        match rows.get(idx) {
            Some(Row::Tab { position, .. }) => {
                if Some(*position) != self.my_tab {
                    self.browse_to(*position, None);
                } else {
                    self.highlight(None);
                }
            }
            Some(Row::Pane { id, .. }) => {
                let tab = self.pane_tab.get(id).copied();
                if tab.is_some() && tab != self.my_tab {
                    self.browse_to(tab.unwrap(), Some(*id));
                } else {
                    self.highlight(Some(*id));
                }
            }
            _ => self.highlight(None),
        }
    }

    fn keys_off(&mut self, restore_focus: bool) {
        self.keys_on = false;
        self.keys_focus_seen = false;
        self.pending_pane = None;
        self.highlight(None);
        if self.pinned {
            set_selectable(false);
        }
        if restore_focus {
            if let Some(id) = self.prev_focus {
                focus_terminal_pane(id, false, false);
            }
        }
    }

    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        if self.renaming.is_some() {
            return self.handle_rename_key(key);
        }
        let len = self.build_rows().len();
        if len == 0 {
            return false;
        }
        let shift = key.key_modifiers.contains(&KeyModifier::Shift);
        if self.keys_on {
            let rows = self.build_rows();
            match key.bare_key {
                // Esc: cancel browsing — back to the tab where keyboard mode started
                BareKey::Esc => {
                    match self.keys_origin {
                        Some(o) if Some(o) != self.my_tab => {
                            self.keys_off(false);
                            switch_tab_to(o as u32 + 1);
                        }
                        _ => self.keys_off(true),
                    }
                    return true;
                }
                // Enter on a tab: it is already shown, just give the keys back to it
                BareKey::Enter => {
                    if let Some(Row::Pane { id, .. }) = rows.get(self.selected) {
                        let id = *id;
                        self.keys_off(false);
                        focus_terminal_pane(id, false, false);
                        return true;
                    }
                    if let Some(Row::Tab { position, .. }) = rows.get(self.selected) {
                        if Some(*position) == self.my_tab {
                            self.keys_off(true);
                            return true;
                        }
                        let p = *position;
                        self.keys_off(false);
                        switch_tab_to(p as u32 + 1);
                        return true;
                    }
                }
                BareKey::Char('j') | BareKey::Down | BareKey::Char('k') | BareKey::Up if !shift => {
                    let down = matches!(key.bare_key, BareKey::Char('j') | BareKey::Down);
                    self.selected = Self::step(&rows, self.selected, down);
                    self.browse_row(&rows, self.selected);
                    return true;
                }
                _ => {}
            }
        }
        match key.bare_key {
            // shifted chars arrive as their uppercase form, arrows carry the modifier
            BareKey::Char('J') => self.move_selected(1),
            BareKey::Char('K') => self.move_selected(-1),
            BareKey::Down if shift => self.move_selected(1),
            BareKey::Up if shift => self.move_selected(-1),
            BareKey::Char('j') | BareKey::Down => {
                self.selected = Self::step(&self.build_rows(), self.selected, true);
                true
            }
            BareKey::Char('k') | BareKey::Up => {
                self.selected = Self::step(&self.build_rows(), self.selected, false);
                true
            }
            BareKey::Enter | BareKey::Char(' ') => {
                let sel = self.selected;
                self.activate(sel)
            }
            BareKey::Char('r') => {
                let rows = self.build_rows();
                match rows.get(self.selected) {
                    Some(Row::Group { name, .. }) => {
                        self.renaming =
                            Some((RenameTarget::Group(name.clone()), name.clone()));
                        true
                    }
                    Some(Row::Tab { position, label, .. }) => {
                        self.renaming =
                            Some((RenameTarget::Tab(*position), label.clone()));
                        true
                    }
                    Some(Row::Pane { .. }) | None => false,
                }
            }
            _ => false,
        }
    }

    fn handle_mouse(&mut self, mouse: Mouse) -> bool {
        if self.renaming.take().is_some() {
            return true; // any mouse action cancels an in-progress rename
        }
        let len = self.build_rows().len();
        if len == 0 {
            return false;
        }
        match mouse {
            Mouse::LeftClick(row, _col) => {
                let idx = row - TOP_PAD as isize;
                if idx < 0 || idx as usize >= len {
                    return false;
                }
                let idx = idx as usize;
                self.selected = idx;
                self.activate(idx);
                true
            }
            Mouse::ScrollUp(_) => {
                self.selected = Self::step(&self.build_rows(), self.selected, false);
                true
            }
            Mouse::ScrollDown(_) => {
                self.selected = Self::step(&self.build_rows(), self.selected, true);
                true
            }
            _ => false,
        }
    }
}

#[cfg(not(test))]
impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.separator = configuration
            .get("separator")
            .and_then(|s| s.chars().next())
            .unwrap_or(':');
        self.waiting_icon = configuration
            .get("waiting_icon")
            .cloned()
            .unwrap_or_else(|| "◆".to_string());
        self.completed_icon = configuration
            .get("completed_icon")
            .cloned()
            .unwrap_or_else(|| "✓".to_string());
        self.autogroup_default = match configuration
            .get("autogroup_default")
            .map(String::as_str)
        {
            Some("repo") => AutoDefault::Repo,
            Some("dir") => AutoDefault::Dir,
            _ => AutoDefault::Off,
        };
        self.autogroup_rules = parse_autogroup_rules(&configuration);
        self.spinner = parse_spinner(configuration.get("spinner"));
        self.pinned = configuration.get("pinned").map(String::as_str) != Some("false");
        self.show_single_pane =
            configuration.get("show_single_pane").map(String::as_str) == Some("true");
        self.my_id = get_plugin_ids().plugin_id;
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::ReadCliPipes,
            // keyboard-mode hand-over between the per-tab instances (vtabs-take-keys)
            PermissionType::MessageAndLaunchOtherPlugins,
        ]);
        subscribe(&[
            EventType::ModeUpdate,
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::Key,
            EventType::Mouse,
            EventType::Timer,
            EventType::PermissionRequestResult,
        ]);
        self.ensure_timer();
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            // Not in load(): a non-selectable pane can't take the `y` answer to the
            // permission prompt, so pin only after the grant (cached grants fire it too).
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                if self.pinned {
                    set_selectable(false);
                }
                false
            }
            Event::ModeUpdate(mode_info) => {
                if mode_info.session_name != self.session {
                    self.session = mode_info.session_name;
                    self.load_state();
                    return true;
                }
                false
            }
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                // Pick up order/collapse changes written by other instances —
                // every tab switch fires a TabUpdate, so the visible sidebar
                // is always freshly synced.
                self.load_state();
                // Clear on focus: the active tab is "seen", so strip its marker.
                // Safe on every TabUpdate because set_attention never marks the
                // active tab, so there's no marker here to race with.
                if let Some(pos) = self.tabs.iter().find(|t| t.active).map(|t| t.position) {
                    self.clear_tab(pos);
                }
                // Keyboard mode: keep a selection already inside this tab — TabUpdates
                // also fire on pane highlight/focus changes, and resetting to the tab
                // row there made browsing bounce back to the first panes.
                let keep = self.keys_on
                    && match self.build_rows().get(self.selected) {
                        Some(Row::Tab { position, .. }) => Some(*position) == self.my_tab,
                        Some(Row::Pane { id, .. }) => self.pane_tab.get(id) == self.my_tab.as_ref(),
                        _ => false,
                    };
                if self.pending_pane.is_some() {
                    self.select_pending_pane();
                } else if keep {
                } else if let Some(i) = self.active_row_index() {
                    self.selected = i;
                } else {
                    let len = self.build_rows().len();
                    if len > 0 && self.selected >= len {
                        self.selected = len - 1;
                    }
                }
                self.ensure_timer();
                true
            }
            Event::Timer(_) => {
                if self.close_pending {
                    self.close_pending = false;
                    self.had_terminal = false;
                    if let Some(my_tab) = self.my_tab {
                        close_tab_with_index(my_tab);
                    }
                    // no early return: this may be the title/spinner timer tick
                }
                self.timer_running = false;
                let mut redraw = false;
                if self.any_working() {
                    self.spin = self.spin.wrapping_add(1);
                    redraw = true;
                }
                self.title_elapsed += self.timer_interval;
                if self.title_elapsed >= TITLE_INTERVAL {
                    self.title_elapsed = 0.0;
                    redraw |= self.refresh_titles();
                }
                self.ensure_timer();
                redraw
            }
            Event::PaneUpdate(manifest) => {
                self.pane_tab.clear();
                let mut me_focused = false;
                let my_url = manifest
                    .panes
                    .values()
                    .flatten()
                    .find(|p| p.is_plugin && p.id == self.my_id)
                    .and_then(|p| p.plugin_url.clone());
                self.sidebar_by_tab.clear();
                self.tab_panes.clear();
                self.plugin_tab.clear();
                for (tab_pos, panes) in &manifest.panes {
                    let mut list: Vec<_> = panes
                        .iter()
                        .filter(|p| !p.is_plugin && !p.is_floating && !p.is_suppressed)
                        .collect();
                    list.sort_by_key(|p| (p.pane_y, p.pane_x));
                    self.tab_panes.insert(
                        *tab_pos,
                        list.iter().map(|p| (p.id, short_title(&p.title), p.is_focused)).collect(),
                    );
                    for p in panes {
                        if !p.is_plugin {
                            self.pane_tab.insert(p.id, *tab_pos);
                        } else {
                            self.plugin_tab.insert(p.id, *tab_pos);
                            if p.id == self.my_id {
                                self.my_tab = Some(*tab_pos);
                                me_focused = p.is_focused;
                            }
                            if my_url.is_some() && p.plugin_url == my_url {
                                self.sidebar_by_tab.insert(*tab_pos, p.id);
                            }
                        }
                    }
                }
                // Auto-close: Zellij keeps a tab while any pane is left, and the sidebar
                // is one — so `exit` in the last terminal left a tab with just the
                // sidebar. Each instance closes only its own tab.
                if let Some(my_tab) = self.my_tab {
                    let terminals = manifest
                        .panes
                        .get(&my_tab)
                        .map_or(0, |ps| ps.iter().filter(|p| !p.is_plugin).count());
                    if terminals > 0 {
                        self.had_terminal = true;
                        self.close_pending = false;
                    } else if self.had_terminal && !self.close_pending {
                        self.close_pending = true;
                        set_timeout(0.5);
                    }
                }
                if let Some(my_tab) = self.my_tab {
                    if !self.keys_on {
                        if let Some(p) = manifest.panes.get(&my_tab).and_then(|ps| {
                            ps.iter().find(|p| !p.is_plugin && p.is_focused && !p.is_floating)
                        }) {
                            self.prev_focus = Some(p.id);
                        }
                    }
                }
                if self.keys_on {
                    // plugin ids are unique per instance, so the pane alone identifies us
                    let me_focused = me_focused
                        && matches!(get_focused_pane_info(), Ok((_, PaneId::Plugin(id))) if id == self.my_id);
                    if me_focused {
                        self.keys_focus_seen = true;
                    } else if self.keys_focus_seen {
                        // focus left (click elsewhere, Alt+arrow): pin again, keep focus where it went
                        self.keys_off(false);
                        return true;
                    }
                }
                false
            }
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            _ => false,
        }
    }

    /// Attention signals: `zellij-vtabs::waiting|completed::<pane_id>` (broadcast CLI pipe).
    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        if pipe_message.name == "vtabs-focus" {
            // Every instance (one per tab) gets the message; only the one whose tab
            // holds the client's focused pane acts. Not self.tabs: hidden instances
            // miss TabUpdates, so each believes its own tab is still the active one.
            // Not the returned tab index either: it is the tab *id*, which drifts
            // from the position once a tab is closed. pane_tab is fresh (PaneUpdate).
            let Ok((_, PaneId::Terminal(focused))) = get_focused_pane_info() else {
                return false;
            };
            if self.pane_tab.get(&focused) == self.my_tab.as_ref() && !self.keys_on {
                self.prev_focus = Some(focused);
                self.keys_on_here(self.my_tab);
                return true;
            }
            return false;
        }
        // hand-over from the instance of the previously shown tab (payload: origin tab)
        if pipe_message.name == "vtabs-take-keys" {
            if !self.keys_on {
                let (origin, pane) = pipe_message.payload.as_deref().unwrap_or("").split_once(':').unwrap_or(("", ""));
                self.keys_on_here(origin.parse().ok());
                if let Ok(id) = pane.parse::<u32>() {
                    self.pending_pane = Some(id);
                    self.select_pending_pane();
                }
                return true;
            }
            return false;
        }
        if let Some(rest) = pipe_message.name.strip_prefix("zellij-vtabs::") {
            let parts: Vec<&str> = rest.split("::").collect();
            if parts.len() == 2 {
                if let Ok(pane_id) = parts[1].parse::<u32>() {
                    let payload = pipe_message.payload.as_deref().unwrap_or("");
                    match parts[0] {
                        "waiting" => {
                            self.set_attention(pane_id, Attention::Waiting);
                            return false;
                        }
                        "completed" => {
                            self.set_attention(pane_id, Attention::Completed);
                            return false;
                        }
                        "cwd" => {
                            self.auto_rename(pane_id, payload, false);
                            return false;
                        }
                        "cwd-force" => {
                            self.auto_rename(pane_id, payload, true);
                            return false;
                        }
                        "working" => {
                            self.set_working(pane_id);
                            return false;
                        }
                        "clear-working" => {
                            self.clear_working(pane_id);
                            return false;
                        }
                        _ => {}
                    }
                }
            }
        }
        false
    }

    fn render(&mut self, _rows: usize, cols: usize) {
        let visible = self.build_rows();
        let pad = " ".repeat(LEFT_PAD);
        let mut out = String::new();
        for _ in 0..TOP_PAD {
            out.push_str("\r\n");
        }
        if visible.is_empty() {
            out.push_str(&format!("{}\u{1b}[2m(no tabs)\u{1b}[0m", pad));
            print!("{}", out);
            return;
        }
        for (i, row) in visible.iter().enumerate() {
            let core = match row {
                Row::Group { name, collapsed, count, attention } => {
                    let disc = if *collapsed { "▶" } else { "▼" };
                    let editing = match &self.renaming {
                        Some((RenameTarget::Group(o), buf)) if o == name => Some(buf),
                        _ => None,
                    };
                    if let Some(buf) = editing {
                        format!("{} {}▏", disc, buf)
                    } else {
                        let icon = if *collapsed { self.icon(*attention) } else { String::new() };
                        format!("{} {}{} ({})", disc, icon, name, count)
                    }
                }
                Row::Tab { position, label, active, attention } => {
                    let dot = if *active { "●" } else { " " };
                    let editing = match &self.renaming {
                        Some((RenameTarget::Tab(p), buf)) if p == position => Some(buf),
                        _ => None,
                    };
                    if let Some(buf) = editing {
                        format!("  {} {}▏", dot, buf)
                    } else {
                        format!("  {} {}{}", dot, self.icon(*attention), label)
                    }
                }
                Row::Pane { title, focused, last, .. } => {
                    let branch = if *last { "└" } else { "├" };
                    if *focused {
                        format!("    {} {}", branch, title)
                    } else {
                        format!("    \u{1b}[2m{} {}\u{1b}[22m", branch, title)
                    }
                }
            };
            let line = truncate_visible(&format!("{}{}", pad, core), cols);
            if i == self.selected {
                let w = visible_width(&line);
                let bar = if w < cols {
                    format!("{}{}", line, " ".repeat(cols - w))
                } else {
                    line
                };
                out.push_str(&format!("\u{1b}[7m{}\u{1b}[0m", bar));
            } else {
                out.push_str(&line);
            }
            out.push_str("\r\n");
        }
        print!("{}", out);
    }
}

/// Pane title for the 28-column sidebar: drop the shell's `user@host: ` prefix
/// (the default bash title), leaving the cwd — `likemusic@PC: ~/a/b` -> `…/b`.
fn short_title(title: &str) -> String {
    if let Some((head, rest)) = title.split_once(": ") {
        if head.contains('@') && !head.contains(char::is_whitespace) {
            // the meaningful end of a path survives the narrow column: ~/a/b/c -> …/c
            return match rest.rsplit_once('/') {
                Some((dir, last)) if !last.is_empty() && dir.contains('/') => format!("…/{}", last),
                _ => rest.to_string(),
            };
        }
    }
    title.to_string()
}

/// Tab name from a pane title: without Claude's status glyph (`✳ wsa`, `⠐ wsa`)
/// and the `…/` of a shortened path.
fn tab_name_from_pane(title: &str) -> String {
    let t = title.trim_start_matches(|c: char| {
        !(c.is_alphanumeric() || c == '~' || c == '/' || c == '.' || c == '…' || c == '_' || c == '-')
    });
    let t = t.strip_prefix("…/").unwrap_or(t).trim();
    // a command line (held pane before a restore, `Waiting to run: …`): its program
    // name — `/home/u/.local/bin/claude-zellij --zellij-marker …` -> `claude-zellij`
    match t.split_once(char::is_whitespace) {
        Some((cmd, _)) if cmd.starts_with('/') => cmd.rsplit('/').next().unwrap_or(cmd).to_string(),
        _ => t.to_string(),
    }
}

/// Number of *visible* characters, ignoring ANSI CSI escape sequences (`ESC [ … letter`).
fn visible_width(s: &str) -> usize {
    let mut w = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // consume the escape sequence up to and including its final letter
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            w += 1;
        }
    }
    w
}

/// Truncate to `max` *visible* columns, preserving ANSI escapes intact (never cut
/// mid-sequence) and appending `…` when content is dropped.
fn truncate_visible(s: &str, max: usize) -> String {
    if visible_width(s) <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            out.push(c);
            for e in chars.by_ref() {
                out.push(e);
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            if w >= keep {
                break;
            }
            out.push(c);
            w += 1;
        }
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_attention_variants() {
        assert_eq!(parse_attention("work:api ⏳"), (Some(Attention::Waiting), "work:api"));
        assert_eq!(parse_attention("db ✅"), (Some(Attention::Completed), "db"));
        assert_eq!(parse_attention("run ⚙"), (Some(Attention::Working), "run"));
        assert_eq!(parse_attention("plain"), (None, "plain"));
    }

    #[test]
    fn markers_roundtrip_for_all_states() {
        for att in [Attention::Waiting, Attention::Working, Attention::Completed] {
            let name = format!("x:y{}", marker_of(att));
            assert_eq!(parse_attention(&name), (Some(att), "x:y"));
        }
    }

    #[test]
    fn parse_attention_roundtrips_with_set() {
        let (_, base) = parse_attention("x:y ⏳");
        assert_eq!(format!("{}{}", base, MARK_WAITING), "x:y ⏳");
    }

    #[test]
    fn group_of_splits_and_defaults() {
        let s = State { separator: ':', ..Default::default() };
        assert_eq!(s.group_of("work:api"), ("work".to_string(), "api"));
        assert_eq!(s.group_of("work: api"), ("work".to_string(), "api")); // trims one space
        assert_eq!(s.group_of("scratch"), ("General".to_string(), "scratch"));
    }

    #[test]
    fn merge_priority_waiting_working_completed() {
        assert_eq!(merge(Some(Attention::Completed), Attention::Waiting), Attention::Waiting);
        assert_eq!(merge(Some(Attention::Waiting), Attention::Completed), Attention::Waiting);
        assert_eq!(merge(Some(Attention::Working), Attention::Waiting), Attention::Waiting);
        assert_eq!(merge(Some(Attention::Completed), Attention::Working), Attention::Working);
        assert_eq!(merge(Some(Attention::Working), Attention::Completed), Attention::Working);
        assert_eq!(merge(None, Attention::Completed), Attention::Completed);
        assert_eq!(merge(None, Attention::Working), Attention::Working);
    }

    #[test]
    fn state_roundtrips_through_serialize_and_parse() {
        let p = Persisted {
            order: vec!["work".to_string(), "General".to_string()],
            collapsed: ["scratch".to_string()].into(),
            tab_order: [(
                "work".to_string(),
                vec!["db".to_string(), "api".to_string()],
            )]
            .into(),
        };
        assert_eq!(parse_state(&serialize_state(&p)), p);
        assert_eq!(parse_state(""), Persisted::default());
        assert_eq!(parse_state("junk line\n"), Persisted::default());
    }

    #[test]
    fn sort_by_saved_orders_known_labels_then_native() {
        let item = |label: &str, position: usize| TabItem {
            position,
            label: label.to_string(),
            active: false,
            attention: None,
        };
        let mut items = vec![item("a", 0), item("b", 1), item("c", 2), item("b", 3)];
        sort_by_saved(&mut items, &["c".to_string(), "b".to_string()]);
        let got: Vec<(usize, &str)> =
            items.iter().map(|it| (it.position, it.label.as_str())).collect();
        // saved first (c, then both b's in stable native order), unseen (a) after
        assert_eq!(got, vec![(2, "c"), (1, "b"), (3, "b"), (0, "a")]);
    }

    #[test]
    fn merge_order_saved_first_then_new_dropping_stale() {
        let saved = vec!["b".to_string(), "gone".to_string(), "a".to_string()];
        let appearance = vec!["a".to_string(), "b".to_string(), "new".to_string()];
        assert_eq!(merge_order(&saved, appearance), vec!["b", "a", "new"]);
        assert_eq!(merge_order(&[], vec!["x".to_string()]), vec!["x"]);
    }

    #[test]
    fn move_in_swaps_neighbors_and_respects_edges() {
        let mut order: Vec<String> =
            vec!["a".into(), "b".into(), "c".into()];
        assert!(move_in(&mut order, "b", 1));
        assert_eq!(order, vec!["a", "c", "b"]);
        assert!(!move_in(&mut order, "a", -1)); // already first
        assert!(!move_in(&mut order, "b", 1)); // already last
        assert!(!move_in(&mut order, "missing", 1));
        assert_eq!(order, vec!["a", "c", "b"]); // edges/missing leave order untouched
    }

    fn facts(cwd: &str, toplevel: &str, common: &str, branch: &str) -> CwdFacts {
        CwdFacts {
            cwd: cwd.into(),
            toplevel: toplevel.into(),
            common: common.into(),
            branch: branch.into(),
        }
    }

    #[test]
    fn parse_facts_reads_kv_lines_and_ignores_junk() {
        let f = parse_facts("cwd=/a/b\ntoplevel=/a/b\ncommon=/a/b/.git\nbranch=main\nx\n");
        assert_eq!(f, facts("/a/b", "/a/b", "/a/b/.git", "main"));
        assert_eq!(parse_facts(""), CwdFacts::default());
    }

    #[test]
    fn default_tab_names_detected() {
        assert!(is_default_tab_name("Tab #1"));
        assert!(is_default_tab_name("Tab #42"));
        assert!(!is_default_tab_name("Tab #"));
        assert!(!is_default_tab_name("Tab #1x"));
        assert!(!is_default_tab_name("work:api"));
    }

    #[test]
    fn autogroup_rules_parse_in_numeric_order() {
        let cfg: BTreeMap<String, String> = [
            ("autogroup_10".to_string(), "/b/** -> bee".to_string()),
            ("autogroup_2".to_string(), "/a/** -> ay".to_string()),
            ("autogroup_default".to_string(), "repo".to_string()),
            ("autogroup_x".to_string(), "/junk/** -> nope".to_string()),
            ("separator".to_string(), ":".to_string()),
        ]
        .into();
        assert_eq!(
            parse_autogroup_rules(&cfg),
            vec![
                ("/a/**".to_string(), "ay".to_string()),
                ("/b/**".to_string(), "bee".to_string()),
            ]
        );
    }

    #[test]
    fn derive_repo_mode_names() {
        use AutoDefault::*;
        // at repo root: group = repo, label = branch
        assert_eq!(
            derive_auto_name(Repo, &[], &facts("/c/app", "/c/app", "/c/app/.git", "main")),
            Some("app:main".to_string())
        );
        // in a subdir: label = dir basename
        assert_eq!(
            derive_auto_name(Repo, &[], &facts("/c/app/src", "/c/app", "/c/app/.git", "main")),
            Some("app:src".to_string())
        );
        // linked worktree: group = owning repo, label = worktree dir
        assert_eq!(
            derive_auto_name(
                Repo,
                &[],
                &facts(
                    "/c/app/.claude/worktrees/fix-auth",
                    "/c/app/.claude/worktrees/fix-auth",
                    "/c/app/.git",
                    "fix-auth"
                )
            ),
            Some("app:fix-auth".to_string())
        );
        // not a repo: leave alone
        assert_eq!(derive_auto_name(Repo, &[], &facts("/tmp/x", "", "", "")), None);
    }

    #[test]
    fn derive_rule_dir_and_off_modes() {
        use AutoDefault::*;
        let rules = vec![("/w/**".to_string(), "work".to_string())];
        // rule wins over default, exact base dir matches too
        assert_eq!(
            derive_auto_name(Off, &rules, &facts("/w/app", "/w/app", "/w/app/.git", "dev")),
            Some("work:dev".to_string())
        );
        assert_eq!(
            derive_auto_name(Repo, &rules, &facts("/w", "", "", "")),
            Some("work:w".to_string())
        );
        // dir mode: plain basename, ungrouped
        assert_eq!(
            derive_auto_name(Dir, &[], &facts("/tmp/scratch", "", "", "")),
            Some("scratch".to_string())
        );
        // off + no match: leave alone
        assert_eq!(derive_auto_name(Off, &[], &facts("/tmp/x", "", "", "")), None);
        assert_eq!(derive_auto_name(Off, &[], &CwdFacts::default()), None);
    }

    #[test]
    fn parse_spinner_splits_chars_with_default_fallback() {
        assert_eq!(parse_spinner(Some(&"◐◓◑◒".to_string())), vec!["◐", "◓", "◑", "◒"]);
        let default: Vec<String> = SPINNER_DEFAULT.chars().map(String::from).collect();
        assert_eq!(parse_spinner(None), default);
        assert_eq!(parse_spinner(Some(&"  ".to_string())), default); // whitespace-only
    }

    #[test]
    fn migrate_group_state_moves_all_entries() {
        let mut s = State {
            group_order: vec!["work".to_string(), "misc".to_string()],
            collapsed: ["work".to_string()].into(),
            tab_order: [("work".to_string(), vec!["api".to_string()])].into(),
            ..Default::default()
        };
        s.migrate_group_state("work", "proj");
        assert_eq!(s.group_order, vec!["proj", "misc"]);
        assert!(s.collapsed.contains("proj") && !s.collapsed.contains("work"));
        assert_eq!(s.tab_order.get("proj"), Some(&vec!["api".to_string()]));
        assert!(!s.tab_order.contains_key("work"));
        // renaming into an existing group must not duplicate it in the order
        s.migrate_group_state("proj", "misc");
        assert_eq!(s.group_order, vec!["misc"]);
    }

    #[test]
    fn derive_never_produces_empty_names() {
        use AutoDefault::*;
        assert_eq!(derive_auto_name(Repo, &[], &facts("", "", "", "")), None);
        assert_eq!(derive_auto_name(Dir, &[], &facts("", "", "", "")), None);
        let rules = vec![("/**".to_string(), "g".to_string())];
        // basename of "" (parse_facts trims "/" to "") — no "g:" ghost tab
        assert_eq!(derive_auto_name(Off, &rules, &facts("", "", "", "")), None);
    }

    #[test]
    fn rename_label_first_match_only() {
        let mut order = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        rename_label(&mut order, "a", "z");
        assert_eq!(order, vec!["z", "b", "a"]);
        rename_label(&mut order, "missing", "x");
        assert_eq!(order, vec!["z", "b", "a"]);
    }

    #[test]
    fn visible_width_ignores_ansi() {
        // ◆, space, x  => 3 visible; the color codes count for nothing
        assert_eq!(visible_width("\u{1b}[33m◆\u{1b}[39m x"), 3);
        assert_eq!(visible_width("plain"), 5);
    }

    #[test]
    fn truncate_visible_keeps_escapes_and_width() {
        let s = "\u{1b}[33m◆\u{1b}[39m hello"; // visible "◆ hello" = 7
        let out = truncate_visible(s, 4);
        assert_eq!(visible_width(&out), 4); // 3 kept + …
        assert!(out.contains("\u{1b}[33m")); // escape preserved, not sliced
        assert_eq!(truncate_visible("abc", 5), "abc"); // no-op when it fits
    }
}
