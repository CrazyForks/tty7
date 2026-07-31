use tty7_core::core::machine::{Machine, PaneNode, Workspace};
use tty7_core::core::session::WorkspaceId;
use tty7_core::daemon::control::{PaneAgentState, RouteInfo, ServerStatus};
use tty7_core::daemon::protocol::PaneProcs;

use crate::resolve;

pub fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }
    let mut out = String::new();
    render_row(&mut out, &mut header.iter().map(|h| h.to_string()), &widths);
    for row in rows {
        render_row(&mut out, &mut row.iter().cloned(), &widths);
    }
    out
}

fn render_row(out: &mut String, cells: &mut dyn Iterator<Item = String>, widths: &[usize]) {
    let mut line = String::new();
    for (i, cell) in cells.enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        line.push_str(&cell);
        let pad = widths.get(i).copied().unwrap_or(0).saturating_sub(cell.len());
        line.push_str(&" ".repeat(pad));
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

pub fn workspace_table(machine: &Machine) -> String {
    if machine.workspaces.is_empty() {
        return "no workspaces — `tty7 new <path>` starts one\n".to_string();
    }
    let rows: Vec<Vec<String>> = machine
        .workspaces
        .iter()
        .map(|ws| {
            vec![
                resolve::short_id(&ws.id),
                ws.name.clone().unwrap_or_else(|| "-".to_string()),
                ws.tabs.len().to_string(),
                ws.tabs
                    .iter()
                    .map(|t| t.root.pane_ids().len())
                    .sum::<usize>()
                    .to_string(),
                ws.attachment
                    .as_ref()
                    .map(|a| a.hostname.clone())
                    .unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();
    table(&["WORKSPACE", "NAME", "TABS", "PANES", "ATTACHED"], &rows)
}

pub fn pane_table(machine: &Machine, only: Option<WorkspaceId>) -> String {
    let mut rows = Vec::new();
    for ws in &machine.workspaces {
        if only.is_some_and(|id| id != ws.id) {
            continue;
        }
        for tab in &ws.tabs {
            let ordinal = resolve::ordinal_of(machine, tab.id).unwrap_or(0);
            for pane in tab.root.pane_ids() {
                let record = machine.panes.iter().find(|p| p.id == pane);
                rows.push(vec![
                    format!("%{pane}"),
                    ws.name.clone().unwrap_or_else(|| resolve::short_id(&ws.id)),
                    format!("@{ordinal}"),
                    record
                        .and_then(|r| r.cwd.clone())
                        .unwrap_or_else(|| "-".to_string()),
                    record
                        .map(|r| if r.live { "yes" } else { "no" }.to_string())
                        .unwrap_or_else(|| "?".to_string()),
                ]);
            }
        }
    }
    if rows.is_empty() {
        return "no panes\n".to_string();
    }
    table(&["PANE", "WS", "TAB", "CWD", "LIVE"], &rows)
}

pub fn workspace_tree(ws: &Workspace, machine: &Machine) -> String {
    let mut out = format!(
        "{} ({})\n",
        ws.name.as_deref().unwrap_or("-"),
        resolve::short_id(&ws.id)
    );
    for tab in &ws.tabs {
        let ordinal = resolve::ordinal_of(machine, tab.id).unwrap_or(0);
        match &tab.name {
            Some(name) => out.push_str(&format!("  @{ordinal}  {name}\n")),
            None => out.push_str(&format!("  @{ordinal}\n")),
        }
        render_node(&mut out, &tab.root, machine, 2);
    }
    out
}

fn render_node(out: &mut String, node: &PaneNode, machine: &Machine, depth: usize) {
    let indent = "  ".repeat(depth);
    match node {
        PaneNode::Leaf { pane } => {
            let cwd = machine
                .panes
                .iter()
                .find(|p| p.id == *pane)
                .and_then(|p| p.cwd.clone());
            match cwd {
                Some(cwd) => out.push_str(&format!("{indent}%{pane}  {cwd}\n")),
                None => out.push_str(&format!("{indent}%{pane}\n")),
            }
        }
        PaneNode::Split { axis, ratio, a, b } => {
            let axis = match axis {
                tty7_core::core::machine::Axis::Horizontal => "h",
                tty7_core::core::machine::Axis::Vertical => "v",
            };
            out.push_str(&format!("{indent}{axis} {:.0}%\n", ratio * 100.0));
            render_node(out, a, machine, depth + 1);
            render_node(out, b, machine, depth + 1);
        }
    }
}

pub fn procs_tables(procs: &PaneProcs) -> String {
    if procs.procs.is_empty() && procs.ports.is_empty() {
        return "nothing running in this pane\n".to_string();
    }
    let rows: Vec<Vec<String>> = procs
        .procs
        .iter()
        .map(|p| {
            vec![
                p.pid.to_string(),
                format!("{}{}", "  ".repeat(p.depth as usize), p.name),
                if p.foreground { "*" } else { "" }.to_string(),
            ]
        })
        .collect();
    let mut out = table(&["PID", "NAME", "FG"], &rows);
    if !procs.ports.is_empty() {
        out.push('\n');
        let rows: Vec<Vec<String>> = procs
            .ports
            .iter()
            .map(|p| vec![p.port.to_string(), p.pid.to_string(), p.name.clone()])
            .collect();
        out.push_str(&table(&["PORT", "PID", "NAME"], &rows));
    }
    out
}

pub fn agents_table(states: &[PaneAgentState]) -> String {
    if states.is_empty() {
        return "no agents running\n".to_string();
    }
    let rows: Vec<Vec<String>> = states
        .iter()
        .map(|s| {
            vec![
                format!("%{}", s.pane_id),
                s.agent
                    .map(|a| format!("{a:?}").to_lowercase())
                    .unwrap_or_else(|| "-".to_string()),
                format!("{:?}", s.state.status).to_lowercase(),
                s.state.message.clone().unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();
    table(&["PANE", "AGENT", "STATUS", "MESSAGE"], &rows)
}

pub fn status_lines(status: &ServerStatus) -> String {
    let rows = vec![
        vec!["pid".to_string(), status.pid.to_string()],
        vec!["uptime".to_string(), format!("{}s", status.uptime_secs)],
        vec!["panes".to_string(), status.panes.to_string()],
        vec![
            "dialect".to_string(),
            format!(
                "control v{}, protocol v{}",
                status.control_version, status.protocol_version
            ),
        ],
        vec!["build".to_string(), status.build.clone()],
        vec!["socket".to_string(), status.socket.clone()],
    ];
    table(&["SERVER", ""], &rows)
}

pub fn routes_table(routes: &[RouteInfo]) -> String {
    let mut rows = vec![vec![
        "local".to_string(),
        "local".to_string(),
        "yes".to_string(),
    ]];
    rows.extend(routes.iter().map(|r| {
        vec![
            r.key.clone(),
            r.kind.clone(),
            if r.connected { "yes" } else { "no" }.to_string(),
        ]
    }));
    table(&["MACHINE", "KIND", "CONNECTED"], &rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testbed::two_workspace_machine;
    use tty7_core::daemon::protocol::{PortEntry, ProcEntry};

    #[test]
    fn columns_line_up_and_lines_carry_no_trailing_spaces() {
        let rendered = table(
            &["PANE", "CWD"],
            &[
                vec!["%1".into(), "C:\\proj".into()],
                vec!["%12345".into(), "C:\\".into()],
            ],
        );
        assert_eq!(rendered, "PANE    CWD\n%1      C:\\proj\n%12345  C:\\\n");
        assert!(
            rendered.lines().all(|l| l == l.trim_end()),
            "trailing spaces break naive downstream parsing"
        );
    }

    #[test]
    fn the_workspace_table_is_one_line_per_workspace() {
        let m = two_workspace_machine();
        let rendered = workspace_table(&m);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3, "header plus two workspaces:\n{rendered}");
        assert!(lines[0].starts_with("WORKSPACE"));
        assert!(lines[1].contains("api"), "{rendered}");
        assert!(lines[1].contains('3'), "api holds three panes: {rendered}");
        assert!(lines[2].contains("web"), "{rendered}");
    }

    #[test]
    fn an_empty_machine_says_how_to_start() {
        let rendered = workspace_table(&Machine::default());
        assert!(rendered.contains("tty7 new"), "{rendered}");
    }

    #[test]
    fn the_pane_table_addresses_panes_and_tabs_the_way_commands_take_them() {
        let m = two_workspace_machine();
        let rendered = pane_table(&m, None);
        assert!(rendered.contains("%1"), "{rendered}");
        assert!(rendered.contains("%5"), "{rendered}");
        assert!(rendered.contains("@3"), "web's tab keeps its machine-wide number: {rendered}");

        let web_only = pane_table(&m, Some(m.workspaces[1].id));
        assert!(!web_only.contains("%1"), "{web_only}");
        assert!(web_only.contains("%5"), "{web_only}");
    }

    #[test]
    fn the_tree_shows_tabs_splits_and_cwds_by_indentation() {
        let m = two_workspace_machine();
        let rendered = workspace_tree(&m.workspaces[0], &m);
        let expected = format!(
            "api ({})\n  @1  build\n    %1  C:\\proj\n  @2\n    h 50%\n      %2  C:\\proj\n      %3  C:\\proj\\sub\n",
            crate::resolve::short_id(&m.workspaces[0].id)
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn procs_render_as_a_process_tree_plus_ports() {
        let procs = PaneProcs {
            procs: vec![
                ProcEntry {
                    pid: 100,
                    name: "pwsh".into(),
                    depth: 0,
                    foreground: false,
                },
                ProcEntry {
                    pid: 200,
                    name: "cargo".into(),
                    depth: 1,
                    foreground: true,
                },
            ],
            ports: vec![PortEntry {
                port: 3000,
                pid: 200,
                name: "node".into(),
            }],
        };
        let rendered = procs_tables(&procs);
        assert!(rendered.contains("  cargo"), "children are indented: {rendered}");
        assert!(rendered.contains('*'), "the foreground process is marked: {rendered}");
        assert!(rendered.contains("3000"), "{rendered}");
    }
}
