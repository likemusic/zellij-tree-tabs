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
const MARK_WAITING: &str = "⏳ ";
const MARK_COMPLETED: &str = "✅ ";
/// Ongoing-work state (Claude is running): unlike the attention marks above it
/// is NOT cleared on focus — it ends when a waiting/completed/clear pipe arrives.
const MARK_WORKING: &str = "⚙ ";

/// Default sidebar animation frames for `MARK_WORKING` (the tab name carries
/// only the static marker; the spinner lives purely in the render). Dense
/// braille (7 of 8 dots lit) fills the cell evenly, unlike the sparse ⠋⠙⠹ set
/// which sits visibly high-and-left next to ◆/✓. Override with the `spinner`
/// config key (each char = one frame; width-1 glyphs only).
const SPINNER_DEFAULT: &str = "⣾⣽⣻⢿⡿⣟⣯⣷";
/// 2 frames a second (local change, was 0.15 s): each frame is a full render
/// of the pane, and at ~7 a second the sidebar and Zellij's screen thread took
/// a quarter of a core.
const SPINNER_INTERVAL: f64 = 0.5;
/// Pane titles set by programs (OSC, e.g. Claude Code) don't trigger a
/// PaneUpdate, so the visible sidebar re-reads them this often (seconds).
const TITLE_INTERVAL: f64 = 2.0;
/// title ticks to wait for a resize to land before counting it lost
const RESIZE_WAIT_TICKS: u8 = 2;

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

/// Directory group rules kept outside the layout (local change): `glob ->
/// group` lines, `#` comments — the same as the `dirgroup_N` config keys, which
/// come first. A file because a changed plugin config block is a different
/// plugin to Zellij (Alt+w would launch a new one), and because another tool
/// (task-hub's project tree) can write it. Re-read with the state file.
const GROUPS_FILE: &str = "/cache/vtabs-groups";

/// Worktrees are listed every this many title refreshes (2 s each), and when
/// the sidebar is shown.
const WORKTREE_TICKS: u32 = 15;

/// Unread panes (`pane_unread`), space-separated ids, one file per session.
const UNREAD_FILE_PREFIX: &str = "/cache/vtabs-unread-";
/// Worktree lists the visible instance last got from git, shared with the
/// others: an instance shown with none drew the tree without its worktree
/// rows, they popped in a moment later and moved the rows under the mouse.
const PIN_FILE_PREFIX: &str = "/cache/vtabs-pin-";
const WORKTREES_FILE_PREFIX: &str = "/cache/vtabs-worktrees-";

fn parse_groups_file(s: &str) -> Vec<(String, String)> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (pattern, group) = l.split_once(" -> ")?;
            Some((pattern.trim().to_string(), group.trim().to_string()))
        })
        .collect()
}


/// One line in the Zellij log (`zellij.log`), prefixed `VTABS` so a watcher
/// can follow clicks and hand-overs and tell when one went nowhere.
macro_rules! vlog {
    ($($arg:tt)*) => { eprintln!("VTABS {}", format!($($arg)*)) };
}
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

/// Key of a folded tab in `collapsed`: group paths never hold a ':' (it
/// separates the group from the label in a tab name).
fn tab_fold_key(label: &str) -> String {
    format!("tab:{}", label)
}

/// ▼/▶ before a tab's label when it has rows of its own (see `Row::Tab`);
/// two blanks keep the labels of the others aligned.
fn fold_mark(fold: Option<bool>) -> &'static str {
    match fold {
        Some(true) => "▶ ",
        Some(false) => "▼ ",
        None => "  ",
    }
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

/// `<prefix>N "glob -> group"` config keys (`autogroup_`, `dirgroup_`), in
/// numeric order.
fn parse_glob_rules(configuration: &BTreeMap<String, String>, prefix: &str) -> Vec<(String, String)> {
    let mut numbered: Vec<(u32, (String, String))> = configuration
        .iter()
        .filter_map(|(k, v)| {
            let n: u32 = k.strip_prefix(prefix)?.parse().ok()?;
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

/// Where a `glob -> group` rule puts a tab working in `cwd` (local change):
/// the group's `/`-separated path, and how many of its segments were named —
/// a group ending in `*` continues with the directories below the rule's base
/// (`~/Projects/** -> *`: `a/b` for `~/Projects/a/b`), which `fold_paths`
/// later drops where they would hold a single tab.
fn dir_group_path(rules: &[(String, String)], cwd: &str) -> Option<(Vec<String>, usize)> {
    let cwd = cwd.trim_end_matches('/');
    rules.iter().find_map(|(pattern, group)| {
        if !(cwd == pattern.trim_end_matches("/**") || glob_match::glob_match(pattern, cwd)) {
            return None;
        }
        let (named, auto) = match group.strip_suffix('*') {
            Some(p) => (p, true),
            None => (group.as_str(), false),
        };
        let mut segs = path_segments(named, '/');
        let fixed = segs.len();
        if auto {
            let base = &pattern[..pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len())];
            if let Some(rest) = cwd.strip_prefix(base.trim_end_matches('/')) {
                segs.extend(path_segments(rest, '/'));
            }
        }
        Some((segs, fixed))
    })
}

/// `git worktree list --porcelain`: (path, branch) per worktree, main first;
/// bare and prunable entries left out, a detached one has an empty branch.
fn parse_worktrees(out: &str) -> Vec<(String, String)> {
    out.split("\n\n")
        .filter_map(|block| {
            let mut path = None;
            let mut branch = String::new();
            for line in block.lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    path = Some(p.to_string());
                } else if let Some(b) = line.strip_prefix("branch ") {
                    branch = b.strip_prefix("refs/heads/").unwrap_or(b).to_string();
                } else if line == "bare" || line.starts_with("prunable") {
                    return None;
                }
            }
            path.map(|p| (p, branch))
        })
        .collect()
}

fn path_segments(s: &str, sep: char) -> Vec<String> {
    s.split(sep).map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
}

/// Final group path of each tab: directory segments (past the first `fixed`)
/// are dropped from the end while the group they end in would hold only this
/// tab — a project folder with one tab is not worth a level of its own.
fn fold_paths(raw: &[(Vec<String>, usize)]) -> Vec<Vec<String>> {
    raw.iter()
        .map(|(segs, fixed)| {
            let mut n = segs.len();
            while n > *fixed
                && raw.iter().filter(|(o, _)| o.len() >= n && o[..n] == segs[..n]).count() == 1
            {
                n -= 1;
            }
            segs[..n].to_vec()
        })
        .collect()
}

fn parent_path(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

fn leaf_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Every group path with its ancestors, in first-appearance order of `paths`
/// (tabs by position).
fn appearance_paths<'a>(paths: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        let mut end = 0;
        for seg in p.split('/') {
            end += seg.len();
            let prefix = p[..end].to_string();
            if !out.contains(&prefix) {
                out.push(prefix);
            }
            end += 1;
        }
    }
    out
}

/// A group row of the tree, in display order (depth first).
#[derive(Debug, PartialEq)]
struct GroupNode {
    /// the group's path: collapse and tab-order key
    path: String,
    /// shown name: a chain of groups that hold no tabs and one subgroup each
    /// is one row, `a/b`
    label: String,
    /// top of that chain — the node that is ordered among its siblings
    top: String,
    depth: usize,
}

/// The group tree of `paths` (each tab's group path, empty = none) as rows,
/// siblings in `order` (paths not in it last).
fn group_tree(paths: &[String], order: &[String]) -> Vec<GroupNode> {
    let all = appearance_paths(paths.iter());
    let rank = |p: &String| order.iter().position(|o| o == p).unwrap_or(usize::MAX);
    // parent path -> children paths, sibling order
    let mut children: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
    for p in &all {
        children.entry(parent_path(p)).or_default().push(p);
    }
    for c in children.values_mut() {
        c.sort_by_key(|p| rank(p));
    }
    let mut out = Vec::new();
    // depth-first, explicit stack: (top of a row, depth), next sibling last
    let mut stack: Vec<(&String, usize)> =
        children.get("").into_iter().flatten().rev().map(|p| (*p, 0)).collect();
    while let Some((top, depth)) = stack.pop() {
        let mut cur = top;
        let mut label = leaf_name(top).to_string();
        while let Some([only]) = children.get(cur.as_str()).map(Vec::as_slice) {
            if paths.contains(cur) {
                break;
            }
            cur = only;
            label = format!("{}/{}", label, leaf_name(cur));
        }
        out.push(GroupNode { path: cur.clone(), label, top: top.clone(), depth });
        if let Some(kids) = children.get(cur.as_str()) {
            stack.extend(kids.iter().rev().map(|k| (*k, depth + 1)));
        }
    }
    out
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
    /// `view status` / `view projects` (local change); None: not saved yet
    status_view: Option<bool>,
    /// `width N` / `width_saved N` (local change): sidebar width in columns, and
    /// the width to restore when a collapsed sidebar is expanded
    width: Option<usize>,
    width_saved: Option<usize>,
}

/// One line per entry: `order <group>` / `collapsed <group>` /
/// `taborder <group>\t<label>` (labels of one group in order, one per line) /
/// `view status|projects`.
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
        } else if let Some(v) = line.strip_prefix("view ") {
            p.status_view = Some(v == "status");
        } else if let Some(v) = line.strip_prefix("width ") {
            p.width = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("width_saved ") {
            p.width_saved = v.parse().ok();
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
    if let Some(v) = p.status_view {
        out.push_str(if v { "view status\n" } else { "view projects\n" });
    }
    if let Some(w) = p.width {
        out.push_str(&format!("width {}\n", w));
    }
    if let Some(w) = p.width_saved {
        out.push_str(&format!("width_saved {}\n", w));
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

/// Split the attention marker off a tab name: "⏳ work:api" -> (Waiting, "work:api").
/// Markers are prefixes (local change): the state reads first, before the name.
/// A pane title that is the shell's own `user@host: path` (bash's default
/// xterm title): the shell is at its prompt, nothing runs in the pane.
fn is_shell_prompt_title(title: &str) -> bool {
    title
        .split_once(": ")
        .is_some_and(|(who, _)| !who.is_empty() && !who.contains(' ') && who.contains('@'))
}

fn parse_attention(name: &str) -> (Option<Attention>, &str) {
    if let Some(base) = name.strip_prefix(MARK_WAITING) {
        (Some(Attention::Waiting), base)
    } else if let Some(base) = name.strip_prefix(MARK_COMPLETED) {
        (Some(Attention::Completed), base)
    } else if let Some(base) = name.strip_prefix(MARK_WORKING) {
        (Some(Attention::Working), base)
    // upstream's suffix form, left on tab names by sessions started before the change
    } else if let Some(base) = name.strip_suffix(" ⏳") {
        (Some(Attention::Waiting), base)
    } else if let Some(base) = name.strip_suffix(" ✅") {
        (Some(Attention::Completed), base)
    } else if let Some(base) = name.strip_suffix(" ⚙") {
        (Some(Attention::Working), base)
    } else {
        (None, name)
    }
}

/// Status-view group headers, in display order (local change).
const STATUS_GROUPS: [&str; 4] = ["Waiting", "Working", "Done", "Idle"];

/// Below this width — or when collapsed — the sidebar draws only state icons
/// (local change).
const NARROW_COLS: usize = 8;

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
    /// background-work column icons (local change, `shell_icon` / `monitor_icon`)
    shell_icon: String,
    monitor_icon: String,
    autogroup_default: AutoDefault,
    autogroup_rules: Vec<(String, String)>,
    /// active inline rename edit: (target, input buffer)
    renaming: Option<(RenameTarget, String)>,
    /// `m` on a tab (local change): (tab id, tab name) waiting for a digit that
    /// picks the session to move it to; `sessions` are the other running ones
    moving: Option<(usize, String)>,
    sessions: Vec<String>,
    /// `move_tab_command` config key: the script that does the move
    move_command: String,
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
    /// keyboard mode was handed over and this tab is being switched to: take
    /// focus on the TabUpdate that shows it — taken earlier, Zellij gives it back
    /// to the tab's last pane when the switch lands (local change)
    focus_on_show: bool,
    /// tab position -> plugin id of the vtabs instance in that tab (from PaneUpdate),
    /// to hand keyboard mode over when browsing switches the visible tab
    sidebar_by_tab: BTreeMap<usize, u32>,
    /// tab where keyboard mode started — Esc goes back there
    keys_origin: Option<usize>,
    /// tab position -> its tiled terminal panes (id, title, focused) in screen order
    tab_panes: BTreeMap<usize, Vec<(u32, String, bool)>>,
    /// per-pane state from the same pipes as the tab markers (local change), kept
    /// in memory: every instance gets every pipe, so each holds the full map
    pane_attention: BTreeMap<u32, Attention>,
    /// panes whose session finished its turn (or asked) while you were
    /// elsewhere (local change), until you focus them — Zellij's `[!]`, kept
    /// per pane: their ✓ is drawn bright, a read one dim. Kept by the visible
    /// instance (it knows the focus from PaneUpdate, no host call) in
    /// UNREAD_FILE_PREFIX, read back by an instance when it is shown.
    pane_unread: BTreeSet<u32>,
    /// focused terminal pane of this instance's tab (from PaneUpdate)
    my_focused: Option<u32>,
    /// tab whose row was clicked: its row stays selected (not its focused
    /// pane's) until the focus moves to another pane — shared through
    /// PIN_FILE_PREFIX with the instance of that tab, which shows next
    pin_tab: Option<usize>,
    /// `git worktree list` per repo (local change): main worktree path -> its
    /// other worktrees (path, branch), and which repo each tab cwd is in
    worktrees: BTreeMap<String, Vec<(String, String)>>,
    cwd_repo: BTreeMap<String, String>,
    /// title refreshes since the last worktree listing
    wt_ticks: u32,
    /// tab_group_paths' last result and the hash of what it was computed from
    group_paths_cache: std::cell::RefCell<Option<(u64, BTreeMap<usize, String>)>>,
    /// per-pane background work of its Claude session (local change, from
    /// vtabs-bg.sh): shell commands and monitors still running, so a finished
    /// turn that waits on them reads apart from an idle one
    pane_background: BTreeMap<u32, (u16, u16)>,
    /// terminal panes collapsed in a stack (Zellij reports them as suppressed)
    stacked_collapsed: BTreeSet<u32>,
    /// a collapsed stacked pane being unfolded by a focus round trip: its own
    /// focus may show up in a PaneUpdate after the sidebar got it back
    expanding: Option<u32>,
    /// this instance's tab is not the shown one (Event::Visible): it neither
    /// animates nor refreshes titles — ten sidebars spinning at 0.15 s kept
    /// the server's plugin threads busy for one picture on screen
    hidden: bool,
    /// status view (local change, `v` key / `default_view` config key): tabs
    /// grouped by attention state instead of by name prefix; saved in the state
    /// file so every tab's instance shows the same view
    status_view: bool,
    /// sidebar width (local change, `-` `=` `0` keys): a target this instance
    /// resizes its own pane toward, cleared once reached. Each tab keeps its
    /// own width (the user's choice): Zellij resizes in steps of 5% of the tab
    /// and a hidden tab's resize is dropped, so a shared width jumped on every
    /// tab switch, and one set with the mouse was out of the others' reach.
    width: Option<usize>,
    width_saved: Option<usize>,
    /// own pane's columns, the full tab width, and the terminal pane right of
    /// the sidebar (from PaneUpdate)
    my_cols: usize,
    full_cols: usize,
    neighbor: Option<u32>,
    /// last resize issued: (columns before, increase, ticks waited); observed step size;
    /// columns and direction in which a resize had no effect (not retried)
    last_resize: Option<(usize, bool, u8)>,
    resize_step: usize,
    /// own width as of the last tick (see sync_width)
    cols_settled: usize,
    width_stuck: Option<(usize, bool)>,
    /// (tab id, name) of leftover marks already stripped: PaneUpdates keep
    /// coming before the TabUpdate with the new name, don't rename again
    stale_renamed: Vec<(usize, String)>,
    /// resizes in a row that had no effect — one sent while a swap layout is
    /// being applied gets lost, so give up only after a few
    resize_misses: u8,
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
    /// `dirgroup_N "glob -> group"` (local change): a tab whose name carries no
    /// `group:` prefix joins the group of the first rule its working directory
    /// matches — projects group by where they live, whatever the tab is called
    dir_groups: Vec<(String, String)>,
    dir_groups_config: Vec<(String, String)>,
    /// working directory per tab position: of its first pane that has one
    /// (refreshed with the titles, by the visible instance)
    tab_cwd: BTreeMap<usize, String>,
    /// brain/screen split (local change, see `Role`)
    role: Role,
    /// a screen's brain; a screen without one works on its own (legacy)
    brain_id: Option<u32>,
    /// a screen's last frame from the brain
    frame_text: String,
    /// brain: every sidebar pane's (rows, cols), and the last frame sent to each
    screen_size: BTreeMap<u32, (usize, usize)>,
    sent_frames: BTreeMap<u32, String>,
    /// brain: ids of tabs that have had a terminal, and those found empty since
    /// (closed on the next timer tick unless a terminal shows up)
    tabs_with_terminal: BTreeSet<usize>,
    tabs_to_close: BTreeSet<usize>,
    my_rows: usize,
    /// this instance's own plugin id (a brain's `my_id` is the active screen's)
    self_id: u32,
}

/// Local change. One instance of this plugin per tab (the layout's sidebar
/// pane) used to hold all the state, but Zellij sends tab and pane updates only
/// to the plugins of the active tab: the others went stale and had to be synced
/// through files and hand-overs. Now one more instance, loaded headless with
/// `role "brain"` (`load_plugins` in config.kdl), gets every update, keeps the
/// state, and draws the frame each sidebar pane (a screen) shows; screens send
/// it their keys and clicks. A screen with no brain works on its own (Legacy).
#[derive(Default, PartialEq, Clone, Copy, Debug)]
enum Role {
    #[default]
    Legacy,
    Brain,
    Screen,
}

/// A key as text for the screen -> brain pipe: modifiers, then `u<code point in hex>` or the key's name.
fn encode_key(key: &KeyWithModifier) -> String {
    let mut parts: Vec<String> = key.key_modifiers.iter().map(|m| format!("{:?}", m)).collect();
    parts.push(match key.bare_key {
        BareKey::Char(c) => format!("u{:x}", c as u32),
        BareKey::F(n) => format!("f{}", n),
        other => format!("{:?}", other),
    });
    parts.join(" ")
}

fn decode_key(s: &str) -> Option<KeyWithModifier> {
    let mut parts: Vec<&str> = s.split(' ').collect();
    let bare = parts.pop()?;
    let bare_key = if let Some(c) = bare.strip_prefix('u').and_then(|h| u32::from_str_radix(h, 16).ok()).and_then(char::from_u32) {
        BareKey::Char(c)
    } else {
        bare.parse::<BareKey>().ok()?
    };
    let mut key_modifiers = BTreeSet::new();
    for m in parts {
        key_modifiers.insert(match m {
            "Shift" => KeyModifier::Shift,
            "Alt" => KeyModifier::Alt,
            "Ctrl" => KeyModifier::Ctrl,
            "Super" => KeyModifier::Super,
            "" => continue,
            _ => return None,
        });
    }
    Some(KeyWithModifier { bare_key, key_modifiers })
}

fn encode_mouse(m: &Mouse) -> Option<String> {
    Some(match m {
        Mouse::LeftClick(r, c) => format!("L {} {}", r, c),
        Mouse::ScrollUp(n) => format!("U {}", n),
        Mouse::ScrollDown(n) => format!("D {}", n),
        _ => return None,
    })
}

fn decode_mouse(s: &str) -> Option<Mouse> {
    let f: Vec<&str> = s.split(' ').collect();
    Some(match f.as_slice() {
        ["L", r, c] => Mouse::LeftClick(r.parse().ok()?, c.parse().ok()?),
        ["U", n] => Mouse::ScrollUp(n.parse().ok()?),
        ["D", n] => Mouse::ScrollDown(n.parse().ok()?),
        _ => return None,
    })
}

/// A sidebar pane: this plugin in a pane (a brain has none).
fn is_sidebar(p: &PaneInfo) -> bool {
    p.is_plugin && p.plugin_url.as_deref().is_some_and(|u| u.ends_with("zellij-vtabs.wasm"))
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
    /// `name`: the group's path (`a/b`, collapse and order key); `label`: what is
    /// shown (a chain of single-subgroup groups is one row); `depth`: nesting
    Group { name: String, label: String, depth: usize, collapsed: bool, count: usize, attention: Option<Attention>, unread: bool },
    /// `fold`: None without panes or worktrees listed under it, else whether they are folded
    Tab { position: usize, label: String, depth: usize, active: bool, attention: Option<Attention>, unread: bool, fold: Option<bool> },
    /// a terminal pane of the tab above, listed when the tab has 2+ panes;
    /// keyboard mode browses onto it (frame highlighted), Enter/click focuses it
    /// `flat`: a session row of the status view — no tree branch, tab-row indent
    Pane { id: u32, title: String, depth: usize, focused: bool, last: bool, flat: bool, attention: Option<Attention>, unread: bool },
    /// a worktree of a repo shown above with no tab or session in it (local
    /// change); Enter/click opens a tab there
    Worktree { path: String, label: String, depth: usize },
}

impl State {
    /// Group path (`/`-joined) and label from a tab name: everything before the
    /// last separator is the group, nested at each separator (`a:b:label` is
    /// `label` in `a/b`). No separator: General.
    fn group_of<'a>(&self, name: &'a str) -> (String, &'a str) {
        match name.rfind(self.separator) {
            Some(i) => {
                let segs = path_segments(&name[..i], self.separator);
                let label = name[i + self.separator.len_utf8()..].trim_start();
                (if segs.is_empty() { "General".to_string() } else { segs.join("/") }, label)
            }
            None => ("General".to_string(), name),
        }
    }

    /// Group path of every tab, by position (local change): the name's prefix,
    /// else the `dirgroup_N` rule its working directory matches (directory
    /// levels with a single tab folded away), else General.
    fn tab_group_paths(&self) -> BTreeMap<usize, String> {
        // memoized (local change): every render rebuilds the rows, ~7 times a
        // second while a spinner turns, and the plugin runs interpreted
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for t in &self.tabs {
            (t.position, &t.name).hash(&mut h);
        }
        self.tab_cwd.hash(&mut h);
        self.dir_groups.hash(&mut h);
        self.separator.hash(&mut h);
        let key = h.finish();
        if let Some((k, paths)) = &*self.group_paths_cache.borrow() {
            if *k == key {
                return paths.clone();
            }
        }
        let paths = self.compute_tab_group_paths();
        *self.group_paths_cache.borrow_mut() = Some((key, paths.clone()));
        paths
    }

    fn compute_tab_group_paths(&self) -> BTreeMap<usize, String> {
        let raw: Vec<(usize, (Vec<String>, usize))> = self
            .tabs
            .iter()
            .map(|t| {
                let base = parse_attention(&t.name).1;
                let path = match base.rfind(self.separator) {
                    Some(i) => {
                        let segs = path_segments(&base[..i], self.separator);
                        let n = segs.len();
                        (segs, n)
                    }
                    None => self
                        .tab_cwd
                        .get(&t.position)
                        .and_then(|cwd| dir_group_path(&self.dir_groups, cwd))
                        .unwrap_or_default(),
                };
                (t.position, path)
            })
            .collect();
        let folded = fold_paths(&raw.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>());
        raw.iter()
            .zip(folded)
            .map(|((pos, _), segs)| {
                (*pos, if segs.is_empty() { "General".to_string() } else { segs.join("/") })
            })
            .collect()
    }

    /// A tab's group path and its label (the name without a group prefix).
    fn group_of_tab<'a>(&self, position: usize, base: &'a str) -> (String, &'a str) {
        let label = self.group_of(base).1;
        let path = self.tab_group_paths().remove(&position).unwrap_or_else(|| "General".to_string());
        (path, label)
    }

    /// Group paths, with their ancestors, in first-appearance (tab position) order.
    fn appearance_groups(&self) -> Vec<String> {
        let paths = self.tab_group_paths();
        let mut tabs: Vec<&TabInfo> = self.tabs.iter().collect();
        tabs.sort_by_key(|t| t.position);
        appearance_paths(tabs.iter().filter_map(|t| paths.get(&t.position)))
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
        let mut groups = self.dir_groups_config.clone();
        if let Ok(s) = std::fs::read_to_string(GROUPS_FILE) {
            groups.extend(parse_groups_file(&s));
        }
        self.dir_groups = groups;
        let Some(path) = self.state_file() else {
            return;
        };
        if let Ok(s) = std::fs::read_to_string(path) {
            let p = parse_state(&s);
            self.group_order = p.order;
            self.collapsed = p.collapsed;
            self.tab_order = p.tab_order;
            if let Some(v) = p.status_view {
                self.status_view = v;
            }
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
            status_view: Some(self.status_view),
            // per tab now: an old shared width is dropped on the next save
            width: None,
            width_saved: None,
        };
        let _ = std::fs::write(path, serialize_state(&p));
    }

    /// Groups in display order (the tree, depth first), each with its own tabs
    /// in display order; groups holding only subgroups are left out.
    fn grouped_items(&self) -> Vec<(String, Vec<TabItem>)> {
        let paths = self.tab_group_paths();
        let mut groups: BTreeMap<String, Vec<TabItem>> = BTreeMap::new();
        for t in &self.tabs {
            let (attention, base) = parse_attention(&t.name);
            let label = self.group_of(base).1;
            let g = paths.get(&t.position).cloned().unwrap_or_else(|| "General".to_string());
            groups.entry(g).or_default().push(TabItem {
                position: t.position,
                label: label.to_string(),
                active: t.active,
                attention: self.pane_rollup(t.position).or(attention),
            });
        }
        let names: Vec<String> = groups.keys().cloned().collect();
        group_tree(&names, &self.display_group_order())
            .into_iter()
            .filter_map(|n| {
                let mut items = groups.remove(&n.path)?;
                if let Some(saved) = self.tab_order.get(&n.path) {
                    sort_by_saved(&mut items, saved);
                }
                Some((n.path, items))
            })
            .collect()
    }

    /// Tab state from its panes' marks (local change): a tab holding several
    /// Claude sessions shows the most urgent of them, not the last signal. None
    /// when no pane is marked — the tab name's marker is used then.
    fn pane_rollup(&self, position: usize) -> Option<Attention> {
        self.tab_panes.get(&position)?.iter().fold(None, |acc, (id, _, _)| {
            match self.pane_attention.get(id) {
                Some(&x) => Some(merge(acc, x)),
                None => acc,
            }
        })
    }

    /// Status view (local change): every session — each terminal pane — under
    /// its state, most urgent state first, in project display order. A pane is
    /// labelled with its title and, dimmed, its tab; a one-pane tab with the
    /// tab's full name, whose marker also stands in for an unmarked pane.
    fn status_rows(&self) -> Vec<Row> {
        let mut buckets: [Vec<Row>; 4] = Default::default();
        for (g, items) in self.grouped_items() {
            for it in items {
                let Some(t) = self.tabs.iter().find(|t| t.position == it.position) else {
                    continue;
                };
                let base = parse_attention(&t.name).1;
                let tab_label = if base.contains(self.separator) {
                    format!("{}{}{}", g, self.separator, it.label)
                } else {
                    it.label.clone()
                };
                let Some(panes) = self.tab_panes.get(&it.position) else { continue };
                let tab_mark = parse_attention(&t.name).0;
                for (id, title, focused) in panes {
                    let (title, attention) = if panes.len() == 1 {
                        (tab_label.clone(), self.pane_attention.get(id).copied().or(tab_mark))
                    } else {
                        let title = strip_claude_glyph(title);
                        (
                            format!("{}\u{1b}[2m · {}\u{1b}[22m", title, tab_label),
                            self.pane_attention.get(id).copied(),
                        )
                    };
                    let b = match attention {
                        Some(Attention::Waiting) => 0,
                        Some(Attention::Working) => 1,
                        Some(Attention::Completed) => 2,
                        None => 3,
                    };
                    buckets[b].push(Row::Pane {
                        id: *id,
                        title,
                        depth: 0,
                        unread: self.pane_unread.contains(id),
                        focused: *focused && it.active,
                        last: false,
                        flat: true,
                        attention,
                    });
                }
            }
        }
        let mut rows = Vec::new();
        for (name, items) in STATUS_GROUPS.iter().zip(buckets) {
            if items.is_empty() {
                continue;
            }
            let collapsed = self.collapsed.contains(*name);
            let attention = items.iter().fold(None, |acc, r| match r {
                Row::Pane { attention: Some(x), .. } => Some(merge(acc, *x)),
                _ => acc,
            });
            let unread = items.iter().any(|r| matches!(r, Row::Pane { unread: true, .. }));
            rows.push(Row::Group {
                name: name.to_string(),
                label: name.to_string(),
                depth: 0,
                collapsed,
                count: items.len(),
                attention,
                unread,
            });
            if !collapsed {
                rows.extend(items);
            }
        }
        rows
    }

    fn build_rows(&self) -> Vec<Row> {
        if self.status_view {
            return self.status_rows();
        }
        let mut items: BTreeMap<String, Vec<TabItem>> = self.grouped_items().into_iter().collect();
        let names: Vec<String> = items.keys().cloned().collect();
        let mut rows = Vec::new();
        // depth of a collapsed group whose subtree is being skipped
        let mut skip_below: Option<usize> = None;
        for node in group_tree(&names, &self.display_group_order()) {
            if skip_below.is_some_and(|d| node.depth > d) {
                continue;
            }
            skip_below = None;
            let in_subtree = |g: &String| *g == node.path || g.starts_with(&format!("{}/", node.path));
            let (count, group_att) = items
                .iter()
                .filter(|(g, _)| in_subtree(g))
                .flat_map(|(_, its)| its)
                .fold((0, None), |(n, acc), it| {
                    (n + 1, it.attention.map_or(acc, |x| Some(merge(acc, x))))
                });
            let collapsed = self.collapsed.contains(&node.path);
            let unread = items
                .iter()
                .filter(|(g, _)| in_subtree(g))
                .flat_map(|(_, its)| its)
                .any(|it| self.any_unread(self.tab_panes.get(&it.position).into_iter().flatten().map(|(id, _, _)| id)));
            rows.push(Row::Group {
                name: node.path.clone(),
                label: node.label,
                depth: node.depth,
                collapsed,
                count,
                attention: group_att,
                unread,
            });
            if collapsed {
                skip_below = Some(node.depth);
                continue;
            }
            // a repo's worktree list goes under the first tab in its main worktree
            let mut repos_shown: Vec<String> = Vec::new();
            for it in items.remove(&node.path).unwrap_or_default() {
                let panes = self.tab_panes.get(&it.position);
                let listed = panes.filter(|p| p.len() > 1 || self.show_single_pane && !p.is_empty());
                let main = self.tab_cwd.get(&it.position).and_then(|c| self.cwd_repo.get(c).filter(|m| *m == c));
                let main = main.filter(|m| !repos_shown.contains(m) && !self.closed_worktrees(m).is_empty()).cloned();
                let folded = self.collapsed.contains(&tab_fold_key(&it.label));
                rows.push(Row::Tab {
                    position: it.position,
                    label: it.label,
                    depth: node.depth,
                    active: it.active,
                    attention: it.attention,
                    unread: self.any_unread(panes.into_iter().flatten().map(|(id, _, _)| id)),
                    fold: (listed.is_some() || main.is_some()).then_some(folded),
                });
                if let Some(m) = &main {
                    repos_shown.push(m.clone());
                }
                if folded {
                    continue;
                }
                if let Some(panes) = listed {
                    for (i, (id, title, focused)) in panes.iter().enumerate() {
                        rows.push(Row::Pane {
                            id: *id,
                            title: title.clone(),
                            depth: node.depth,
                            focused: *focused,
                            last: i + 1 == panes.len(),
                            flat: false,
                            attention: self.pane_attention.get(id).copied(),
                            unread: self.pane_unread.contains(id),
                        });
                    }
                }
                if let Some(main) = main {
                    let before = rows.len();
                    // depth + 2 puts the header's ▶ right of the panes' └ (see render)
                    self.push_worktree_rows(&mut rows, &node.path, &main, node.depth + 2);
                    // the header continues the tab's pane tree: its last pane is no longer last
                    if rows.len() > before {
                        if let Some(Row::Pane { last, .. }) = rows[..before].last_mut() {
                            *last = false;
                        }
                    }
                }
            }
        }
        rows
    }

    /// The folded `⎇ worktrees (N)` row of repo `main` and, unfolded, its
    /// worktrees that nothing is open in — under the tab working in `main`.
    fn push_worktree_rows(&self, rows: &mut Vec<Row>, group: &str, main: &str, depth: usize) {
        let closed = self.closed_worktrees(main);
        if closed.is_empty() {
            return;
        }
        // folded unless opened: the key is in `collapsed` while open
        let name = format!("{}/⎇{}", group, basename(main));
        let open = self.collapsed.contains(&name);
        rows.push(Row::Group {
            name,
            label: "⎇ worktrees".to_string(),
            depth,
            collapsed: !open,
            count: closed.len(),
            attention: None,
            unread: false,
        });
        if open {
            for (path, branch) in closed {
                let dir = basename(&path).to_string();
                let label = if branch.is_empty() || branch == dir { dir } else { format!("{} ({})", dir, branch) };
                rows.push(Row::Worktree { path, label, depth });
            }
        }
    }

    /// Row of what has focus: the focused pane's row when the active tab lists
    /// its panes (local change: the bar used to jump back to the tab row after
    /// Enter on a pane), else the active tab's row.
    fn active_row_index(&self) -> Option<usize> {
        let rows = self.build_rows();
        if self.status_view {
            // focused is set only for the active tab's focused pane there
            return rows.iter().position(|r| matches!(r, Row::Pane { focused: true, .. }));
        }
        let tab = rows.iter().position(|r| matches!(r, Row::Tab { active: true, .. }))?;
        if matches!(rows[tab], Row::Tab { position, .. } if Some(position) == self.pin_tab) {
            return Some(tab);
        }
        let pane = rows[tab + 1..]
            .iter()
            .take_while(|r| matches!(r, Row::Pane { .. }))
            .position(|r| matches!(r, Row::Pane { focused: true, .. }));
        Some(pane.map_or(tab, |i| tab + 1 + i))
    }

    /// A finished session's ✓ (local change): bright and bold while unread,
    /// dim once you have been in the pane.
    fn icon(&self, att: Option<Attention>, unread: bool) -> String {
        match att {
            Some(Attention::Completed) if unread => {
                format!("\u{1b}[1;92m{}\u{1b}[22;39m ", self.completed_icon)
            }
            Some(Attention::Completed) => format!("\u{1b}[2m\u{1b}[32m{}\u{1b}[39m\u{1b}[22m ", self.completed_icon),
            Some(Attention::Waiting) => format!("\u{1b}[33m{}\u{1b}[39m ", self.waiting_icon),
            Some(Attention::Working) => {
                let frame = self
                    .spinner
                    .get(self.spin % self.spinner.len().max(1))
                    .map(String::as_str)
                    .unwrap_or("⣾");
                format!("\u{1b}[36m{}\u{1b}[39m ", frame)
            }
            None => String::new(),
        }
    }

    /// The icon in a fixed 2-column slot (local change), so names start in the
    /// same column whether or not a row has a state icon.
    fn icon_slot(&self, att: Option<Attention>, unread: bool) -> String {
        if att.is_some() { self.icon(att, unread) } else { "  ".to_string() }
    }

    /// Background work of panes (local change): shells and monitors summed.
    fn background_of<'a>(&self, panes: impl IntoIterator<Item = &'a u32>) -> (u16, u16) {
        panes.into_iter().fold((0, 0), |(s, m), id| {
            let (a, b) = self.pane_background.get(id).copied().unwrap_or((0, 0));
            (s + a, m + b)
        })
    }

    /// The background-work column (local change): one cell for shell commands,
    /// one for monitors, right after the state icon. Always reserved: a column
    /// that came and went with the work shifted every name sideways.
    fn background_slot(&self, bg: (u16, u16)) -> String {
        let cell = |n: u16, icon: &str, color: u8| {
            if n == 0 {
                " ".to_string()
            } else {
                format!("\u{1b}[{}m{}\u{1b}[39m", color, icon)
            }
        };
        format!("{}{}", cell(bg.0, &self.shell_icon, 35), cell(bg.1, &self.monitor_icon, 34))
    }

    /// State icon and background cells in one fixed 4-column slot (local change).
    fn state_slot(&self, att: Option<Attention>, unread: bool, bg: (u16, u16)) -> String {
        let glyph = if att.is_some() { self.icon(att, unread).trim_end().to_string() } else { " ".to_string() };
        format!("{}{} ", glyph, self.background_slot(bg))
    }

    /// Background work of a tab row: the sum over its panes.
    fn tab_background(&self, position: usize) -> (u16, u16) {
        let panes = self.tab_panes.get(&position);
        self.background_of(panes.into_iter().flatten().map(|(id, _, _)| id))
    }

    /// `background` pipe (payload "<shells> <monitors>"). True if it changed.
    fn set_pane_background(&mut self, pane_id: u32, payload: &str) -> bool {
        let mut it = payload.split_whitespace().map(|n| n.parse::<u16>().unwrap_or(0));
        let bg = (it.next().unwrap_or(0), it.next().unwrap_or(0));
        let old = self.pane_background.get(&pane_id).copied();
        if bg == (0, 0) {
            self.pane_background.remove(&pane_id);
        } else {
            self.pane_background.insert(pane_id, bg);
        }
        self.pane_background.get(&pane_id).copied() != old
    }

    /// Whether the spinner must turn: a working pane mark counts too (local
    /// change). The tab name carries only the last signal of its panes, so a tab
    /// whose one pane finished while the others still work read as done, and
    /// with no `⚙` left in any name every spinner froze.
    fn any_working(&self) -> bool {
        self.pane_attention.iter().any(|(id, a)| *a == Attention::Working && self.pane_tab.contains_key(id))
            || self.tabs.iter().any(|t| parse_attention(&t.name).0 == Some(Attention::Working))
    }

    /// Arm the animation timer if something is working and none is scheduled.
    fn ensure_timer(&mut self) {
        if !self.timer_running {
            self.timer_interval =
                if !self.hidden && self.any_working() { SPINNER_INTERVAL } else { TITLE_INTERVAL };
            set_timeout(self.timer_interval);
            self.timer_running = true;
        }
    }

    /// Re-read pane titles if this instance is the visible one; true if any changed.
    fn refresh_titles(&mut self) -> bool {
        if self.hidden {
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
        changed |= self.refresh_tab_cwds();
        if self.wt_ticks == 0 {
            self.list_worktrees();
        }
        self.wt_ticks = (self.wt_ticks + 1) % WORKTREE_TICKS;
        self.auto_name_tabs();
        changed
    }

    /// Working directory of each tab (for `dirgroup_N`): its first pane that
    /// reports one — a held pane of a restored session may not. True if any
    /// changed. Skipped without rules: one host call per tab every refresh.
    fn refresh_tab_cwds(&mut self) -> bool {
        let mut changed = false;
        for (position, panes) in &self.tab_panes {
            let cwd = panes.iter().find_map(|(id, _, _)| {
                get_pane_cwd(PaneId::Terminal(*id)).ok().map(|p| p.to_string_lossy().into_owned())
            });
            if let Some(cwd) = cwd {
                if self.tab_cwd.get(position) != Some(&cwd) {
                    vlog!("tab cwd my_id={} tab={} cwd={} group={:?}", self.my_id, position, cwd, dir_group_path(&self.dir_groups, &cwd));
                    self.tab_cwd.insert(*position, cwd);
                    changed = true;
                }
            }
        }
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
            rename_tab_with_id(tab_id as u64, format!("{}{}", marker, want));
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
    ///
    /// Two instances do it (local change): the visible one and the one in the
    /// pane's own tab — the query is a blocking host call, and all ~10
    /// instances making it on every signal slowed the server (CLI pipes timed
    /// out). Two still cover a pipe that one of them misses.
    fn fresh_tab(&self, pane_id: u32) -> Option<TabInfo> {
        let tab_pos = *self.pane_tab.get(&pane_id)?;
        if self.hidden && self.my_tab != Some(tab_pos) {
            return None;
        }
        let tab_id = self.tabs.iter().find(|t| t.position == tab_pos)?.tab_id;
        get_tab_info(tab_id)
    }

    /// Add an attention marker to the tab containing `pane_id` (global rename).
    fn set_attention(&self, pane_id: u32, attention: Attention) {
        let Some(t) = self.fresh_tab(pane_id) else {
            return;
        };
        let (att, base) = parse_attention(&t.name);
        // The tab you're looking at is marked too (local change): the mark is
        // the session's state, not an unread cue — only the next signal ends it.
        if att == Some(attention) {
            return;
        }
        rename_tab_with_id(t.tab_id as u64, format!("{}{}", marker_of(attention), base));
    }

    /// Mark the tab containing `pane_id` as working. Unlike attention marks
    /// this applies to the active tab too (it is not cleared on focus, so
    /// there is no set/clear race — only pipes ever end it).
    fn set_working(&self, pane_id: u32) {
        if let Some(t) = self.fresh_tab(pane_id) {
            let (att, base) = parse_attention(&t.name);
            if att != Some(Attention::Working) {
                rename_tab_with_id(t.tab_id as u64, format!("{}{}", MARK_WORKING, base));
            }
        }
    }

    /// Same signal, per pane (local change): the pane's session state, kept
    /// until the next signal — looking at the pane does not clear it. True if
    /// it changed.
    fn set_pane_attention(&mut self, pane_id: u32, signal: &str) -> bool {
        let old = self.pane_attention.get(&pane_id).copied();
        let new = match signal {
            "working" => Some(Attention::Working),
            "waiting" => Some(Attention::Waiting),
            "completed" => Some(Attention::Completed),
            // the session ended: no state left to show
            "clear-working" => None,
            _ => return false,
        };
        match new {
            Some(a) => self.pane_attention.insert(pane_id, a),
            None => self.pane_attention.remove(&pane_id),
        };
        new != old
    }

    /// The pane you are in is read (local change): drop its unread mark. Only
    /// the visible instance knows where you are; it saves the change for the
    /// others. True if it changed.
    fn mark_focused_read(&mut self) -> bool {
        if self.hidden {
            return false;
        }
        let read = self.my_focused.is_some_and(|f| self.pane_unread.remove(&f));
        if read {
            self.save_unread();
        }
        read
    }

    /// One line per tab cwd ("R cwd main") and per worktree ("W main path branch"), tab-separated.
    fn save_worktrees(&self) {
        let Some(session) = &self.session else { return };
        let mut out = String::new();
        for (cwd, main) in &self.cwd_repo {
            out.push_str(&format!("R\t{}\t{}\n", cwd, main));
        }
        for (main, list) in &self.worktrees {
            for (path, branch) in list {
                out.push_str(&format!("W\t{}\t{}\t{}\n", main, path, branch));
            }
        }
        let _ = std::fs::write(format!("{}{}", WORKTREES_FILE_PREFIX, session), out);
    }

    fn load_worktrees(&mut self) {
        let Some(session) = &self.session else { return };
        let Ok(s) = std::fs::read_to_string(format!("{}{}", WORKTREES_FILE_PREFIX, session)) else { return };
        let (mut repos, mut lists): (BTreeMap<String, String>, BTreeMap<String, Vec<(String, String)>>) = Default::default();
        for line in s.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            match f.as_slice() {
                ["R", cwd, main] => {
                    repos.insert(cwd.to_string(), main.to_string());
                    lists.entry(main.to_string()).or_default();
                }
                ["W", main, path, branch] => lists.entry(main.to_string()).or_default().push((path.to_string(), branch.to_string())),
                _ => {}
            }
        }
        self.cwd_repo = repos;
        self.worktrees = lists;
    }

    fn unread_file(&self) -> Option<String> {
        self.session.as_ref().map(|s| format!("{}{}", UNREAD_FILE_PREFIX, s))
    }

    fn save_unread(&self) {
        if self.hidden {
            return;
        }
        if let Some(path) = self.unread_file() {
            let ids: Vec<String> = self.pane_unread.iter().map(u32::to_string).collect();
            let _ = std::fs::write(path, ids.join(" "));
        }
    }

    fn load_unread(&mut self) {
        if let Some(s) = self.unread_file().and_then(|p| std::fs::read_to_string(p).ok()) {
            self.pane_unread = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        }
    }

    /// Whether any of `panes` is unread.
    fn any_unread<'a>(&self, panes: impl IntoIterator<Item = &'a u32>) -> bool {
        panes.into_iter().any(|id| self.pane_unread.contains(id))
    }

    /// Forget the marks of a closed pane (local change: on PaneClosed, not on
    /// every PaneUpdate — a pane briefly missing from one, e.g. while a stack
    /// expands, lost its mark until the next signal). True if anything changed.
    fn forget_pane(&mut self, id: u32) -> bool {
        let a = self.pane_attention.remove(&id).is_some();
        let b = self.pane_background.remove(&id).is_some();
        let c = self.pane_unread.remove(&id);
        if c {
            self.save_unread();
        }
        a || b || c
    }

    /// The session ended (SessionEnd): strip whatever marker the tab has.
    fn clear_working(&self, pane_id: u32) {
        if let Some(t) = self.fresh_tab(pane_id) {
            let (att, base) = parse_attention(&t.name);
            if att.is_some() {
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
        rename_tab_with_id(t.tab_id as u64, format!("{}{}", marker, name));
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
            Row::Worktree { path, label, .. } => {
                let name = label.split(" (").next().unwrap_or(label).to_string();
                let path = path.clone();
                if self.keys_on {
                    self.keys_off(false);
                }
                new_tab(Some(name.as_str()), Some(path.as_str()));
                true
            }
        }
    }

    /// Move the row under the selection up/down (persisted): a group header
    /// reorders the group, a tab row reorders the tab within its group.
    fn move_selected(&mut self, delta: isize) -> bool {
        // status groups are fixed and derived; order is set in the project view
        if self.status_view {
            return false;
        }
        let rows = self.build_rows();
        match rows.get(self.selected) {
            Some(Row::Group { name, .. }) => {
                let name = name.clone();
                let mut order = self.display_group_order();
                // a chain row moves as its top group, among that group's siblings
                let Some(top) = group_tree(&self.grouped_items().into_iter().map(|(g, _)| g).collect::<Vec<_>>(), &order)
                    .into_iter()
                    .find(|n| n.path == name)
                    .map(|n| n.top)
                else {
                    return false;
                };
                let sibs: Vec<String> =
                    order.iter().filter(|p| parent_path(p) == parent_path(&top)).cloned().collect();
                let Some(i) = sibs.iter().position(|p| *p == top) else { return false };
                let j = i as isize + delta;
                if j < 0 || j as usize >= sibs.len() {
                    return false;
                }
                let a = order.iter().position(|p| *p == top).unwrap();
                let b = order.iter().position(|p| *p == sibs[j as usize]).unwrap();
                order.swap(a, b);
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
            Some(Row::Pane { .. }) | Some(Row::Worktree { .. }) | None => false,
        }
    }

    /// Move `old` group's saved order/collapse/tab-order entries to `new` —
    /// its subgroups' too (`old/x` becomes `new/x`).
    fn migrate_group_state(&mut self, old: &str, new: &str) {
        let moved = |g: &str| -> Option<String> {
            if g == old {
                Some(new.to_string())
            } else {
                g.strip_prefix(old).filter(|r| r.starts_with('/')).map(|r| format!("{}{}", new, r))
            }
        };
        for g in self.group_order.iter_mut() {
            if let Some(n) = moved(g) {
                *g = n;
            }
        }
        // renaming into an existing group must not leave a duplicate entry
        let mut seen = BTreeSet::new();
        self.group_order.retain(|g| seen.insert(g.clone()));
        self.collapsed = std::mem::take(&mut self.collapsed)
            .into_iter()
            .map(|g| moved(&g).unwrap_or(g))
            .collect();
        for (g, order) in std::mem::take(&mut self.tab_order) {
            let key = moved(&g).unwrap_or(g);
            self.tab_order.entry(key).or_insert(order);
        }
    }

    /// Rename group `old` (a path) to `new` (its last segment): re-prefix every
    /// tab in its subtree (preserving attention marks) and migrate the saved
    /// state. A group from a directory rule becomes a named prefix this way.
    fn commit_rename(&mut self, old: &str, new: &str) {
        let new = new.trim();
        if new.is_empty() || new == leaf_name(old) {
            return;
        }
        let parent = parent_path(old);
        let new_path = if parent.is_empty() { new.to_string() } else { format!("{}/{}", parent, new) };
        let paths = self.tab_group_paths();
        for t in &self.tabs {
            let Some(g) = paths.get(&t.position) else { continue };
            let rest = match g.strip_prefix(old) {
                Some(r) if r.is_empty() || r.starts_with('/') => r,
                _ => continue,
            };
            let (att, base) = parse_attention(&t.name);
            let label = self.group_of(base).1;
            let prefix = format!("{}{}", new_path, rest).replace('/', &self.separator.to_string());
            let marker = att.map(marker_of).unwrap_or("");
            rename_tab(
                t.position as u32 + 1,
                format!("{}{}{}{}", marker, prefix, self.separator, label),
            );
        }
        self.migrate_group_state(old, &new_path);
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
        let (g, old_label) = self.group_of_tab(position, base);
        if new_label.is_empty() || new_label == old_label {
            return;
        }
        let marker = att.map(marker_of).unwrap_or("");
        let name = match base.rfind(self.separator) {
            None => new_label.to_string(),
            Some(i) => format!("{}{}{}", &base[..i], self.separator, new_label),
        };
        rename_tab(position as u32 + 1, format!("{}{}", marker, name));
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

    /// Browsing onto a pane collapsed in a stack (local change): its frame can't
    /// be seen, so expand it — focus it, which is what unfolds a stack, and take
    /// the focus straight back so the keys stay in the sidebar.
    fn expand_if_stacked(&mut self, pane: Option<u32>) {
        let Some(id) = pane.filter(|id| self.stacked_collapsed.contains(id)) else { return };
        // the round trip must not read as "focus left the sidebar"
        self.keys_focus_seen = false;
        self.expanding = Some(id);
        focus_terminal_pane(id, false, false);
        focus_plugin_pane(self.my_id, false, false);
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

    /// Make sidebar pane `id` selectable or not: a plugin can do it only to
    /// its own pane, so the brain asks the screen — which then also focuses
    /// itself: a focus sent from here could land before the pane is selectable.
    fn set_sel(&self, id: u32, on: bool) {
        if self.role == Role::Brain {
            pipe_message_to_plugin(
                MessageToPlugin::new("vtabs-cmd")
                    .with_destination_plugin_id(id)
                    .with_payload(if on { "selectable" } else { "pinned" }),
            );
        } else {
            set_selectable(on);
        }
    }

    fn keys_on_here(&mut self, origin: Option<usize>) {
        vlog!("keys_on my_id={} my_tab={:?} origin={:?}", self.my_id, self.my_tab, origin);
        self.keys_on = true;
        self.keys_focus_seen = false;
        self.keys_origin = origin;
        self.set_sel(self.my_id, true);
        focus_plugin_pane(self.my_id, false, false);
    }

    /// Browsing onto another tab shows it right away; keyboard mode moves to
    /// that tab's sidebar instance (only one plugin pane can hold the keys).
    fn browse_to(&mut self, position: usize, pane: Option<u32>) {
        if self.role == Role::Brain {
            // one brain: the keys move with the tab (see `follow_active_screen`);
            // the focus leaving this tab's sidebar is not a click elsewhere
            self.keys_focus_seen = false;
            self.pending_pane = pane;
            if let Some(id) = pane {
                self.highlight(Some(id));
            }
            switch_tab_to(position as u32 + 1);
            return;
        }
        let Some(&target) = self.sidebar_by_tab.get(&position) else {
            return;
        };
        let origin = self.keys_origin;
        self.keys_off(false);
        // payload "origin_tab:pane_id:target_tab", the first two may be empty; the
        // target's own tab goes along — an instance in a tab not shown since a
        // plugin reload may not have had a PaneUpdate to learn it from
        let payload = format!(
            "{}:{}:{}",
            origin.map(|o| o.to_string()).unwrap_or_default(),
            pane.map(|p| p.to_string()).unwrap_or_default(),
            position
        );
        // No switch_tab_to here (local change): the other instance switches to its
        // own tab on take-keys, after it has selected the row. Switching from here
        // showed its old selection first (a flash of the focused pane's row): the
        // pipe arrives after the TabUpdate.
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
                    self.expand_if_stacked(Some(*id));
                }
            }
            _ => self.highlight(None),
        }
    }

    fn keys_off(&mut self, restore_focus: bool) {
        vlog!("keys_off my_id={} my_tab={:?} restore={}", self.my_id, self.my_tab, restore_focus);
        self.keys_on = false;
        self.focus_on_show = false;
        self.keys_focus_seen = false;
        self.expanding = None;
        self.pending_pane = None;
        self.highlight(None);
        if self.pinned {
            self.set_sel(self.my_id, false);
        }
        if restore_focus {
            // the brain's last focus may be in the tab browsing started from:
            // in another tab the tab's own focused pane takes over
            let id = self
                .prev_focus
                .filter(|id| self.role != Role::Brain || self.pane_tab.get(id) == self.my_tab.as_ref())
                .or_else(|| self.my_tab.and_then(|t| self.tab_panes.get(&t)).and_then(|ps| ps.iter().find(|p| p.2).or(ps.first())).map(|p| p.0));
            if let Some(id) = id {
                focus_terminal_pane(id, false, false);
            }
        }
    }

    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        if self.moving.is_some() {
            return self.handle_move_key(key);
        }
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
            // tree keys (local change), as in a file tree; р/д: the same keys on
            // the Russian layout
            BareKey::Left | BareKey::Char('h') | BareKey::Char('р') => self.tree_left(),
            BareKey::Right | BareKey::Char('l') | BareKey::Char('д') => self.tree_right(),
            // Russian layout sends the letter on the same key
            // width keys are the same on both keyboard layouts
            BareKey::Char('-') => self.change_width(false),
            BareKey::Char('=') | BareKey::Char('+') => self.change_width(true),
            BareKey::Char('0') => self.toggle_collapse(),
            BareKey::Char('m') | BareKey::Char('ь') => self.start_move(),
            BareKey::Char('v') | BareKey::Char('м') => {
                self.toggle_view();
                true
            }
            BareKey::Char('r') => {
                let rows = self.build_rows();
                match rows.get(self.selected) {
                    Some(Row::Group { .. }) if self.status_view => false,
                    // the worktree list of a repo: nothing to rename
                    Some(Row::Group { name, .. }) if name.contains('⎇') => false,
                    Some(Row::Group { name, .. }) => {
                        self.renaming =
                            Some((RenameTarget::Group(name.clone()), leaf_name(name).to_string()));
                        true
                    }
                    Some(Row::Tab { position, .. }) => {
                        // the label without its group prefix, even in the status view
                        let label = self
                            .grouped_items()
                            .into_iter()
                            .flat_map(|(_, items)| items)
                            .find(|it| it.position == *position)
                            .map(|it| it.label)
                            .unwrap_or_default();
                        self.renaming = Some((RenameTarget::Tab(*position), label));
                        true
                    }
                    Some(Row::Pane { .. }) | Some(Row::Worktree { .. }) | None => false,
                }
            }
            _ => false,
        }
    }

    /// ←: fold an open group; elsewhere go up to the row it belongs to (a pane
    /// to its tab, a tab or folded group to its group).
    fn tree_left(&mut self) -> bool {
        let rows = self.build_rows();
        let parent_depth = match rows.get(self.selected) {
            Some(Row::Group { name, collapsed: false, .. }) => {
                let name = name.clone();
                self.set_folded(&name, true);
                return true;
            }
            Some(Row::Tab { label, fold: Some(false), .. }) => {
                let key = tab_fold_key(label);
                self.set_folded(&key, true);
                return true;
            }
            Some(Row::Group { name, .. }) if name.contains('⎇') => {
                // a worktree list sits under its tab
                let Some(i) = rows[..self.selected].iter().rposition(|r| matches!(r, Row::Tab { .. })) else {
                    return false;
                };
                self.selected = i;
                self.browse_row(&rows, i);
                return true;
            }
            Some(Row::Group { depth: 0, .. }) | None => return false,
            Some(Row::Group { depth, .. }) => Some(depth - 1),
            Some(Row::Tab { depth, .. }) | Some(Row::Worktree { depth, .. }) => Some(*depth),
            Some(Row::Pane { .. }) => None,
        };
        let up = rows[..self.selected].iter().rposition(|r| match (r, parent_depth) {
            (Row::Group { depth, .. }, Some(d)) => *depth == d,
            (Row::Tab { .. }, None) => true,
            _ => false,
        });
        match up {
            Some(i) => {
                self.selected = i;
                self.browse_row(&rows, i);
                true
            }
            None => false,
        }
    }

    /// Fold or unfold a group, a worktree list or a tab (`tab_fold_key`). A
    /// worktree list is folded by default: its key in `collapsed` means open.
    fn set_folded(&mut self, key: &str, folded: bool) {
        if folded != key.contains('⎇') {
            self.collapsed.insert(key.to_string());
        } else {
            self.collapsed.remove(key);
        }
        self.save_state();
    }

    /// →: unfold a folded group or tab; on an open one go to its first row.
    fn tree_right(&mut self) -> bool {
        let rows = self.build_rows();
        match rows.get(self.selected) {
            Some(Row::Group { name, collapsed: true, .. }) => {
                let name = name.clone();
                self.set_folded(&name, false);
                true
            }
            Some(Row::Tab { label, fold: Some(true), .. }) => {
                let key = tab_fold_key(label);
                self.set_folded(&key, false);
                true
            }
            Some(Row::Group { .. }) | Some(Row::Tab { fold: Some(false), .. }) if self.selected + 1 < rows.len() => {
                self.selected += 1;
                self.browse_row(&rows, self.selected);
                true
            }
            _ => false,
        }
    }

    /// Ask git for the worktrees of every tab's repo (answers come as
    /// RunCommandResult). Visible instance only.
    fn list_worktrees(&self) {
        if self.hidden {
            return;
        }
        let cwds: BTreeSet<&String> = self.tab_cwd.values().collect();
        for cwd in cwds {
            let ctx = BTreeMap::from([
                ("vtabs".to_string(), "worktrees".to_string()),
                ("cwd".to_string(), cwd.clone()),
            ]);
            run_command(&["git", "-C", cwd, "worktree", "list", "--porcelain"], ctx);
        }
    }

    /// Worktrees of the repo whose main worktree is `main` that nothing is open
    /// in: no tab works there and no tab or pane (a session) carries its name —
    /// sessions are often started in the main worktree and named after theirs.
    fn closed_worktrees(&self, main: &str) -> Vec<(String, String)> {
        let Some(wts) = self.worktrees.get(main) else { return Vec::new() };
        let mut names: BTreeSet<String> = BTreeSet::new();
        for panes in self.tab_panes.values() {
            for (_, title, _) in panes {
                names.insert(strip_claude_glyph(title).trim().to_string());
            }
        }
        for t in &self.tabs {
            names.insert(self.group_of(parse_attention(&t.name).1).1.trim().to_string());
        }
        wts.iter()
            .filter(|(path, branch)| {
                !self.tab_cwd.values().any(|c| c == path || c.starts_with(&format!("{}/", path)))
                    && !names.contains(basename(path))
                    && (branch.is_empty() || !names.contains(branch))
            })
            .cloned()
            .collect()
    }

    /// Key hints for the footer: while renaming, or in keyboard mode.
    fn key_hints(&self) -> Option<Vec<&'static str>> {
        if self.renaming.is_some() {
            return Some(vec!["Enter save  Esc cancel"]);
        }
        if !self.keys_on {
            return None;
        }
        Some(if self.status_view {
            vec!["↑↓ select  Enter go", "v projects  r rename", "-= width  Esc exit"]
        } else {
            vec![
                "↑↓ select  Enter go",
                "←→ fold  ⇧↑↓ reorder",
                "r rename  m to session",
                "v by state  -= width",
                "Esc exit",
            ]
        })
    }

    /// Pick the tab under the selection (a pane row: its tab) for moving.
    fn start_move(&mut self) -> bool {
        let position = match self.build_rows().get(self.selected) {
            Some(Row::Tab { position, .. }) => Some(*position),
            Some(Row::Pane { id, .. }) => self.pane_tab.get(id).copied(),
            _ => None,
        };
        let Some(t) = position.and_then(|p| self.tabs.iter().find(|t| t.position == p)) else {
            return false;
        };
        self.moving = Some((t.tab_id, parse_attention(&t.name).1.to_string()));
        true
    }

    /// Digit: move the tab to that session; Esc or anything else: cancel.
    fn handle_move_key(&mut self, key: KeyWithModifier) -> bool {
        let Some((tab_id, _)) = self.moving.take() else { return false };
        let BareKey::Char(c) = key.bare_key else { return true };
        let Some(target) = c
            .to_digit(10)
            .and_then(|d| (d as usize).checked_sub(1))
            .and_then(|i| self.sessions.get(i))
        else {
            return true;
        };
        let Some(session) = self.session.clone() else { return true };
        // Detached: the script closes this tab, and this instance with it.
        let line = format!(
            "setsid {} {} {} {} >/tmp/zellij-move-tab.log 2>&1 &",
            self.move_command,
            sh_quote(&session),
            tab_id,
            sh_quote(target)
        );
        run_command(&["sh", "-c", &line], BTreeMap::new());
        self.keys_off(true);
        true
    }

    /// Resize step in columns: observed, else Zellij's 5% of the tab width.
    fn step_cols(&self) -> usize {
        if self.resize_step > 0 {
            self.resize_step
        } else {
            (self.full_cols * 5 / 100).max(1)
        }
    }

    /// One step narrower/wider, in this tab only.
    fn change_width(&mut self, wider: bool) -> bool {
        if self.my_cols == 0 {
            return false;
        }
        let step = self.step_cols();
        let target = if wider {
            self.my_cols + step
        } else {
            self.my_cols.saturating_sub(step).max(1)
        };
        self.width = Some(target);
        self.width_saved = None;
        self.width_stuck = None;
        self.resize_misses = 0;
        self.last_resize = None;
        self.save_state();
        self.sync_width(false);
        true
    }

    /// Collapse to the narrowest width (icons only), or expand back.
    fn toggle_collapse(&mut self) -> bool {
        if let Some(w) = self.width_saved.take() {
            self.width = Some(w);
        } else {
            self.width_saved = Some(self.width.unwrap_or(self.my_cols));
            self.width = Some(1);
        }
        self.width_stuck = None;
        self.resize_misses = 0;
        self.last_resize = None;
        self.save_state();
        self.sync_width(false);
        true
    }

    /// Move own pane one resize step toward `width` if that brings it closer.
    /// Called on every PaneUpdate, which also reports each step's result, and
    /// on the 2 s timer (`tick`). One resize at a time: PaneUpdates arrive before
    /// a resize lands, and resending on each queued up several that then all
    /// landed at once.
    fn sync_width(&mut self, tick: bool) {
        let (Some(target), c) = (self.width, self.my_cols) else { return };
        // a hidden tab's resize is dropped by Zellij and its misses would mark
        // the width stuck for good: each sidebar catches up when it is shown
        if c == 0 || self.hidden {
            return;
        }
        if let Some((before, increase, waited)) = self.last_resize {
            if c != before {
                self.last_resize = None;
                self.resize_misses = 0;
                // a step that took longer than one tick may have landed together
                // with a retry: its size is not one step
                if waited == 0 {
                    self.resize_step = c.abs_diff(before);
                }
            } else if !tick {
                return; // still in flight
            } else if waited < RESIZE_WAIT_TICKS {
                // a busy server applies a resize seconds late: resending now
                // queues a second one and both land together, overshooting
                self.last_resize = Some((before, increase, waited + 1));
                return;
            } else {
                // no effect for several ticks: lost in a relayout, a fixed-size
                // pane, or the minimum width — retry a few times, then give up
                // at this width until it or the target changes
                self.last_resize = None;
                self.resize_misses += 1;
                if self.resize_misses >= 3 {
                    self.width_stuck = Some((c, increase));
                    vlog!("width stuck my_id={} cols={} target={} increase={}", self.my_id, c, target, increase);
                }
            }
        } else if self.width_stuck.is_some_and(|(w, _)| w != c) {
            // the width moved on its own (a late resize, a relayout): not stuck here
            self.width_stuck = None;
            self.resize_misses = 0;
        }
        // act only on a width that held since the last tick: a relayout (focus
        // moving onto the sidebar, a swap layout) passes through widths for a
        // moment, and a resize sent then lands on the settled width
        if tick && c != self.cols_settled {
            self.cols_settled = c;
            return;
        }
        if c != self.cols_settled {
            return;
        }
        let step = self.step_cols();
        let increase = if c < target && (c + step).abs_diff(target) < target - c {
            true
        } else if c > target && c.saturating_sub(step).abs_diff(target) < c - target {
            false
        } else {
            // as close as the resize steps get: done, the border is the user's
            // again (a mouse drag stays)
            self.width = None;
            return;
        };
        if self.width_stuck == Some((c, increase)) {
            self.width = None;
            return;
        }
        let Some(neighbor) = self.neighbor else {
            self.width = None;
            return;
        };
        vlog!("width resize my_id={} cols={} target={} step={} increase={} neighbor={}", self.my_id, c, target, step, increase, neighbor);
        // the neighbour gives up (or takes) columns on its left edge
        let resize = if increase { Resize::Decrease } else { Resize::Increase };
        resize_pane_with_id(
            ResizeStrategy { resize, direction: Some(Direction::Left), invert_on_boundaries: false },
            PaneId::Terminal(neighbor),
        );
        self.last_resize = Some((c, increase, 0));
    }

    /// Switch project/status view, keeping the selection on the same tab or pane.
    fn toggle_view(&mut self) {
        let keep = self.selected_key();
        self.status_view = !self.status_view;
        self.save_state();
        self.reselect(keep);
    }

    /// What the selection is on — (tab position, pane id, group name) — to find
    /// it again after the rows change.
    fn selected_key(&self) -> (Option<usize>, Option<u32>, Option<String>) {
        match self.build_rows().get(self.selected) {
            Some(Row::Tab { position, .. }) => (Some(*position), None, None),
            Some(Row::Pane { id, .. }) => (None, Some(*id), None),
            Some(Row::Group { name, .. }) => (None, None, Some(name.clone())),
            Some(Row::Worktree { path, .. }) => (None, None, Some(path.clone())),
            None => (None, None, None),
        }
    }

    fn reselect(&mut self, (tab, pane, group): (Option<usize>, Option<u32>, Option<String>)) {
        let rows = self.build_rows();
        let idx = rows.iter().position(|r| match r {
            Row::Tab { position, .. } => tab == Some(*position),
            Row::Pane { id, .. } => pane == Some(*id),
            Row::Group { name, .. } => group.as_ref() == Some(name),
            Row::Worktree { path, .. } => group.as_ref() == Some(path),
        });
        self.selected = idx
            .or_else(|| self.active_row_index())
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
    }

    /// A click on a tab or pane row goes there (local change), done by the
    /// clicked instance itself: it is the visible one, so its tab positions are
    /// fresh. Handing the click over to the target tab's instance (keyboard mode
    /// on click) relied on a hidden instance's idea of its own tab, which goes
    /// stale, and clicks onto some tabs did nothing. Zellij moves to the tab of a
    /// focused pane and unfolds its stack; the selection then follows the focus.
    fn click_row(&mut self, rows: &[Row], idx: usize) {
        let target = match &rows[idx] {
            Row::Tab { position, .. } => (Some(*position), None),
            Row::Pane { id, .. } => (self.pane_tab.get(id).copied(), Some(*id)),
            Row::Group { .. } => return,
            Row::Worktree { .. } => {
                self.selected = idx;
                self.activate(idx);
                return;
            }
        };
        vlog!(
            "click my_id={} my_tab={:?} row={} tab={:?} tab_id={:?} pane={:?} keys_on={}",
            self.my_id,
            self.my_tab,
            idx,
            target.0,
            target.0.and_then(|p| self.tabs.iter().find(|t| t.position == p)).map(|t| t.tab_id),
            target.1,
            self.keys_on
        );
        if self.keys_on {
            self.keys_off(false);
        }
        self.pin_tab = target.1.is_none().then_some(target.0).flatten();
        if let Some(session) = &self.session {
            let pin = self.pin_tab.map(|p| p.to_string()).unwrap_or_default();
            let _ = std::fs::write(format!("{}{}", PIN_FILE_PREFIX, session), pin);
        }
        self.selected = idx;
        self.activate(idx);
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
            Mouse::LeftClick(row, col) => {
                let idx = row - TOP_PAD as isize;
                if idx < 0 || idx as usize >= len {
                    return false;
                }
                let idx = idx as usize;
                let rows = self.build_rows();
                if let Row::Group { name, depth, .. } = &rows[idx] {
                    vlog!("group click my_id={} my_tab={:?} row={} col={} group={} keys_on={}", self.my_id, self.my_tab, idx, col, name, self.keys_on);
                    let depth = *depth;
                    // a click on the ▶/▼ folds (local change); on the name it only
                    // selects, and takes the keys so ←/→ and Enter act on it
                    self.selected = idx;
                    if col == LEFT_PAD + 2 * depth {
                        self.activate(idx);
                    } else if !self.keys_on {
                        self.keys_on_here(self.my_tab);
                    }
                } else if let Row::Tab { position, label, depth, attention, unread, fold: Some(folded), .. } = &rows[idx] {
                    // the ▼/▶ before the label folds the tab's panes and worktrees
                    let _ = (position, attention, unread);
                    let mark = LEFT_PAD + 2 * depth + 2;
                    if col == mark || col == mark + 1 && self.width_saved.is_none() {
                        let key = tab_fold_key(label);
                        self.set_folded(&key, !folded);
                    } else {
                        self.click_row(&rows, idx);
                    }
                } else {
                    self.click_row(&rows, idx);
                }
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

/// The plugin's own handling (local change: wrapped by the brain/screen roles
/// in `impl ZellijPlugin` below).
impl State {
    fn load_inner(&mut self, configuration: BTreeMap<String, String>) {
        if configuration.get("role").map(String::as_str) == Some("brain") {
            self.role = Role::Brain;
        }
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
        self.shell_icon = configuration
            .get("shell_icon")
            .cloned()
            .unwrap_or_else(|| "$".to_string());
        self.monitor_icon = configuration
            .get("monitor_icon")
            .cloned()
            .unwrap_or_else(|| "◉".to_string());
        self.autogroup_default = match configuration
            .get("autogroup_default")
            .map(String::as_str)
        {
            Some("repo") => AutoDefault::Repo,
            Some("dir") => AutoDefault::Dir,
            _ => AutoDefault::Off,
        };
        self.autogroup_rules = parse_glob_rules(&configuration, "autogroup_");
        self.dir_groups_config = parse_glob_rules(&configuration, "dirgroup_");
        self.dir_groups = self.dir_groups_config.clone();
        self.spinner = parse_spinner(configuration.get("spinner"));
        self.pinned = configuration.get("pinned").map(String::as_str) != Some("false");
        self.move_command = configuration
            .get("move_tab_command")
            .cloned()
            .unwrap_or_else(|| "zellij-move-tab".to_string());
        self.show_single_pane =
            configuration.get("show_single_pane").map(String::as_str) == Some("true");
        // the state file's saved view, once loaded, wins over this default
        self.status_view =
            configuration.get("default_view").map(String::as_str) == Some("status");
        self.my_id = get_plugin_ids().plugin_id;
        self.self_id = self.my_id;
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::ReadCliPipes,
            // keyboard-mode hand-over between the per-tab instances (vtabs-take-keys)
            PermissionType::MessageAndLaunchOtherPlugins,
            // moving a tab to another session runs the zellij-move-tab script
            PermissionType::RunCommands,
        ]);
        subscribe(&[
            EventType::ModeUpdate,
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::Key,
            EventType::Mouse,
            EventType::Timer,
            EventType::PermissionRequestResult,
            EventType::SessionUpdate,
            EventType::PaneClosed,
            EventType::Visible,
            EventType::RunCommandResult,
        ]);
        self.ensure_timer();
    }

    fn update_inner(&mut self, event: Event) -> bool {
        match event {
            // Not in load(): a non-selectable pane can't take the `y` answer to the
            // permission prompt, so pin only after the grant (cached grants fire it too).
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                if self.role == Role::Brain {
                    // to every plugin: the screens that load before the brain
                    pipe_message_to_plugin(MessageToPlugin::new("vtabs-brain"));
                    return false;
                }
                if self.pinned {
                    set_selectable(false);
                }
                // a brain already running answers with vtabs-brain
                pipe_message_to_plugin(MessageToPlugin::new("vtabs-hello"));
                false
            }
            Event::SessionUpdate(sessions, _) => {
                let others: Vec<String> = sessions
                    .into_iter()
                    .filter(|s| !s.is_current_session)
                    .map(|s| s.name)
                    .collect();
                let changed = others != self.sessions;
                self.sessions = others;
                changed && self.moving.is_some()
            }
            Event::ModeUpdate(mode_info) => {
                if mode_info.session_name != self.session {
                    self.session = mode_info.session_name;
                    self.load_state();
                    if self.role == Role::Brain {
                        self.load_unread();
                        self.load_worktrees();
                    }
                    return true;
                }
                false
            }
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                if self.role == Role::Brain {
                    self.follow_active_screen();
                }
                self.mark_focused_read();
                // Pick up order/collapse changes written by other instances —
                // every tab switch fires a TabUpdate, so the visible sidebar
                // is always freshly synced.
                self.load_state();
                self.sync_width(false);
                if self.focus_on_show
                    && self.tabs.iter().any(|t| t.active && Some(t.position) == self.my_tab)
                {
                    self.focus_on_show = false;
                    self.keys_on_here(self.keys_origin);
                    let h = self.highlighted;
                    self.expand_if_stacked(h);
                }
                // No clear on focus (local change): markers are session state,
                // ended only by the next signal.
                // Keyboard mode: keep a selection already inside this tab — TabUpdates
                // also fire on pane highlight/focus changes, and resetting to the tab
                // row there made browsing bounce back to the first panes.
                let keep = self.keys_on
                    && match self.build_rows().get(self.selected) {
                        Some(Row::Tab { position, .. }) => Some(*position) == self.my_tab,
                        Some(Row::Pane { id, .. }) => self.pane_tab.get(id) == self.my_tab.as_ref(),
                        // a header belongs to no tab: browsing onto it switches nothing
                        Some(Row::Group { .. }) | Some(Row::Worktree { .. }) => true,
                        None => false,
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
            Event::PaneClosed(PaneId::Terminal(id)) => self.forget_pane(id),
            Event::Timer(_) => {
                if !self.tabs_to_close.is_empty() {
                    // positions in a PaneUpdate can predate a closed tab: check
                    // against a fresh snapshot before closing anything
                    let me = get_session_list().ok().and_then(|l| l.live_sessions.into_iter().find(|s| s.is_current_session));
                    for id in std::mem::take(&mut self.tabs_to_close) {
                        let empty = me.as_ref().is_some_and(|me| {
                            me.tabs.iter().find(|t| t.tab_id == id).is_some_and(|t| {
                                me.panes.panes.get(&t.position).is_some_and(|ps| ps.iter().all(|p| p.is_plugin))
                            })
                        });
                        if empty {
                            vlog!("close empty tab id={}", id);
                            close_tab_with_id(id as u64);
                        }
                    }
                }
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
                if !self.hidden && self.any_working() {
                    self.spin = self.spin.wrapping_add(1);
                    redraw = true;
                }
                self.title_elapsed += self.timer_interval;
                if self.title_elapsed >= TITLE_INTERVAL {
                    self.title_elapsed = 0.0;
                    redraw |= self.refresh_titles();
                    // a resize sent during a relayout can get lost: retry from here
                    self.sync_width(true);
                }
                self.ensure_timer();
                redraw
            }
            Event::PaneUpdate(manifest) => {
                if self.role == Role::Brain {
                    self.screen_size = manifest
                        .panes
                        .values()
                        .flatten()
                        // the brain is listed too (as a pane in no layout)
                        .filter(|p| is_sidebar(p) && p.id != self.self_id)
                        .map(|p| (p.id, (p.pane_rows, p.pane_columns)))
                        .collect();
                    self.close_emptied_tabs(&manifest);
                    if let Some(active) = self.tabs.iter().find(|t| t.active).map(|t| t.position) {
                        if let Some(p) = manifest.panes.get(&active).and_then(|ps| ps.iter().find(|p| is_sidebar(p) && p.id != self.self_id)) {
                            self.my_id = p.id;
                        }
                    }
                }
                self.pane_tab.clear();
                let mut me_focused = false;
                let mut focused_terminal = None;
                let my_url = manifest
                    .panes
                    .values()
                    .flatten()
                    .find(|p| p.is_plugin && p.id == self.my_id)
                    .and_then(|p| p.plugin_url.clone());
                self.sidebar_by_tab.clear();
                self.tab_panes.clear();
                self.plugin_tab.clear();
                self.stacked_collapsed = manifest
                    .panes
                    .values()
                    .flatten()
                    .filter(|p| !p.is_plugin && p.is_suppressed)
                    .map(|p| p.id)
                    .collect();
                for (tab_pos, panes) in &manifest.panes {
                    let mut list: Vec<_> = panes
                        .iter()
                        // suppressed too (local change): Zellij 0.45 reports the
                        // collapsed panes of a stack as suppressed
                        .filter(|p| !p.is_plugin && !p.is_floating)
                        .collect();
                    // a stack's panes share their position; id keeps them in stack order
                    list.sort_by_key(|p| (p.pane_y, p.pane_x, p.id));
                    self.tab_panes.insert(
                        *tab_pos,
                        list.iter().map(|p| (p.id, short_title(&p.title), p.is_focused)).collect(),
                    );
                    for p in panes {
                        if !p.is_plugin {
                            self.pane_tab.insert(p.id, *tab_pos);
                            if p.is_focused && !p.is_floating {
                                focused_terminal.get_or_insert((*tab_pos, p.id));
                            }
                        } else {
                            self.plugin_tab.insert(p.id, *tab_pos);
                            if p.id == self.my_id {
                                self.my_tab = Some(*tab_pos);
                                me_focused = p.is_focused;
                                if p.pane_columns != self.my_cols {
                                    vlog!(
                                        "cols my_id={} tab={} {} -> {} x={} floating={} suppressed={} fullscreen={} full={}",
                                        self.my_id, tab_pos, self.my_cols, p.pane_columns, p.pane_x,
                                        p.is_floating, p.is_suppressed, p.is_fullscreen, self.full_cols
                                    );
                                }
                                self.my_cols = p.pane_columns;
                                self.my_rows = p.pane_rows;
                            }
                            if p.plugin_url.as_deref() == Some("zellij:tab-bar") {
                                self.full_cols = p.pane_columns;
                            }
                            if my_url.is_some() && p.plugin_url == my_url && p.id != self.self_id {
                                self.sidebar_by_tab.insert(*tab_pos, p.id);
                            }
                        }
                    }
                }
                // the leftmost tiled terminal right of the sidebar: resizing it on its
                // left edge is what moves the sidebar's border (a resize of the
                // sidebar itself is ignored once a swap layout nests the panes)
                if let Some(ps) = self.my_tab.and_then(|t| manifest.panes.get(&t)) {
                    let me = ps.iter().find(|p| p.is_plugin && p.id == self.my_id);
                    self.neighbor = me.and_then(|me| {
                        ps.iter()
                            .filter(|p| !p.is_plugin && !p.is_floating && !p.is_suppressed && p.pane_x > me.pane_x)
                            .min_by_key(|p| p.pane_x)
                            .map(|p| p.id)
                    });
                }
                // Auto-close: Zellij keeps a tab while any pane is left, and the sidebar
                // is one — so `exit` in the last terminal left a tab with just the
                // sidebar. Each instance closes only its own tab.
                if let Some(my_tab) = self.my_tab.filter(|_| self.role != Role::Brain) {
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
                // A restored session (local change): Zellij brings tab names back
                // with the marks they had, but the sessions behind them are not
                // running — their panes wait for Enter (held), have exited, or are
                // a shell sitting at its prompt.
                // Working/waiting there is a leftover; done (✅) stays, it is
                // still unread. Only the visible instance acts: its tab names are
                // fresh, a hidden one's could rename a tab back.
                if !self.hidden {
                    for (tab_pos, panes) in &manifest.panes {
                        let mut terminals = panes.iter().filter(|p| !p.is_plugin).peekable();
                        if terminals.peek().is_none()
                            || !terminals.all(|p| p.is_held || p.exited || is_shell_prompt_title(&p.title))
                        {
                            continue;
                        }
                        let Some(t) = self.tabs.iter().find(|t| t.position == *tab_pos) else { continue };
                        let (att, base) = parse_attention(&t.name);
                        if matches!(att, Some(Attention::Working | Attention::Waiting))
                            && !self.stale_renamed.iter().any(|(id, n)| *id == t.tab_id && *n == t.name)
                        {
                            vlog!("stale mark tab={} name={:?}", tab_pos, t.name);
                            self.stale_renamed.push((t.tab_id, t.name.clone()));
                            rename_tab_with_id(t.tab_id as u64, base.to_string());
                        }
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
                self.sync_width(false);
                let was_focused = self.my_focused;
                self.my_focused = self.my_tab.and_then(|t| manifest.panes.get(&t)).and_then(|ps| {
                    ps.iter().find(|p| !p.is_plugin && p.is_focused && !p.is_suppressed).map(|p| p.id)
                });
                if !self.hidden && was_focused.is_some() && was_focused != self.my_focused {
                    self.pin_tab = None;
                }
                let read = self.mark_focused_read();
                // Outside keyboard mode the bar follows pane focus (Alt+arrows, clicks,
                // Enter on a pane row) — TabUpdate does not fire for a pane-only change.
                // hidden: rows are rebuilt when shown (Visible), not on every update
                if self.hidden && !self.keys_on {
                    return false;
                }
                if !self.keys_on && self.pending_pane.is_none() {
                    if let Some(i) = self.active_row_index() {
                        if i != self.selected {
                            self.selected = i;
                            return true;
                        }
                    }
                }
                if read && !self.keys_on {
                    return true;
                }
                if self.keys_on {
                    // plugin ids are unique per instance, so the pane alone identifies us
                    let me_focused = me_focused
                        && matches!(get_focused_pane_info(), Ok((_, PaneId::Plugin(id))) if id == self.my_id);
                    let expanding = self.expanding.filter(|id| {
                        focused_terminal.is_some_and(|(t, f)| Some(t) == self.my_tab && f == *id)
                    });
                    if me_focused {
                        self.keys_focus_seen = true;
                        // unfolded and back in the sidebar: the round trip is over
                        if self.expanding.is_some_and(|id| !self.stacked_collapsed.contains(&id)) {
                            self.expanding = None;
                        }
                    } else if expanding.is_some() {
                        // the stacked pane's own focus, arriving late: not a click elsewhere
                    } else if self.keys_focus_seen {
                        // focus left (click elsewhere, Alt+arrow): pin again, keep focus where it went
                        self.keys_off(false);
                        return true;
                    }
                }
                false
            }
            Event::Visible(visible) => {
                self.hidden = !visible;
                self.wt_ticks = 0;
                // Hidden instances stop taking the full pane list and session
                // list (local change): Zellij sends both to every instance on
                // any change — a working Claude animates its pane title — and
                // ~10 hidden sidebars decoding them kept the server busy. Shown
                // again, the subscription brings a fresh PaneUpdate.
                let heavy = [EventType::PaneUpdate, EventType::SessionUpdate];
                if visible {
                    subscribe(&heavy);
                } else {
                    unsubscribe(&heavy);
                }
                // shown again: catch up on titles and the spinner right away
                if visible {
                    // A hidden instance misses TabUpdates, and the new subscription
                    // brings no PaneUpdate until something changes: shown right after
                    // a plugin reload it drew every tab in General, no panes, the
                    // wrong tab active. Take both from the session's own snapshot.
                    if let Some(me) = get_session_list().ok().and_then(|l| l.live_sessions.into_iter().find(|s| s.is_current_session)) {
                        vlog!("visible catch-up my_id={} tabs={}", self.my_id, me.tabs.len());
                        self.update_inner(Event::TabUpdate(me.tabs));
                        self.update_inner(Event::PaneUpdate(me.panes));
                    }
                    self.load_worktrees();
                    // a click on this tab's row in the sidebar shown before (read once)
                    if let Some(file) = self.session.as_ref().map(|s| format!("{}{}", PIN_FILE_PREFIX, s)) {
                        self.pin_tab = std::fs::read_to_string(&file)
                            .ok()
                            .and_then(|p| p.trim().parse().ok())
                            .filter(|p| Some(*p) == self.my_tab);
                        let _ = std::fs::remove_file(&file);
                    }
                    if !self.keys_on && self.pending_pane.is_none() {
                        if let Some(i) = self.active_row_index() {
                            self.selected = i;
                        }
                    }
                    self.refresh_titles();
                    // misses counted while hidden say nothing about this width
                    self.width_stuck = None;
                    self.resize_misses = 0;
                    self.last_resize = None;
                    self.sync_width(false);
                    self.load_unread();
                    self.mark_focused_read();
                }
                self.ensure_timer();
                visible
            }
            Event::RunCommandResult(exit, stdout, _, ctx)
                if ctx.get("vtabs").map(String::as_str) == Some("worktrees") =>
            {
                let Some(cwd) = ctx.get("cwd").cloned() else { return false };
                let list = if exit == Some(0) { parse_worktrees(&String::from_utf8_lossy(&stdout)) } else { Vec::new() };
                let Some(((main, _), rest)) = list.split_first() else {
                    let gone = self.cwd_repo.remove(&cwd).is_some();
                    if gone && !self.hidden {
                        self.save_worktrees();
                    }
                    return gone && !self.hidden;
                };
                let changed = self.cwd_repo.get(&cwd) != Some(main) || self.worktrees.get(main).map(Vec::as_slice) != Some(rest);
                if changed {
                    vlog!("worktrees my_id={} cwd={} main={} others={}", self.my_id, cwd, main, rest.len());
                }
                self.cwd_repo.insert(cwd, main.clone());
                self.worktrees.insert(main.clone(), rest.to_vec());
                if changed && !self.hidden {
                    self.save_worktrees();
                }
                changed && !self.hidden
            }
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            _ => false,
        }
    }

    /// Attention signals: `zellij-vtabs::waiting|completed::<pane_id>` (broadcast CLI pipe).
    fn pipe_inner(&mut self, pipe_message: PipeMessage) -> bool {
        if pipe_message.name == "vtabs-focus" {
            // Every instance (one per tab) gets the message; only the one whose tab
            // holds the client's focused pane acts. Not self.tabs: hidden instances
            // miss TabUpdates, so each believes its own tab is still the active one.
            // Not the returned tab index either: it is the tab *id*, which drifts
            // from the position once a tab is closed. pane_tab is fresh (PaneUpdate).
            let info = get_focused_pane_info();
            let Ok((_, PaneId::Terminal(focused))) = info else {
                return false;
            };
            // an instance that has not learned its tab yet (no PaneUpdate since a
            // reload) also misses the focused pane: None == None took the keys in
            // several tabs at once and showed whichever focused last
            if self.my_tab.is_some()
                && !self.hidden
                && self.pane_tab.get(&focused) == self.my_tab.as_ref()
                && !self.keys_on
            {
                vlog!("focus my_id={} my_tab={:?} focused={}", self.my_id, self.my_tab, focused);
                self.prev_focus = Some(focused);
                // start on what has focus, not where the last keyboard visit ended
                if let Some(i) = self.active_row_index() {
                    self.selected = i;
                }
                self.keys_on_here(self.my_tab);
                return true;
            }
            return false;
        }
        // hand-over from the instance of the previously shown tab (payload: origin tab)
        if pipe_message.name == "vtabs-take-keys" {
            // Taken even when this instance still believes it holds the keys (local
            // change): it may have been left through the tab bar or Alt+arrows while
            // in keyboard mode, and a hidden instance never hears that it lost them.
            // Ignoring the hand-over then made a click on its rows do nothing.
            // a hidden instance may not have read the view switched elsewhere
            self.load_state();
            let mut parts = pipe_message.payload.as_deref().unwrap_or("").split(':');
            let (origin, pane) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            // the sender is the visible instance: its position for this tab is
            // fresh, ours may predate a closed tab (then we switched to the
            // neighbouring tab, the one already shown)
            if let Some(t) = parts.next().and_then(|t| t.parse().ok()) {
                if self.my_tab != Some(t) {
                    vlog!("take-keys my_id={} stale my_tab={:?} -> {}", self.my_id, self.my_tab, t);
                }
                self.my_tab = Some(t);
            }
            // select before switching: this tab shows with the row already chosen
            if let Ok(id) = pane.parse::<u32>() {
                self.pending_pane = Some(id);
                self.select_pending_pane();
            } else if let Some(i) = self.build_rows().iter().position(
                |r| matches!(r, Row::Tab { position, .. } if Some(*position) == self.my_tab),
            ) {
                self.selected = i;
            }
            self.keys_on = true;
            self.keys_focus_seen = false;
            self.keys_origin = origin.parse().ok();
            self.focus_on_show = true;
            if let Some(t) = self.my_tab {
                switch_tab_to(t as u32 + 1);
            }
            return true;
        }
        if let Some(rest) = pipe_message.name.strip_prefix("zellij-vtabs::") {
            let parts: Vec<&str> = rest.split("::").collect();
            if parts.len() == 2 {
                if let Ok(pane_id) = parts[1].parse::<u32>() {
                    let payload = pipe_message.payload.as_deref().unwrap_or("");
                    if parts[0] == "background" {
                        // a CLI pipe with a payload is followed by an empty message
                        // (end of input): not a "0 0"
                        if payload.trim().is_empty() {
                            return false;
                        }
                        // hidden: keep the state, draw nothing
                        return self.set_pane_background(pane_id, payload) && !self.hidden;
                    }
                    if parts[0] == "clear-working" {
                        self.pane_background.remove(&pane_id);
                    }
                    // the status view regroups rows on a state change — keep the
                    // selection on the same row, not the same index
                    let keep = (!self.hidden).then(|| self.selected_key());
                    let mut pane_changed = self.set_pane_attention(pane_id, parts[0]);
                    match parts[0] {
                        "completed" | "waiting" => {
                            // a hidden instance can't tell; it re-reads the visible one's file when shown
                            let looking = !self.hidden && self.my_focused == Some(pane_id);
                            if !looking && self.pane_unread.insert(pane_id) {
                                pane_changed = true;
                                self.save_unread();
                            }
                        }
                        // you answered (the session works again) or it ended
                        "working" | "clear-working" => {
                            if self.pane_unread.remove(&pane_id) {
                                pane_changed = true;
                                self.save_unread();
                            }
                        }
                        // marks restored from outside (after a plugin reload) are not news
                        "read" => {
                            let changed = self.pane_unread.remove(&pane_id);
                            if changed {
                                self.save_unread();
                            }
                            return changed;
                        }
                        _ => {}
                    }
                    if let (true, Some(keep)) = (pane_changed, keep) {
                        self.reselect(keep);
                    }
                    // a hidden instance only keeps the state: nothing to draw
                    let pane_changed = pane_changed && !self.hidden;
                    match parts[0] {
                        "waiting" => {
                            self.set_attention(pane_id, Attention::Waiting);
                            return pane_changed;
                        }
                        "completed" => {
                            self.set_attention(pane_id, Attention::Completed);
                            return pane_changed;
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
                            return pane_changed;
                        }
                        "clear-working" => {
                            self.clear_working(pane_id);
                            return pane_changed;
                        }
                        _ => {}
                    }
                }
            }
        }
        false
    }

    /// Brain: the active tab's sidebar is the one drawn for and given the keys.
    fn follow_active_screen(&mut self) {
        let Some(active) = self.tabs.iter().find(|t| t.active).map(|t| t.position) else { return };
        let old = self.my_id;
        self.my_tab = Some(active);
        if let Some(&id) = self.sidebar_by_tab.get(&active) {
            self.my_id = id;
        }
        if let Some(&(r, c)) = self.screen_size.get(&self.my_id) {
            if c != self.my_cols {
                vlog!("follow tab={} screen={} size={:?} cols {} -> {}", active, self.my_id, self.screen_size, self.my_cols, c);
            }
            self.my_rows = r;
            self.my_cols = c;
        }
        if self.my_id != old && self.keys_on {
            vlog!("keys follow tab={} screen {} -> {}", active, old, self.my_id);
            if self.pinned {
                self.set_sel(old, false);
            }
            self.keys_focus_seen = false;
            self.set_sel(self.my_id, true);
            focus_plugin_pane(self.my_id, false, false);
        }
    }

    /// Brain: Zellij keeps a tab while any pane is left and the sidebar is
    /// one, so `exit` in its last terminal left a tab with just the sidebar.
    /// Such a tab is closed on the next timer tick (see `Event::Timer`).
    fn close_emptied_tabs(&mut self, manifest: &PaneManifest) {
        for t in &self.tabs {
            let terminals = manifest.panes.get(&t.position).map_or(0, |ps| ps.iter().filter(|p| !p.is_plugin).count());
            if terminals > 0 {
                self.tabs_with_terminal.insert(t.tab_id);
                self.tabs_to_close.remove(&t.tab_id);
            } else if self.tabs_with_terminal.remove(&t.tab_id) {
                self.tabs_to_close.insert(t.tab_id);
            }
        }
    }

    /// Brain: send screen `to` its frame, unless it has it already.
    fn push_frame(&mut self, to: u32) {
        let Some(&(rows, cols)) = self.screen_size.get(&to) else { return };
        let text = self.frame(rows, cols);
        if self.sent_frames.get(&to) == Some(&text) {
            return;
        }
        self.sent_frames.insert(to, text.clone());
        pipe_message_to_plugin(MessageToPlugin::new("vtabs-frame").with_destination_plugin_id(to).with_payload(text));
    }

    fn to_brain(&self, name: &str, payload: String) {
        if let Some(brain) = self.brain_id {
            pipe_message_to_plugin(MessageToPlugin::new(name).with_destination_plugin_id(brain).with_payload(payload));
        }
    }

    fn become_screen(&mut self, brain: u32) {
        vlog!("screen my_id={} brain={} (was {:?})", self.my_id, brain, self.role);
        self.brain_id = Some(brain);
        if self.role == Role::Screen {
            return;
        }
        self.role = Role::Screen;
        self.keys_on = false;
        unsubscribe(&[
            EventType::ModeUpdate,
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::Timer,
            EventType::SessionUpdate,
            EventType::PaneClosed,
            EventType::RunCommandResult,
        ]);
        if self.pinned {
            set_selectable(false);
        }
    }

    fn screen_update(&mut self, event: Event) -> bool {
        match event {
            Event::Key(key) => self.to_brain("vtabs-key", encode_key(&key)),
            Event::Mouse(mouse) => {
                if let Some(m) = encode_mouse(&mouse) {
                    self.to_brain("vtabs-mouse", m);
                }
            }
            Event::Visible(visible) => {
                self.hidden = !visible;
                if visible {
                    self.to_brain("vtabs-shown", String::new());
                }
            }
            Event::PermissionRequestResult(_) => return self.update_inner(event),
            _ => {}
        }
        false
    }

    fn brain_pipe(&mut self, msg: PipeMessage) -> bool {
        let from = match msg.source {
            PipeSource::Plugin(id) => Some(id),
            _ => None,
        };
        let payload = msg.payload.clone().unwrap_or_default();
        match msg.name.as_str() {
            "vtabs-hello" | "vtabs-shown" => {
                let Some(id) = from else { return false };
                if msg.name == "vtabs-hello" {
                    pipe_message_to_plugin(MessageToPlugin::new("vtabs-brain").with_destination_plugin_id(id));
                }
                self.sent_frames.remove(&id);
                self.push_frame(id);
            }
            "vtabs-key" => {
                if let Some(key) = decode_key(&payload) {
                    self.handle_key(key);
                }
                self.push_frame(self.my_id);
            }
            "vtabs-mouse" => {
                if let Some(m) = decode_mouse(&payload) {
                    self.handle_mouse(m);
                }
                self.push_frame(self.my_id);
            }
            // Two brains would do everything twice (one from load_plugins, one
            // started by hand): the newer one, with the higher id, leaves, and
            // the older one tells the screens again who their brain is.
            "vtabs-brain" => match from {
                Some(other) if other < self.self_id => {
                    vlog!("brain {} leaves: brain {} runs", self.self_id, other);
                    close_self();
                }
                Some(other) if other > self.self_id => {
                    pipe_message_to_plugin(MessageToPlugin::new("vtabs-brain"));
                }
                _ => {}
            },
            "vtabs-frame" | "vtabs-cmd" => {}
            _ => {
                self.pipe_inner(msg);
                self.push_frame(self.my_id);
            }
        }
        false
    }

    /// Legacy and screen instances.
    fn screen_pipe(&mut self, msg: PipeMessage) -> bool {
        let from = match msg.source {
            PipeSource::Plugin(id) => Some(id),
            _ => None,
        };
        match msg.name.as_str() {
            "vtabs-brain" => {
                if let Some(id) = from.filter(|id| *id != self.my_id) {
                    self.become_screen(id);
                    if !self.hidden {
                        self.to_brain("vtabs-shown", String::new());
                    }
                }
                return false;
            }
            "vtabs-frame" => {
                if self.role == Role::Screen {
                    self.frame_text = msg.payload.unwrap_or_default();
                    return true;
                }
                return false;
            }
            "vtabs-cmd" => {
                match msg.payload.as_deref() {
                    Some("selectable") => {
                        set_selectable(true);
                        focus_plugin_pane(self.my_id, false, false);
                    }
                    Some("pinned") => set_selectable(false),
                    _ => {}
                }
                return false;
            }
            "vtabs-hello" => return false,
            // `zellij pipe --name vtabs-start-brain`: a session started before
            // the brain was in load_plugins gets one from its visible sidebar
            "vtabs-start-brain" => {
                if self.role == Role::Legacy && !self.hidden && self.brain_id.is_none() {
                    vlog!("start brain my_id={}", self.my_id);
                    let config = BTreeMap::from([("role".to_string(), "brain".to_string())]);
                    load_new_plugin("zellij:OWN_URL", config, true, false);
                }
                return false;
            }
            _ => {}
        }
        if self.role == Role::Screen {
            // a keybinding's message (Alt+w) is for the brain; CLI broadcasts reach it directly
            if matches!(msg.source, PipeSource::Keybind) {
                self.to_brain(&msg.name, msg.payload.unwrap_or_default());
            }
            return false;
        }
        self.pipe_inner(msg)
    }

    /// The sidebar's text for a pane of `rows` x `cols`.
    fn frame(&mut self, rows: usize, cols: usize) -> String {
        let visible = self.build_rows();
        let pad = " ".repeat(LEFT_PAD);
        let mut out = String::new();
        for _ in 0..TOP_PAD {
            out.push_str("\r\n");
        }
        if visible.is_empty() {
            out.push_str(&format!("{}\u{1b}[2m(no tabs)\u{1b}[0m", pad));
            return out;
        }
        // collapsed sidebar (local change): only the state icons, one per row
        // the brain's width_saved is the active tab's, not this pane's
        let narrow = self.width_saved.is_some() && self.role != Role::Brain || cols < NARROW_COLS;
        for (i, row) in visible.iter().enumerate() {
            let core = if narrow {
                match row {
                    Row::Group { collapsed: true, attention: Some(a), unread, .. } => self.icon(Some(*a), *unread),
                    Row::Group { collapsed, .. } => (if *collapsed { "▶" } else { "▼" }).to_string(),
                    Row::Tab { attention: Some(a), unread, .. } => self.icon(Some(*a), *unread),
                    Row::Tab { active, .. } => (if *active { "●" } else { "·" }).to_string(),
                    Row::Pane { attention, unread, .. } => format!(" {}", self.icon_slot(*attention, *unread)),
                    Row::Worktree { .. } => "○".to_string(),
                }
            } else {
                match row {
                    Row::Worktree { label, depth, .. } => {
                        format!("{}  \u{1b}[2m○ {}\u{1b}[22m", "  ".repeat(*depth), label)
                    }
                    Row::Group { name, label, depth, collapsed, count, attention, unread } => {
                        let indent = "  ".repeat(*depth);
                        let disc = if *collapsed { "▶" } else { "▼" };
                        let editing = match &self.renaming {
                            Some((RenameTarget::Group(o), buf)) if o == name => Some(buf),
                            _ => None,
                        };
                        if let Some(buf) = editing {
                            format!("{}{} {}▏", indent, disc, buf)
                        } else if name.contains('⎇') {
                            // last branch of the tab's pane tree, ▶/▼ still at 2 * depth
                            let indent = "  ".repeat(depth.saturating_sub(2));
                            format!("{}  \u{1b}[2m└\u{1b}[22m {} {} ({})", indent, disc, label, count)
                        } else {
                            let icon = if *collapsed { self.icon(*attention, *unread) } else { String::new() };
                            format!("{}{} {}{} ({})", indent, disc, icon, label, count)
                        }
                    }
                    Row::Tab { position, label, depth, active, attention, unread, fold } => {
                        // ▼/▶ one level in from the group's, like a subgroup's; the
                        // active tab's ● goes into the left margin (see below)
                        let _ = active;
                        let indent = "  ".repeat(*depth);
                        let editing = match &self.renaming {
                            Some((RenameTarget::Tab(p), buf)) if p == position => Some(buf),
                            _ => None,
                        };
                        if let Some(buf) = editing {
                            format!("{}  {}{}▏", indent, fold_mark(*fold), buf)
                        } else {
                            let slot = self.state_slot(*attention, *unread, self.tab_background(*position));
                            format!("{}  {}{}{}", indent, fold_mark(*fold), slot, label)
                        }
                    }
                    Row::Pane { id, title, depth, focused, last, flat, attention, unread } => {
                        let indent = "  ".repeat(*depth);
                        let branch = if *last { "└" } else { "├" };
                        let icon = self.state_slot(*attention, *unread, self.background_of([id]));
                        let title = strip_claude_glyph(title);
                        if *flat {
                            let dot = if *focused { "●" } else { " " };
                            format!("  {} {}{}", dot, icon, title)
                        } else if *focused {
                            format!("{}  {} {}{}", indent, branch, icon, title)
                        } else {
                            format!("{}  \u{1b}[2m{} \u{1b}[22m{}\u{1b}[2m{}\u{1b}[22m", indent, branch, icon, title)
                        }
                    }
                }
            };
            let pad = match row {
                _ if narrow => "",
                Row::Tab { active: true, .. } => "●",
                _ => pad.as_str(),
            };
            let line = truncate_visible(&format!("{}{}", pad, core), cols);
            if i == self.selected {
                let w = visible_width(&line);
                let bar = if w < cols {
                    format!("{}{}", line, " ".repeat(cols - w))
                } else {
                    line
                };
                // no dim inside the bar: dim on reverse video reads as shaded letters
                let bar = bar.replace("\u{1b}[2m", "").replace("\u{1b}[22m", "");
                out.push_str(&format!("\u{1b}[7m{}\u{1b}[0m", bar));
            } else {
                // truncation can cut off a closing escape (a dimmed tail) —
                // don't let it bleed into the rows below
                out.push_str(&line);
                out.push_str("\u{1b}[0m");
            }
            out.push_str("\r\n");
        }
        if let Some((_, name)) = &self.moving {
            let mut menu = vec![String::new(), format!("Move \u{1b}[1m{}\u{1b}[22m to:", name)];
            if self.sessions.is_empty() {
                menu.push("\u{1b}[2m(no other session)\u{1b}[22m".to_string());
            }
            for (i, s) in self.sessions.iter().take(9).enumerate() {
                menu.push(format!(" {} {}", i + 1, s));
            }
            menu.push("\u{1b}[2mdigit: move, Esc: cancel\u{1b}[22m".to_string());
            for l in menu {
                out.push_str(&truncate_visible(&format!("{}{}", pad, l), cols));
                out.push_str("\u{1b}[0m\r\n");
            }
        } else if let Some(hints) = self.key_hints() {
            // the sidebar's keys at its bottom while it has them (local change):
            // Zellij's status bar is another plugin and can't list them
            let used = out.matches("\r\n").count();
            if !narrow && rows >= used + hints.len() + 1 {
                out.push_str(&"\r\n".repeat(rows - used - hints.len()));
                for (i, l) in hints.iter().enumerate() {
                    out.push_str(&truncate_visible(&format!("{}\u{1b}[2m{}\u{1b}[22m", pad, l), cols));
                    out.push_str("\u{1b}[0m");
                    if i + 1 < hints.len() {
                        out.push_str("\r\n");
                    }
                }
            }
        }
        out
    }
}

#[cfg(not(test))]
impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.load_inner(configuration);
    }

    fn update(&mut self, event: Event) -> bool {
        match self.role {
            Role::Legacy => self.update_inner(event),
            Role::Screen => self.screen_update(event),
            Role::Brain => {
                // a resize or a moved selection changes the frame too: it is
                // compared with the last one sent, not trusted to the return value
                self.update_inner(event);
                self.push_frame(self.my_id);
                false
            }
        }
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        match self.role {
            Role::Brain => self.brain_pipe(pipe_message),
            _ => self.screen_pipe(pipe_message),
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        let out = if self.role == Role::Screen { self.frame_text.clone() } else { self.frame(rows, cols) };
        print!("{}", out);
    }
}

/// Single-quote a string for `sh -c` (local change: session names in the move command).
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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

/// Pane title without Claude Code's own status glyph (`✳ wsa`, `⠐ wsa`): the
/// row's state icon already says it, and `✳` draws as a 2-column emoji.
fn strip_claude_glyph(title: &str) -> &str {
    let mut chars = title.chars();
    match chars.next() {
        Some(c) if c == '✳' || ('\u{2800}'..='\u{28ff}').contains(&c) => chars.as_str().trim_start(),
        _ => title,
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
    fn keys_and_clicks_roundtrip_through_the_pipe_codec() {
        let mut shift_up = KeyWithModifier::new(BareKey::Up);
        shift_up.key_modifiers.insert(KeyModifier::Shift);
        for k in [
            KeyWithModifier::new(BareKey::Char('r')),
            KeyWithModifier::new(BareKey::Char('д')),
            KeyWithModifier::new(BareKey::Char(' ')),
            KeyWithModifier::new(BareKey::Char('-')),
            KeyWithModifier::new(BareKey::Esc),
            KeyWithModifier::new(BareKey::Enter),
            KeyWithModifier::new(BareKey::PageDown),
            KeyWithModifier::new(BareKey::F(5)),
            shift_up,
        ] {
            assert_eq!(decode_key(&encode_key(&k)).as_ref(), Some(&k), "{:?}", encode_key(&k));
        }
        for m in [Mouse::LeftClick(3, 7), Mouse::ScrollUp(1), Mouse::ScrollDown(2)] {
            let back = decode_mouse(&encode_mouse(&m).unwrap()).unwrap();
            assert_eq!(format!("{:?}", back), format!("{:?}", m));
        }
    }

    #[test]
    fn shell_prompt_title() {
        assert!(is_shell_prompt_title("likemusic@DESKTOP-VPPAGPR: ~/Projects/wsa-lineage"));
        assert!(!is_shell_prompt_title("✳ wsa-vps"));
        assert!(!is_shell_prompt_title("node /home/likemusic/bin/claude --resume"));
        assert!(!is_shell_prompt_title("ssh user@host: whatever"));
    }

    #[test]
    fn parse_attention_variants() {
        assert_eq!(parse_attention("⏳ work:api"), (Some(Attention::Waiting), "work:api"));
        assert_eq!(parse_attention("work:api ⏳"), (Some(Attention::Waiting), "work:api"));
        assert_eq!(parse_attention("✅ db"), (Some(Attention::Completed), "db"));
        assert_eq!(parse_attention("⚙ run"), (Some(Attention::Working), "run"));
        assert_eq!(parse_attention("plain"), (None, "plain"));
    }

    #[test]
    fn markers_roundtrip_for_all_states() {
        for att in [Attention::Waiting, Attention::Working, Attention::Completed] {
            let name = format!("{}x:y", marker_of(att));
            assert_eq!(parse_attention(&name), (Some(att), "x:y"));
        }
    }

    #[test]
    fn parse_attention_roundtrips_with_set() {
        let (_, base) = parse_attention("⏳ x:y");
        assert_eq!(format!("{}{}", MARK_WAITING, base), "⏳ x:y");
    }

    #[test]
    fn parse_worktrees_main_first_skips_bare_and_prunable() {
        let out = "worktree /r/develop\nHEAD abc\nbranch refs/heads/develop\n\n\
                   worktree /r/fix\nHEAD def\nbranch refs/heads/fix/auth\n\n\
                   worktree /r/ci\nHEAD 123\ndetached\n\n\
                   worktree /r/gone\nHEAD 456\nbranch refs/heads/gone\nprunable gitdir file points to non-existent location\n";
        assert_eq!(
            parse_worktrees(out),
            vec![
                ("/r/develop".to_string(), "develop".to_string()),
                ("/r/fix".to_string(), "fix/auth".to_string()),
                ("/r/ci".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn groups_file_skips_comments_and_junk() {
        let r = parse_groups_file("# x\n\n/a/** -> A\nbad line\n  /b -> B  \n");
        assert_eq!(r, vec![("/a/**".to_string(), "A".to_string()), ("/b".to_string(), "B".to_string())]);
    }

    #[test]
    fn dir_group_path_named_and_auto() {
        let rules = vec![
            ("/h/P/hospice.by/**".to_string(), "hospice.by".to_string()),
            ("/h/W/**".to_string(), "Work/*".to_string()),
            ("/h/P/**".to_string(), "*".to_string()),
        ];
        let g = |c: &str| dir_group_path(&rules, c);
        assert_eq!(g("/h/P/hospice.by"), Some((vec!["hospice.by".to_string()], 1)));
        assert_eq!(g("/h/P/hospice.by/SS"), Some((vec!["hospice.by".to_string()], 1)));
        assert_eq!(g("/h/P/tm/cb"), Some((vec!["tm".to_string(), "cb".to_string()], 0)));
        assert_eq!(g("/h/W/a"), Some((vec!["Work".to_string(), "a".to_string()], 1)));
        assert_eq!(g("/h"), None);
    }

    #[test]
    fn fold_and_tree_drop_single_tab_levels() {
        let v = |s: &[&str], f: usize| (s.iter().map(|x| x.to_string()).collect::<Vec<_>>(), f);
        let raw = vec![
            v(&["tm", "cb", "develop"], 0),
            v(&["tm", "cb", "develop"], 0),
            v(&["tm", "cb", "rosatom"], 0),
            v(&["O", "belfermer"], 0),
            v(&["hospice.by"], 1),
        ];
        let paths: Vec<String> = fold_paths(&raw).iter().map(|s| s.join("/")).collect();
        assert_eq!(paths, ["tm/cb/develop", "tm/cb/develop", "tm/cb", "", "hospice.by"]);
        let named: Vec<String> = ["tm/cb/develop", "tm/cb", "hospice.by"].iter().map(|s| s.to_string()).collect();
        let order = appearance_paths(named.iter());
        let rows: Vec<(String, String, usize)> =
            group_tree(&named, &order).into_iter().map(|n| (n.label, n.top, n.depth)).collect();
        assert_eq!(
            rows,
            [
                ("tm/cb".to_string(), "tm".to_string(), 0),
                ("develop".to_string(), "tm/cb/develop".to_string(), 1),
                ("hospice.by".to_string(), "hospice.by".to_string(), 0),
            ]
        );
    }

    #[test]
    fn group_of_splits_and_defaults() {
        let s = State { separator: ':', ..Default::default() };
        assert_eq!(s.group_of("work:api"), ("work".to_string(), "api"));
        assert_eq!(s.group_of("work: api"), ("work".to_string(), "api")); // trims one space
        assert_eq!(s.group_of("scratch"), ("General".to_string(), "scratch"));
        assert_eq!(s.group_of("TM:corp-back:task"), ("TM/corp-back".to_string(), "task"));
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
            status_view: Some(true),
            width: Some(30),
            width_saved: Some(28),
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
    fn strip_claude_glyph_only_drops_a_leading_status_glyph() {
        assert_eq!(strip_claude_glyph("✳ rosatom"), "rosatom");
        assert_eq!(strip_claude_glyph("⠐ wsa"), "wsa");
        assert_eq!(strip_claude_glyph("…/src"), "…/src");
        assert_eq!(strip_claude_glyph("a ✳ b"), "a ✳ b");
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
            parse_glob_rules(&cfg, "autogroup_"),
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
    fn background_column_shows_shells_and_monitors() {
        let mut s = State {
            shell_icon: "$".into(),
            monitor_icon: "@".into(),
            tab_panes: [(0, vec![(1, "a".to_string(), false), (2, "b".to_string(), false)])].into(),
            ..Default::default()
        };
        // the cells are reserved even with no background work
        assert_eq!(s.background_slot((0, 0)), "  ");
        assert!(s.set_pane_background(1, "2 0"));
        assert!(!s.set_pane_background(1, "2 0"));
        assert!(s.set_pane_background(2, "0 1"));
        let plain = |x: String| x.replace("\u{1b}[35m", "").replace("\u{1b}[34m", "").replace("\u{1b}[39m", "");
        assert_eq!(plain(s.background_slot(s.background_of([&1]))), "$ ");
        assert_eq!(plain(s.background_slot(s.background_of([&2]))), " @");
        assert_eq!(plain(s.background_slot(s.tab_background(0))), "$@");
        assert_eq!(plain(s.state_slot(None, false, s.tab_background(0))), " $@ ");
        assert!(s.set_pane_background(1, "0 0"));
        assert!(s.set_pane_background(2, "0 0"));
        assert_eq!(s.tab_background(0), (0, 0));
        assert_eq!(s.state_slot(None, false, (0, 0)), "    ");
    }

    #[test]
    fn status_view_lists_sessions_under_their_state() {
        let tab = |position: usize, name: &str| TabInfo {
            position,
            name: name.to_string(),
            ..Default::default()
        };
        let pane = |id: u32, title: &str| (id, title.to_string(), false);
        let s = State {
            separator: ':',
            status_view: true,
            tabs: vec![
                tab(0, &format!("{}work:api", MARK_COMPLETED)),
                tab(1, "notes"),
            ],
            tab_panes: [
                (0, vec![pane(1, "a"), pane(2, "b"), pane(3, "c")]),
                (1, vec![pane(4, "notes")]),
            ]
            .into(),
            pane_attention: [(1, Attention::Working), (3, Attention::Waiting)].into(),
            ..Default::default()
        };
        let got: Vec<String> = s
            .build_rows()
            .iter()
            .map(|r| match r {
                Row::Group { name, .. } => name.clone(),
                Row::Pane { title, .. } => format!("  {}", title.split('\u{1b}').next().unwrap()),
                Row::Tab { .. } => "tab?".to_string(),
                Row::Worktree { .. } => "worktree?".to_string(),
            })
            .collect();
        assert_eq!(
            got,
            ["Waiting", "  c", "Working", "  a", "Idle", "  b", "  notes"]
        );
        // the tab rolls its panes up (waiting wins) over its own completed marker
        assert_eq!(s.pane_rollup(0), Some(Attention::Waiting));
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
