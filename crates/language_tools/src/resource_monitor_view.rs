use std::{
    any::TypeId,
    collections::HashMap,
    time::{Duration, Instant},
};

use gpui::{
    actions, App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Render, SharedString, Styled, Subscription, Task, Window, div, px,
};
use project::Project;
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
use ui::prelude::*;
use workspace::{SplitDirection, Workspace, WorkspaceId, item::ItemEvent};

use crate::get_or_create_tool;

actions!(
    dev,
    [
        /// Opens the resource monitor, showing CPU and memory usage for all Zed-managed processes.
        OpenResourceMonitor
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        workspace.register_action(|workspace, _: &OpenResourceMonitor, window, cx| {
            let project = workspace.project().clone();
            get_or_create_tool(
                workspace,
                SplitDirection::Right,
                window,
                cx,
                move |window, cx| ResourceMonitorView::new(project, window, cx),
            );
        });
    })
    .detach();
}

// ── Data model ──────────────────────────────────────────────────────────

const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ProcessCategory {
    LanguageServer,
    Terminal,
    MainProcess,
}

impl ProcessCategory {
    fn label(self) -> &'static str {
        match self {
            Self::LanguageServer => "Language Servers",
            Self::Terminal => "Terminals",
            Self::MainProcess => "Zed",
        }
    }
}

#[derive(Debug, Clone)]
struct ProcessEntry {
    name: String,
    context: Option<String>,
    category: ProcessCategory,
    pid: Option<u32>,
    cpu_percent: f32,
    memory_bytes: u64,
}

#[derive(Debug, Clone, Default)]
struct ProcessSnapshot {
    entries: Vec<ProcessEntry>,
    total_cpu_percent: f32,
    total_memory_bytes: u64,
}

// ── Collector (GPUI entity) ─────────────────────────────────────────────

struct ProcessCollector {
    project: Entity<Project>,
    system: System,
    snapshot: ProcessSnapshot,
    last_refresh: Option<Instant>,
    _poll_task: Task<()>,
}

impl ProcessCollector {
    fn new(project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        let poll_task = cx.spawn(|this, cx| async move {
            loop {
                cx.background_executor()
                    .timer(POLL_INTERVAL)
                    .await;
                let result = this.update_in(&cx, |this, _window, cx| {
                    this.refresh(cx);
                });
                if result.is_err() {
                    break;
                }
            }
        });

        let mut collector = Self {
            project,
            system: System::new(),
            snapshot: ProcessSnapshot::default(),
            last_refresh: None,
            _poll_task: poll_task,
        };
        // Initial placeholder entry while first real refresh happens.
        collector.snapshot.entries.push(ProcessEntry {
            name: "Zed".into(),
            context: None,
            category: ProcessCategory::MainProcess,
            pid: Some(std::process::id()),
            cpu_percent: 0.0,
            memory_bytes: 0,
        });
        collector
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let refresh_kind = RefreshKind::nothing()
            .with_processes(ProcessRefreshKind::nothing().without_tasks().with_memory().with_cpu());
        self.system.refresh_specifics(refresh_kind);
        self.last_refresh = Some(Instant::now());

        let mut entries = Vec::new();
        let parent_map = self.build_parent_map();
        let zed_pid = std::process::id();

        // ── Main Zed process ────────────────────────────────────────────
        let zed_mem = self.memory_for_tree(Pid::from_u32(zed_pid), &parent_map);
        let zed_cpu = self
            .system
            .process(Pid::from_u32(zed_pid))
            .map(|p| p.cpu_usage())
            .unwrap_or(0.0);
        entries.push(ProcessEntry {
            name: "Zed".into(),
            context: None,
            category: ProcessCategory::MainProcess,
            pid: Some(zed_pid),
            cpu_percent: zed_cpu,
            memory_bytes: zed_mem,
        });

        // ── Language servers ────────────────────────────────────────────
        let project = self.project.read(cx);
        let lsp_store = project.lsp_store().read(cx);
        for (_server_id, status) in lsp_store.language_server_statuses() {
            let worktree_name = status.worktree.and_then(|wt_id| {
                project
                    .worktree_for_id(wt_id, cx)
                    .map(|wt| wt.read(cx).root_name().to_string())
            });

            let (pid, cpu, mem) = if let Some(process_id) = status.process_id {
                let root_pid = Pid::from_u32(process_id);
                let cpu = self
                    .system
                    .process(root_pid)
                    .map(|p| p.cpu_usage())
                    .unwrap_or(0.0);
                let mem = self.memory_for_tree(root_pid, &parent_map);
                (Some(process_id), cpu, mem)
            } else {
                (None, 0.0, 0)
            };

            entries.push(ProcessEntry {
                name: status.name.0.to_string(),
                context: worktree_name,
                category: ProcessCategory::LanguageServer,
                pid,
                cpu_percent: cpu,
                memory_bytes: mem,
            });
        }

        // ── Terminals ───────────────────────────────────────────────────
        for weak_terminal in project.local_terminal_handles() {
            if let Some(terminal) = weak_terminal.upgrade() {
                let terminal = terminal.read(cx);
                let title = terminal.title(true);
                let pid = terminal.pid();
                let (pid_u32, cpu, mem) = if let Some(p) = pid {
                    let cpu = self
                        .system
                        .process(p)
                        .map(|proc_| proc_.cpu_usage())
                        .unwrap_or(0.0);
                    let mem = self.memory_for_tree(p, &parent_map);
                    (Some(p.as_u32()), cpu, mem)
                } else {
                    (None, 0.0, 0)
                };
                entries.push(ProcessEntry {
                    name: title,
                    context: None,
                    category: ProcessCategory::Terminal,
                    pid: pid_u32,
                    cpu_percent: cpu,
                    memory_bytes: mem,
                });
            }
        }

        // Sort entries by category then name.
        entries.sort_by(|a, b| {
            a.category
                .cmp(&b.category)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        let total_cpu: f32 = entries.iter().map(|e| e.cpu_percent).sum();
        let total_mem: u64 = entries.iter().map(|e| e.memory_bytes).sum();

        self.snapshot = ProcessSnapshot {
            entries,
            total_cpu_percent: total_cpu,
            total_memory_bytes: total_mem,
        };
        cx.notify();
    }

    fn build_parent_map(&self) -> HashMap<Pid, Pid> {
        self.system
            .processes()
            .iter()
            .filter_map(|(&pid, process)| Some((pid, process.parent()?)))
            .collect()
    }

    /// Sum memory of `root_pid` and all its descendant processes.
    fn memory_for_tree(&self, root_pid: Pid, parent_map: &HashMap<Pid, Pid>) -> u64 {
        self.system
            .processes()
            .iter()
            .filter(|(pid, _)| is_descendant_of(**pid, root_pid, parent_map))
            .map(|(_, process)| process.memory())
            .sum()
    }
}

/// Walk the parent chain from `pid` up to `root_pid`.
/// Returns `true` if `pid == root_pid` or if `pid` is a descendant of `root_pid`.
fn is_descendant_of(pid: Pid, root_pid: Pid, parent_map: &HashMap<Pid, Pid>) -> bool {
    let mut current = pid;
    let mut visited = collections::HashSet::default();
    while current != root_pid {
        if !visited.insert(current) {
            return false;
        }
        match parent_map.get(&current) {
            Some(&parent) => current = parent,
            None => return false,
        }
    }
    true
}

// ── View ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum SortColumn {
    Name,
    Pid,
    Cpu,
    Memory,
}

pub struct ResourceMonitorView {
    collector: Entity<ProcessCollector>,
    focus_handle: FocusHandle,
    sort_column: SortColumn,
    sort_ascending: bool,
    _subscription: Subscription,
}

impl EventEmitter<ItemEvent> for ResourceMonitorView {}

impl ResourceMonitorView {
    pub fn new(
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let collector = cx.new(|cx| ProcessCollector::new(project, cx));
        let subscription = cx.observe(&collector, |_, _, cx| cx.notify());
        let focus_handle = cx.focus_handle();

        Self {
            collector,
            focus_handle,
            sort_column: SortColumn::Cpu,
            sort_ascending: false,
            _subscription: subscription,
        }
    }

    fn sorted_entries(&self, snapshot: &ProcessSnapshot) -> Vec<ProcessEntry> {
        let mut entries = snapshot.entries.clone();
        let ascending = self.sort_ascending;
        match self.sort_column {
            SortColumn::Name => entries.sort_by(|a, b| {
                let cmp = a.name.to_lowercase().cmp(&b.name.to_lowercase());
                if ascending { cmp } else { cmp.reverse() }
            }),
            SortColumn::Pid => entries.sort_by(|a, b| {
                let cmp = a.pid.cmp(&b.pid);
                if ascending { cmp } else { cmp.reverse() }
            }),
            SortColumn::Cpu => entries.sort_by(|a, b| {
                let cmp = a
                    .cpu_percent
                    .partial_cmp(&b.cpu_percent)
                    .unwrap_or(std::cmp::Ordering::Equal);
                if ascending { cmp } else { cmp.reverse() }
            }),
            SortColumn::Memory => entries.sort_by(|a, b| {
                let cmp = a.memory_bytes.cmp(&b.memory_bytes);
                if ascending { cmp } else { cmp.reverse() }
            }),
        }
        entries
    }

    fn toggle_sort(&mut self, column: SortColumn, cx: &mut Context<Self>) {
        if self.sort_column == column {
            self.sort_ascending = !self.sort_ascending;
        } else {
            self.sort_column = column;
            self.sort_ascending = false;
        }
        cx.notify();
    }

    fn copy_report(&self, _window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.collector.read(cx).snapshot.clone();
        let entries = self.sorted_entries(&snapshot);

        let mut report = String::new();
        report.push_str("Zed Resource Monitor Report\n");
        report.push_str(&format!(
            "Total: {:.1}% CPU · {}\n\n",
            snapshot.total_cpu_percent,
            format_bytes(snapshot.total_memory_bytes),
        ));

        let mut current_category: Option<ProcessCategory> = None;
        for entry in &entries {
            if current_category != Some(entry.category) {
                current_category = Some(entry.category);
                report.push_str(&format!("\n{}:\n", entry.category.label()));
            }
            let ctx = entry
                .context
                .as_ref()
                .map(|c| format!(" ({})", c))
                .unwrap_or_default();
            report.push_str(&format!(
                "  {}{} PID:{} CPU:{:.1}% Mem:{}\n",
                entry.name,
                ctx,
                entry.pid.map_or("—".to_string(), |p| p.to_string()),
                entry.cpu_percent,
                format_bytes(entry.memory_bytes),
            ));
        }

        cx.write_to_clipboard(ClipboardItem::new_string(report));
    }

    fn sort_indicator(&self, column: SortColumn) -> &'static str {
        if self.sort_column == column {
            if self.sort_ascending { " ▲" } else { " ▼" }
        } else {
            ""
        }
    }

    fn render_header(&self, snapshot: &ProcessSnapshot, cx: &App) -> impl IntoElement {
        let count = snapshot.entries.len();
        h_flex()
            .id("resource-monitor-header")
            .px_3()
            .py_2()
            .w_full()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Sliders)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .child(
                        Label::new("Resource Monitor")
                            .size(LabelSize::Default)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(format!(
                            "— {} process{} · {:.1}% CPU · {}",
                            count,
                            if count == 1 { "" } else { "es" },
                            snapshot.total_cpu_percent,
                            format_bytes(snapshot.total_memory_bytes),
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
    }

    fn render_column_headers(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let col_name = format!("Name{}", self.sort_indicator(SortColumn::Name));
        let col_pid = format!("PID{}", self.sort_indicator(SortColumn::Pid));
        let col_cpu = format!("CPU{}", self.sort_indicator(SortColumn::Cpu));
        let col_mem = format!("Memory{}", self.sort_indicator(SortColumn::Memory));

        h_flex()
            .id("column-headers")
            .px_3()
            .py_1()
            .w_full()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().surface_background)
            .child(
                div()
                    .id("col-name")
                    .flex_1()
                    .min_w(px(140.))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _w, cx| this.toggle_sort(SortColumn::Name, cx)))
                    .child(Label::new(col_name).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(
                div()
                    .id("col-pid")
                    .w(px(70.))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _w, cx| this.toggle_sort(SortColumn::Pid, cx)))
                    .child(Label::new(col_pid).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(
                div()
                    .id("col-cpu")
                    .w(px(70.))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _w, cx| this.toggle_sort(SortColumn::Cpu, cx)))
                    .child(Label::new(col_cpu).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(
                div()
                    .id("col-mem")
                    .w(px(90.))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _w, cx| this.toggle_sort(SortColumn::Memory, cx)))
                    .child(Label::new(col_mem).size(LabelSize::Small).color(Color::Muted)),
            )
    }

    fn render_category_header(&self, category: ProcessCategory, cx: &App) -> impl IntoElement {
        h_flex()
            .id(SharedString::from(format!("cat-{:?}", category)))
            .px_3()
            .py_1()
            .w_full()
            .bg(cx.theme().colors().surface_background)
            .child(
                Label::new(category.label())
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            )
    }

    fn render_row(&self, entry: &ProcessEntry, idx: usize, cx: &App) -> impl IntoElement {
        let cpu_color = if entry.cpu_percent > 50.0 {
            Color::Error
        } else if entry.cpu_percent > 20.0 {
            Color::Warning
        } else {
            Color::Muted
        };

        let mem_color = if entry.memory_bytes > 2 * 1024 * 1024 * 1024 {
            Color::Error
        } else if entry.memory_bytes > 500 * 1024 * 1024 {
            Color::Warning
        } else {
            Color::Default
        };

        let (icon, icon_color) = match entry.category {
            ProcessCategory::MainProcess => (IconName::Server, Color::Accent),
            ProcessCategory::LanguageServer => (IconName::FileCode, Color::Info),
            ProcessCategory::Terminal => (IconName::Terminal, Color::Default),
        };

        h_flex()
            .id(ElementId::NamedInteger("row".into(), idx))
            .px_3()
            .py(px(3.))
            .w_full()
            .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(140.))
                    .gap_1()
                    .child(Icon::new(icon).size(IconSize::Small).color(icon_color))
                    .child(Label::new(entry.name.clone()).size(LabelSize::Small))
                    .when_some(entry.context.as_ref(), |el, ctx| {
                        el.child(
                            Label::new(format!("({})", ctx))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .child(
                div().w(px(70.)).child(
                    Label::new(entry.pid.map_or("—".to_string(), |p| p.to_string()))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(
                div().w(px(70.)).child(
                    Label::new(format!("{:.1}%", entry.cpu_percent))
                        .size(LabelSize::Small)
                        .color(cpu_color),
                ),
            )
            .child(
                div().w(px(90.)).child(
                    Label::new(format_bytes(entry.memory_bytes))
                        .size(LabelSize::Small)
                        .color(mem_color),
                ),
            )
    }
}

impl Render for ResourceMonitorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.collector.read(cx).snapshot.clone();
        let entries = self.sorted_entries(&snapshot);

        let mut children: Vec<gpui::AnyElement> = Vec::new();
        let mut current_category: Option<ProcessCategory> = None;
        for (i, entry) in entries.iter().enumerate() {
            if current_category != Some(entry.category) {
                current_category = Some(entry.category);
                children.push(self.render_category_header(entry.category, cx).into_any_element());
            }
            children.push(self.render_row(entry, i, cx).into_any_element());
        }

        if entries.is_empty() {
            children.push(
                div()
                    .id("empty")
                    .p_4()
                    .child(Label::new("Collecting process data…").size(LabelSize::Small).color(Color::Muted))
                    .into_any_element(),
            );
        }

        div()
            .id("resource-monitor")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().colors().background)
            .child(self.render_header(&snapshot, cx))
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Button::new("copy-report", "Copy Report")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .icon(IconName::Copy)
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.copy_report(window, cx);
                            })),
                    ),
            )
            .child(self.render_column_headers(cx))
            .child(
                div()
                    .id("rows-scroll")
                    .flex_grow()
                    .overflow_y_scroll()
                    .children(children),
            )
    }
}

impl Focusable for ResourceMonitorView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl workspace::Item for ResourceMonitorView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Resource Monitor".into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::Sliders)
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn can_split(&self) -> bool {
        false
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        Task::ready(None)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else {
            None
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
