//! Command palette: one searchable registry unifying the ex-command table
//! (`commands::COMMANDS`) and the Normal-mode keybind actions
//! (`keybinds::ALL`). The palette is a pure *name layer* — executing an entry
//! reuses `reducer::dispatch_action` / `commands::execute_command`; nothing
//! here has behaviour of its own.

use crate::keybinds::{BindableAction, KeyMap};

/// What pressing Enter on an entry does.
#[derive(Clone, Debug)]
pub enum PaletteExec {
    /// Fire a keybind action through `reducer::dispatch_action`.
    Action(BindableAction),
    /// Run an ex command's no-arg form through `commands::execute_command`.
    Command(&'static str),
    /// Close the palette and pre-fill the `:` line with `"<name> "` — for
    /// commands that require arguments (`:set key=value`).
    Prefill(&'static str),
}

#[derive(Clone, Debug)]
pub struct PaletteEntry {
    /// Canonical display/search name (command name or action snake_case name).
    pub name: &'static str,
    pub description: &'static str,
    /// Formatted keybind hint (`KeyMap::binding_for`), when the entry has one.
    pub keybind: Option<String>,
    pub exec: PaletteExec,
}

/// `BindableAction`s that are 1:1 with an ex command. The palette shows one
/// entry for the pair: the command's canonical name + description, the
/// action's keybind, executed via the action (same code path either way).
const PAIRS: &[(BindableAction, &str)] = &[
    (BindableAction::OpenTab, "tabnew"),
    (BindableAction::CloseTab, "tabclose"),
    (BindableAction::OpenProjectPopup, "open"),
    (BindableAction::NewWorktree, "worktree"),
    (BindableAction::RemoveWorktree, "worktree-remove"),
    (BindableAction::EditProject, "edit"),
    (BindableAction::LaunchPicker, "launch"),
    (BindableAction::UsagePopup, "usage"),
    (BindableAction::HelpPopup, "help"),
    (BindableAction::Quit, "q"),
];

/// Ex commands with no keybind action. All work in their no-arg form.
const COMMAND_ONLY: &[&str] = &[
    "import",
    "restart-supervisor",
    "reconnect",
    "update",
    "gh-enable",
    "gh-disable",
    "gh-refresh",
    "log-path",
];

/// Keybind actions with no ex-command twin. `LeaveTerminal` is excluded
/// (Terminal scope — meaningless from the palette, which opens in Normal),
/// as is `CommandPalette` itself.
const ACTION_ONLY: &[BindableAction] = &[
    BindableAction::FocusSidebar,
    BindableAction::FocusTerminal,
    BindableAction::SidebarUp,
    BindableAction::SidebarDown,
    BindableAction::ActivateSelection,
    BindableAction::EnterTerminalMode,
    BindableAction::EnterCommandMode,
    BindableAction::NextTab,
    BindableAction::PrevTab,
    BindableAction::SidebarGrow,
    BindableAction::SidebarShrink,
    BindableAction::SidebarReset,
];

fn command_description(name: &str) -> &'static str {
    crate::commands::COMMANDS
        .iter()
        .find(|s| s.names.contains(&name))
        .map(|s| s.description)
        .unwrap_or("")
}

/// Build the full entry list in registry order: deduped pairs, command-only,
/// args-required, action-only. Keybind hints come from the live keymap so
/// user overlays show up.
pub fn build_entries(keymap: &KeyMap) -> Vec<PaletteEntry> {
    let mut out = Vec::with_capacity(PAIRS.len() + COMMAND_ONLY.len() + 1 + ACTION_ONLY.len());
    for (action, name) in PAIRS {
        out.push(PaletteEntry {
            name,
            description: command_description(name),
            keybind: keymap.binding_for(*action),
            exec: PaletteExec::Action(*action),
        });
    }
    for name in COMMAND_ONLY {
        out.push(PaletteEntry {
            name,
            description: command_description(name),
            keybind: None,
            exec: PaletteExec::Command(name),
        });
    }
    out.push(PaletteEntry {
        name: "set",
        description: command_description("set"),
        keybind: None,
        exec: PaletteExec::Prefill("set"),
    });
    for action in ACTION_ONLY {
        out.push(PaletteEntry {
            name: action.name(),
            description: action.description(),
            keybind: keymap.binding_for(*action),
            exec: PaletteExec::Action(*action),
        });
    }
    out
}

/// Case-insensitive ranked filter over name + description. Returns indices
/// into `entries`, best first. Rank: name-prefix > name-substring >
/// name-subsequence > description-substring > description-subsequence;
/// registry order within a rank. Empty/whitespace query returns everything.
pub fn filter(entries: &[PaletteEntry], query: &str) -> Vec<usize> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return (0..entries.len()).collect();
    }
    let mut ranked: Vec<(u8, usize)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let name = e.name.to_ascii_lowercase();
        let desc = e.description.to_ascii_lowercase();
        let rank = if name.starts_with(&q) {
            0
        } else if name.contains(&q) {
            1
        } else if is_subsequence(&q, &name) {
            2
        } else if desc.contains(&q) {
            3
        } else if is_subsequence(&q, &desc) {
            4
        } else {
            continue;
        };
        ranked.push((rank, i));
    }
    ranked.sort_by_key(|&(rank, i)| (rank, i));
    ranked.into_iter().map(|(_, i)| i).collect()
}

/// True when every char of `needle` appears in `haystack` in order.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars();
    needle.chars().all(|n| hay.any(|h| h == n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybinds::defaults;

    #[test]
    fn entry_count_is_stable() {
        // 10 pairs + 8 command-only + 1 prefill (set) + 12 action-only.
        let entries = build_entries(&defaults());
        assert_eq!(entries.len(), 31);
    }

    #[test]
    fn no_duplicate_names() {
        let entries = build_entries(&defaults());
        let mut names: Vec<&str> = entries.iter().map(|e| e.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), entries.len());
    }

    #[test]
    fn every_command_is_reachable() {
        // Every ex command's canonical name appears exactly once, except
        // `palette` itself (no self-entry).
        let entries = build_entries(&defaults());
        for spec in crate::commands::COMMANDS {
            let canonical = spec.names[0];
            if canonical == "palette" {
                continue;
            }
            assert!(
                entries.iter().any(|e| e.name == canonical),
                "command {canonical:?} missing from palette"
            );
        }
    }

    #[test]
    fn every_normal_action_is_reachable_except_exclusions() {
        let entries = build_entries(&defaults());
        for action in crate::keybinds::ALL {
            let excluded = matches!(
                action,
                BindableAction::LeaveTerminal | BindableAction::CommandPalette
            );
            let present = entries
                .iter()
                .any(|e| matches!(e.exec, PaletteExec::Action(a) if a == *action));
            assert_eq!(
                present,
                !excluded,
                "action {} presence wrong",
                action.name()
            );
        }
    }

    #[test]
    fn pair_entries_carry_the_action_keybind() {
        let km = defaults();
        let entries = build_entries(&km);
        let launch = entries.iter().find(|e| e.name == "launch").unwrap();
        assert_eq!(launch.keybind, km.binding_for(BindableAction::LaunchPicker));
        assert!(launch.keybind.is_some());
    }

    #[test]
    fn empty_query_returns_all_in_order() {
        let entries = build_entries(&defaults());
        let idx = filter(&entries, "");
        assert_eq!(idx, (0..entries.len()).collect::<Vec<_>>());
    }

    #[test]
    fn name_prefix_ranks_first() {
        let entries = build_entries(&defaults());
        let idx = filter(&entries, "up");
        // `update` (name-prefix) must come before `usage` and friends that
        // only match by subsequence/description.
        assert_eq!(entries[idx[0]].name, "update");
    }

    #[test]
    fn subsequence_matches_name() {
        let entries = build_entries(&defaults());
        let idx = filter(&entries, "wkt");
        assert!(idx.iter().any(|&i| entries[i].name == "worktree"));
    }

    #[test]
    fn description_matches_too() {
        let entries = build_entries(&defaults());
        let idx = filter(&entries, "memory");
        // `:usage` is found via its description ("…live memory/CPU usage…").
        assert!(idx.iter().any(|&i| entries[i].name == "usage"));
    }

    #[test]
    fn no_match_returns_empty() {
        let entries = build_entries(&defaults());
        assert!(filter(&entries, "zzzzqqqq").is_empty());
    }
}
