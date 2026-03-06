use adw::prelude::*;
use freedesktop_desktop_entry::DesktopEntry;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Once;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::adapters::util::command_exists;
use crate::i18n::{pick, Language};
use crate::models::{parse_canonical_id, Package};
use crate::runtime;
use crate::services::discovery;
use crate::subprocess::run_command;

#[derive(Clone)]
struct SelectedPackage {
    canonical_id: String,
    source: String,
    install_method: String,
    install_path: Option<String>,
    desktop_file: Option<String>,
    uninstall_command: Option<String>,
}

#[derive(Clone, Debug)]
struct SizePathCandidate {
    path: String,
    recursive: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PercentageMode {
    RelativeMax,
    RelativeTotal,
}

impl PercentageMode {
    fn from_index(index: u32) -> Self {
        match index {
            1 => Self::RelativeTotal,
            _ => Self::RelativeMax,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryViewMode {
    Folders,
    Files,
}

impl EntryViewMode {
    fn from_index(index: u32) -> Self {
        match index {
            1 => Self::Files,
            _ => Self::Folders,
        }
    }
}

#[derive(Clone, Copy)]
struct FileRecord {
    dev: u64,
    ino: u64,
    size: u64,
}

#[derive(Clone)]
struct SizeEntry {
    path: String,
    size: u64,
    is_dir: bool,
}

struct SizeScanResult {
    total_size: u64,
    entries: Vec<SizeEntry>,
    targets: Vec<SizePathCandidate>,
    truncated: bool,
    permission_denied: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SizeScanPhase {
    PreparingTargets,
    ScanningTargets,
}

#[derive(Clone, Debug)]
struct SizeScanProgress {
    phase: SizeScanPhase,
    current_path: Option<String>,
    targets_done: usize,
    targets_total: usize,
    scanned_files: usize,
    elapsed_ms: u64,
    eta_ms: Option<u64>,
}

enum SizeScanEvent {
    Progress {
        request_id: u64,
        progress: SizeScanProgress,
    },
    Finished {
        request_id: u64,
        result: Result<SizeScanResult, String>,
    },
}

const SIZE_SCAN_CANCELLED: &str = "__SIZE_SCAN_CANCELLED__";
static PANORAMA_STYLE_ONCE: Once = Once::new();

const PANORAMA_PAGE_CSS: &str = r#"
.size-entry-card {
  padding: 6px 8px;
  border-radius: 10px;
  background: alpha(@window_fg_color, 0.035);
}

.size-entry-meta {
  opacity: 0.72;
  font-size: 0.92em;
}

.size-entry-value {
  font-weight: 650;
}

.size-entry-bar {
  min-width: 180px;
}

.size-entry-bar trough,
.size-entry-bar progress {
  min-height: 9px;
  border-radius: 999px;
}

.size-entry-bar-high progress {
  background: #e95420;
}

.size-entry-bar-mid progress {
  background: #f6ad55;
}

.size-entry-bar-low progress {
  background: #4fd1c5;
}

.size-mode-row {
  margin: 2px 0;
}

.size-mode-box {
  border-radius: 10px;
  background: alpha(@window_fg_color, 0.03);
  padding: 4px 6px;
}

.size-mode-label {
  opacity: 0.78;
  font-size: 0.92em;
}

.size-mode-dropdown {
  min-width: 172px;
}
"#;

pub fn build(token: tokio_util::sync::CancellationToken, lang: Language) -> adw::NavigationPage {
    ensure_panorama_style();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let search_entry = gtk::SearchEntry::new();
    search_entry.set_placeholder_text(Some(pick(lang, "搜索软件包...", "Search packages...")));
    search_entry.set_hexpand(true);

    let system_packages_toggle =
        gtk::ToggleButton::with_label(system_packages_toggle_label(lang, false));
    system_packages_toggle.set_halign(gtk::Align::Start);
    system_packages_toggle.set_active(false);

    let header_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    header_box.set_margin_top(8);
    header_box.set_margin_bottom(8);
    header_box.set_margin_start(12);
    header_box.set_margin_end(12);
    header_box.append(&search_entry);
    header_box.append(&system_packages_toggle);

    vbox.append(&header_box);

    let banner = adw::Banner::new("");
    banner.set_revealed(false);
    vbox.append(&banner);

    let spinner = gtk::Spinner::new();
    spinner.set_spinning(true);
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.set_vexpand(true);

    let status_page = adw::StatusPage::builder()
        .title(pick(lang, "正在扫描软件包...", "Scanning packages..."))
        .child(&spinner)
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&status_page, Some("loading"));

    let list_store = gtk::gio::ListStore::new::<glib::BoxedAnyObject>();
    let query_state = Rc::new(RefCell::new(String::new()));
    let show_system_state = Rc::new(Cell::new(false));

    let filter_query_state = query_state.clone();
    let filter_show_system_state = show_system_state.clone();
    let filter = gtk::CustomFilter::new(move |obj| {
        let Some(boxed) = obj.downcast_ref::<glib::BoxedAnyObject>() else {
            return false;
        };
        let pkg: std::cell::Ref<Package> = boxed.borrow();

        if !filter_show_system_state.get() && is_system_package(&pkg) {
            return false;
        }

        let query = filter_query_state.borrow();
        if query.is_empty() {
            return true;
        }

        pkg.name.to_lowercase().contains(query.as_str())
            || pkg.description.to_lowercase().contains(query.as_str())
            || pkg.source.to_lowercase().contains(query.as_str())
    });
    let filter_model = gtk::FilterListModel::new(Some(list_store.clone()), Some(filter.clone()));
    let selection = gtk::SingleSelection::new(Some(filter_model));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = adw::ActionRow::new();
        // ActionRow 是 GtkListBoxRow，嵌在 GtkListView 中会触发 grab_focus 相关警告；
        // 这里禁用自身可聚焦，交由 ListView 处理焦点与选择。
        row.set_focusable(false);
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(row) = item.child().and_downcast::<adw::ActionRow>() else {
            return;
        };
        let Some(obj) = item.item().and_downcast::<glib::BoxedAnyObject>() else {
            return;
        };
        let pkg: std::cell::Ref<Package> = obj.borrow();
        row.set_title(&glib::markup_escape_text(&pkg.name));
        let subtitle = if pkg.version.is_empty() {
            pkg.source.clone()
        } else {
            format!("{} · {}", pkg.source, pkg.version)
        };
        row.set_subtitle(&glib::markup_escape_text(&subtitle));
        #[allow(deprecated)]
        row.set_icon_name(pkg.icon_name.as_deref());
    });

    let list_view = gtk::ListView::new(Some(selection.clone()), Some(factory));
    list_view.add_css_class("boxed-list");

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list_view)
        .build();

    // Detail panel (right side)
    let detail_group = adw::PreferencesGroup::new();
    detail_group.set_title(pick(lang, "软件包详情", "Package Details"));

    let detail_name = adw::ActionRow::builder()
        .title(pick(lang, "名称", "Name"))
        .subtitle("-")
        .build();
    let detail_version = adw::ActionRow::builder()
        .title(pick(lang, "版本", "Version"))
        .subtitle("-")
        .build();
    let detail_source = adw::ActionRow::builder()
        .title(pick(lang, "来源", "Source"))
        .subtitle("-")
        .build();
    let detail_install_method = adw::ActionRow::builder()
        .title(pick(lang, "安装方式", "Install Method"))
        .subtitle("-")
        .build();
    let detail_size = adw::ActionRow::builder()
        .title(pick(lang, "大小", "Size"))
        .subtitle(pick(
            lang,
            "点击计算（后台统计）",
            "Click to calculate (background)",
        ))
        .build();
    detail_size.set_activatable(true);
    let detail_path = adw::ActionRow::builder()
        .title(pick(lang, "安装路径", "Install Path"))
        .subtitle("-")
        .build();
    detail_path.set_tooltip_text(Some(pick(
        lang,
        "双击打开所在文件夹",
        "Double-click to open containing folder",
    )));
    let detail_uninstall = adw::ActionRow::builder()
        .title(pick(lang, "卸载命令", "Uninstall Command"))
        .subtitle("-")
        .build();
    detail_uninstall.set_activatable(true);
    detail_uninstall.set_tooltip_text(Some(pick(
        lang,
        "点击复制到剪贴板",
        "Click to copy to clipboard",
    )));
    detail_uninstall.set_sensitive(false);
    let uninstall_copy_btn = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text(pick(lang, "复制命令", "Copy command"))
        .valign(gtk::Align::Center)
        .sensitive(false)
        .build();
    uninstall_copy_btn.add_css_class("flat");
    detail_uninstall.add_suffix(&uninstall_copy_btn);
    let detail_desc = adw::ActionRow::builder()
        .title(pick(lang, "描述", "Description"))
        .subtitle("-")
        .build();
    let detail_id = adw::ActionRow::builder()
        .title(pick(lang, "唯一标识", "Canonical ID"))
        .subtitle("-")
        .build();

    detail_group.add(&detail_name);
    detail_group.add(&detail_version);
    detail_group.add(&detail_source);
    detail_group.add(&detail_install_method);
    detail_group.add(&detail_size);
    detail_group.add(&detail_path);
    detail_group.add(&detail_uninstall);
    detail_group.add(&detail_desc);
    detail_group.add(&detail_id);

    let size_detail_group = adw::PreferencesGroup::new();
    size_detail_group.set_title(pick(lang, "大小明细", "Size Details"));

    let size_summary_row = adw::ActionRow::builder()
        .title(pick(lang, "总大小", "Total Size"))
        .subtitle("-")
        .build();
    size_detail_group.add(&size_summary_row);

    let size_targets_row = adw::ActionRow::builder()
        .title(pick(lang, "统计目标", "Targets"))
        .subtitle("-")
        .build();
    size_detail_group.add(&size_targets_row);

    let mode_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    mode_row.set_margin_start(6);
    mode_row.set_margin_end(6);
    mode_row.set_homogeneous(true);
    mode_row.add_css_class("size-mode-row");

    let percent_mode_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    percent_mode_box.set_hexpand(true);
    percent_mode_box.add_css_class("size-mode-box");

    let percent_mode_label = gtk::Label::new(Some(pick(lang, "占比口径", "Ratio mode")));
    percent_mode_label.set_halign(gtk::Align::Start);
    percent_mode_label.set_hexpand(true);
    percent_mode_label.add_css_class("size-mode-label");

    let percent_mode_dropdown = gtk::DropDown::from_strings(&[
        pick(lang, "相对最大项", "Relative to max"),
        pick(lang, "相对总大小", "Relative to total"),
    ]);
    percent_mode_dropdown.set_selected(0);
    percent_mode_dropdown.add_css_class("size-mode-dropdown");
    percent_mode_dropdown.set_size_request(172, -1);

    percent_mode_box.append(&percent_mode_label);
    percent_mode_box.append(&percent_mode_dropdown);

    let entry_view_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    entry_view_box.set_hexpand(true);
    entry_view_box.add_css_class("size-mode-box");

    let entry_view_label = gtk::Label::new(Some(pick(lang, "显示方式", "View mode")));
    entry_view_label.set_halign(gtk::Align::Start);
    entry_view_label.set_hexpand(true);
    entry_view_label.add_css_class("size-mode-label");

    let entry_view_dropdown = gtk::DropDown::from_strings(&[
        pick(lang, "按文件夹", "Folders"),
        pick(lang, "按文件", "Files"),
    ]);
    entry_view_dropdown.set_selected(0);
    entry_view_dropdown.add_css_class("size-mode-dropdown");
    entry_view_dropdown.set_size_request(172, -1);

    entry_view_box.append(&entry_view_label);
    entry_view_box.append(&entry_view_dropdown);

    mode_row.append(&percent_mode_box);
    mode_row.append(&entry_view_box);
    size_detail_group.add(&mode_row);

    let size_detail_list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    size_detail_list.set_hexpand(true);
    size_detail_list.add_css_class("boxed-list");
    size_detail_group.add(&size_detail_list);

    let size_back_btn = gtk::Button::with_label(pick(lang, "返回详情", "Back to details"));
    size_back_btn.set_halign(gtk::Align::Start);
    let size_refresh_btn = gtk::Button::with_label(pick(lang, "重新统计", "Rescan"));
    size_refresh_btn.set_halign(gtk::Align::Start);
    let size_cancel_btn = gtk::Button::with_label(pick(lang, "取消统计", "Cancel scan"));
    size_cancel_btn.set_halign(gtk::Align::Start);
    size_cancel_btn.set_sensitive(false);

    let size_btn_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    size_btn_box.set_margin_top(8);
    size_btn_box.set_margin_bottom(8);
    size_btn_box.set_margin_start(12);
    size_btn_box.set_margin_end(12);
    size_btn_box.append(&size_back_btn);
    size_btn_box.append(&size_refresh_btn);
    size_btn_box.append(&size_cancel_btn);

    let size_detail_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    size_detail_box.set_margin_top(12);
    size_detail_box.set_margin_bottom(12);
    size_detail_box.set_margin_start(12);
    size_detail_box.set_margin_end(12);
    size_detail_box.append(&size_btn_box);
    size_detail_box.append(&size_detail_group);

    let size_detail_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&size_detail_box)
        .build();

    let detail_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    detail_box.set_margin_top(12);
    detail_box.set_margin_bottom(12);
    detail_box.set_margin_start(12);
    detail_box.set_margin_end(12);
    detail_box.append(&detail_group);

    let detail_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&detail_box)
        .build();

    let detail_empty = adw::StatusPage::builder()
        .title(pick(lang, "请选择软件包", "Select a package"))
        .icon_name("package-x-generic-symbolic")
        .build();

    let detail_stack = gtk::Stack::new();
    detail_stack.add_named(&detail_empty, Some("empty"));
    detail_stack.add_named(&detail_scroll, Some("detail"));
    detail_stack.add_named(&size_detail_scroll, Some("size-detail"));
    detail_stack.set_visible_child_name("empty");

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_shrink_start_child(true);
    paned.set_shrink_end_child(true);
    paned.set_start_child(Some(&scrolled));
    paned.set_end_child(Some(&detail_stack));
    paned.set_position(500);

    stack.add_named(&paned, Some("list"));

    let empty_page = adw::StatusPage::builder()
        .title(pick(lang, "未找到软件包", "No packages found"))
        .icon_name("package-x-generic-symbolic")
        .build();
    stack.add_named(&empty_page, Some("empty"));

    vbox.append(&stack);

    // Wire selection -> detail panel
    {
        let ds = detail_stack.clone();
        let dn = detail_name.clone();
        let dv = detail_version.clone();
        let dso = detail_source.clone();
        let dim = detail_install_method.clone();
        let dsz = detail_size.clone();
        let dpath = detail_path.clone();
        let duninstall = detail_uninstall.clone();
        let dd = detail_desc.clone();
        let di = detail_id.clone();
        let uninstall_copy_btn = uninstall_copy_btn.clone();
        let size_summary_row = size_summary_row.clone();
        let size_targets_row = size_targets_row.clone();
        let size_detail_list = size_detail_list.clone();
        let size_refresh_btn = size_refresh_btn.clone();
        let size_cancel_btn = size_cancel_btn.clone();
        let percent_mode_dropdown = percent_mode_dropdown.clone();
        let entry_view_dropdown = entry_view_dropdown.clone();
        let percent_mode_state = Rc::new(Cell::new(PercentageMode::RelativeMax));
        let entry_view_state = Rc::new(Cell::new(EntryViewMode::Folders));
        let size_entries_state = Rc::new(RefCell::new(Vec::<SizeEntry>::new()));
        let size_total_state = Rc::new(Cell::new(0_u64));
        let selected_pkg = Rc::new(RefCell::new(None::<SelectedPackage>));
        let uninstall_command_target = Rc::new(RefCell::new(None::<String>));
        let size_request_id = Rc::new(Cell::new(0_u64));
        let size_scan_token = Rc::new(RefCell::new(None::<CancellationToken>));
        let version_request_id = Rc::new(Cell::new(0_u64));
        let page_token = token.clone();

        let run_size_scan: Rc<dyn Fn()> = Rc::new({
            let selected_pkg = selected_pkg.clone();
            let dsz = dsz.clone();
            let ds = ds.clone();
            let size_summary_row = size_summary_row.clone();
            let size_targets_row = size_targets_row.clone();
            let size_detail_list = size_detail_list.clone();
            let size_refresh_btn = size_refresh_btn.clone();
            let size_cancel_btn = size_cancel_btn.clone();
            let percent_mode_state = percent_mode_state.clone();
            let entry_view_state = entry_view_state.clone();
            let size_entries_state = size_entries_state.clone();
            let size_total_state = size_total_state.clone();
            let size_request_id = size_request_id.clone();
            let size_scan_token = size_scan_token.clone();
            let page_token = page_token.clone();
            move || {
                let Some(pkg) = selected_pkg.borrow().clone() else {
                    dsz.set_subtitle("-");
                    return;
                };

                if let Some(token) = size_scan_token.borrow_mut().take() {
                    token.cancel();
                }

                let scan_token = page_token.child_token();
                *size_scan_token.borrow_mut() = Some(scan_token.clone());

                clear_list_box(&size_detail_list);
                size_summary_row.set_subtitle(pick(lang, "计算中...", "Calculating..."));
                size_targets_row.set_subtitle(pick(lang, "准备统计目标...", "Preparing targets..."));
                size_targets_row.set_tooltip_text(None);
                dsz.set_subtitle(pick(lang, "计算中，请稍候...", "Calculating..."));
                dsz.set_sensitive(false);
                size_refresh_btn.set_sensitive(false);
                size_cancel_btn.set_sensitive(true);
                ds.set_visible_child_name("size-detail");

                let request_id = size_request_id.get().saturating_add(1);
                size_request_id.set(request_id);

                let (tx, rx) = async_channel::bounded::<SizeScanEvent>(64);
                runtime::spawn(async move {
                    let result =
                        calculate_package_size_details(&pkg, scan_token, tx.clone(), request_id)
                            .await;
                    let _ = tx
                        .send(SizeScanEvent::Finished { request_id, result })
                        .await;
                });

                let dsz = dsz.clone();
                let ds = ds.clone();
                let size_summary_row = size_summary_row.clone();
                let size_targets_row = size_targets_row.clone();
                let size_detail_list = size_detail_list.clone();
                let size_refresh_btn = size_refresh_btn.clone();
                let size_cancel_btn = size_cancel_btn.clone();
                let percent_mode_state = percent_mode_state.clone();
                let size_entries_state = size_entries_state.clone();
                let size_total_state = size_total_state.clone();
                let entry_view_state = entry_view_state.clone();
                let size_request_id = size_request_id.clone();
                let size_scan_token = size_scan_token.clone();
                glib::spawn_future_local(async move {
                    while let Ok(event) = rx.recv().await {
                        match event {
                            SizeScanEvent::Progress {
                                request_id: done_request_id,
                                progress,
                            } => {
                                if done_request_id != size_request_id.get() {
                                    continue;
                                }

                                let text = format_size_scan_progress(&progress, lang);
                                let safe = glib::markup_escape_text(&text);
                                size_summary_row.set_subtitle(&safe);
                                dsz.set_subtitle(&safe);
                            }
                            SizeScanEvent::Finished {
                                request_id: done_request_id,
                                result,
                            } => {
                                if done_request_id != size_request_id.get() {
                                    return;
                                }

                                let _ = size_scan_token.borrow_mut().take();

                                match result {
                                    Ok(scan) => {
                                        if scan.targets.is_empty() {
                                            size_targets_row.set_subtitle("-");
                                            size_targets_row.set_tooltip_text(None);
                                        } else {
                                            let subtitle = match lang {
                                                Language::ZhCn => {
                                                    format!("{} 个路径（悬停查看）", scan.targets.len())
                                                }
                                                Language::En => format!(
                                                    "{} path(s) (hover to view)",
                                                    scan.targets.len()
                                                ),
                                            };
                                            size_targets_row
                                                .set_subtitle(&glib::markup_escape_text(&subtitle));
                                            let tip = scan
                                                .targets
                                                .iter()
                                                .map(|t| {
                                                    if t.recursive {
                                                        format!("[R] {}", t.path)
                                                    } else {
                                                        format!("[F] {}", t.path)
                                                    }
                                                })
                                                .collect::<Vec<_>>()
                                                .join("\n");
                                            size_targets_row.set_tooltip_text(Some(&tip));
                                        }

                                        if scan.entries.is_empty() {
                                            if scan.permission_denied {
                                                size_summary_row.set_subtitle(pick(
                                                    lang,
                                                    "无权限读取统计目标",
                                                    "Permission denied",
                                                ));
                                                dsz.set_subtitle(pick(
                                                    lang,
                                                    "无权限读取统计目标（可调整权限后重试）",
                                                    "Permission denied (adjust permissions and retry)",
                                                ));
                                            } else {
                                                size_summary_row.set_subtitle(pick(
                                                    lang,
                                                    "未找到可统计文件",
                                                    "No measurable files",
                                                ));
                                                dsz.set_subtitle(pick(
                                                    lang,
                                                    "未找到可统计文件（可重试）",
                                                    "No measurable files",
                                                ));
                                            }
                                        } else {
                                            let summary = format_size(scan.total_size);
                                            let summary_text = if scan.truncated
                                                || scan.permission_denied
                                            {
                                                match lang {
                                                    Language::ZhCn => {
                                                        let mut reasons = Vec::new();
                                                        if scan.truncated {
                                                            reasons.push("文件过多");
                                                        }
                                                        if scan.permission_denied {
                                                            reasons.push("权限不足");
                                                        }
                                                        format!(
                                                            "{summary}（可能不完整：{}）",
                                                            reasons.join("、")
                                                        )
                                                    }
                                                    Language::En => {
                                                        let mut reasons = Vec::new();
                                                        if scan.truncated {
                                                            reasons.push("too many files");
                                                        }
                                                        if scan.permission_denied {
                                                            reasons.push("permission denied");
                                                        }
                                                        format!(
                                                            "{summary} (may be incomplete: {})",
                                                            reasons.join(", ")
                                                        )
                                                    }
                                                }
                                            } else {
                                                summary
                                            };
                                            size_summary_row.set_subtitle(&summary_text);
                                            dsz.set_subtitle(&summary_text);

                                            size_total_state.set(scan.total_size);
                                            *size_entries_state.borrow_mut() = scan.entries.clone();

                                            render_size_entries_incremental(
                                                size_detail_list.clone(),
                                                scan.entries,
                                                scan.total_size,
                                                percent_mode_state.get(),
                                                entry_view_state.get(),
                                                lang,
                                            );
                                        }
                                    }
                                    Err(err) if err == SIZE_SCAN_CANCELLED => {
                                        size_summary_row.set_subtitle(pick(
                                            lang,
                                            "已取消",
                                            "Cancelled",
                                        ));
                                        dsz.set_subtitle(pick(
                                            lang,
                                            "统计已取消",
                                            "Scan cancelled",
                                        ));
                                        size_targets_row.set_subtitle("-");
                                        size_targets_row.set_tooltip_text(None);
                                    }
                                    Err(_) => {
                                        size_summary_row
                                            .set_subtitle(pick(lang, "统计失败", "Failed"));
                                        dsz.set_subtitle(pick(
                                            lang,
                                            "统计失败（可重试）",
                                            "Failed (retry)",
                                        ));
                                        size_targets_row.set_subtitle("-");
                                        size_targets_row.set_tooltip_text(None);
                                    }
                                }

                                dsz.set_sensitive(true);
                                size_refresh_btn.set_sensitive(true);
                                size_cancel_btn.set_sensitive(false);
                                ds.set_visible_child_name("size-detail");
                                break;
                            }
                        }
                    }
                });
            }
        });

        size_back_btn.connect_clicked({
            let ds = ds.clone();
            move |_| {
                ds.set_visible_child_name("detail");
            }
        });

        size_refresh_btn.connect_clicked({
            let run_size_scan = run_size_scan.clone();
            move |_| run_size_scan()
        });

        percent_mode_dropdown.connect_selected_notify({
            let percent_mode_dropdown = percent_mode_dropdown.clone();
            let percent_mode_state = percent_mode_state.clone();
            let size_detail_list = size_detail_list.clone();
            let size_entries_state = size_entries_state.clone();
            let size_total_state = size_total_state.clone();
            let entry_view_state = entry_view_state.clone();
            move |_| {
                let mode = PercentageMode::from_index(percent_mode_dropdown.selected());
                percent_mode_state.set(mode);

                let entries = size_entries_state.borrow().clone();
                if entries.is_empty() {
                    return;
                }

                clear_list_box(&size_detail_list);
                render_size_entries_incremental(
                    size_detail_list.clone(),
                    entries,
                    size_total_state.get(),
                    mode,
                    entry_view_state.get(),
                    lang,
                );
            }
        });

        entry_view_dropdown.connect_selected_notify({
            let entry_view_dropdown = entry_view_dropdown.clone();
            let entry_view_state = entry_view_state.clone();
            let percent_mode_state = percent_mode_state.clone();
            let size_detail_list = size_detail_list.clone();
            let size_entries_state = size_entries_state.clone();
            let size_total_state = size_total_state.clone();
            move |_| {
                let view_mode = EntryViewMode::from_index(entry_view_dropdown.selected());
                entry_view_state.set(view_mode);

                let entries = size_entries_state.borrow().clone();
                if entries.is_empty() {
                    return;
                }

                clear_list_box(&size_detail_list);
                render_size_entries_incremental(
                    size_detail_list.clone(),
                    entries,
                    size_total_state.get(),
                    percent_mode_state.get(),
                    view_mode,
                    lang,
                );
            }
        });

        size_cancel_btn.connect_clicked({
            let size_request_id = size_request_id.clone();
            let size_scan_token = size_scan_token.clone();
            let size_summary_row = size_summary_row.clone();
            let size_targets_row = size_targets_row.clone();
            let dsz = dsz.clone();
            let size_cancel_btn = size_cancel_btn.clone();
            let size_refresh_btn = size_refresh_btn.clone();
            move |_| {
                if let Some(token) = size_scan_token.borrow_mut().take() {
                    token.cancel();
                }
                size_request_id.set(size_request_id.get().saturating_add(1));
                size_summary_row.set_subtitle(pick(lang, "已取消", "Cancelled"));
                dsz.set_subtitle(pick(lang, "统计已取消", "Scan cancelled"));
                size_targets_row.set_subtitle("-");
                size_targets_row.set_tooltip_text(None);
                dsz.set_sensitive(true);
                size_cancel_btn.set_sensitive(false);
                size_refresh_btn.set_sensitive(true);
            }
        });

        detail_size.connect_activated({
            let run_size_scan = run_size_scan.clone();
            move |_| run_size_scan()
        });

        let copy_uninstall_command: Rc<dyn Fn()> = Rc::new({
            let uninstall_command_target = uninstall_command_target.clone();
            move || {
                let Some(command) = uninstall_command_target.borrow().clone() else {
                    return;
                };
                let _ = copy_text_to_clipboard(&command);
            }
        });

        detail_uninstall.connect_activated({
            let copy_uninstall_command = copy_uninstall_command.clone();
            move |_| copy_uninstall_command()
        });
        uninstall_copy_btn.connect_clicked({
            let copy_uninstall_command = copy_uninstall_command.clone();
            move |_| copy_uninstall_command()
        });

        let open_path_target = Rc::new(RefCell::new(None::<String>));
        let path_click = gtk::GestureClick::new();
        {
            let open_path_target = open_path_target.clone();
            path_click.connect_pressed(move |_, n_press, _, _| {
                if n_press != 2 {
                    return;
                }

                let Some(path) = open_path_target.borrow().clone() else {
                    return;
                };

                if let Err(e) = open_path_in_file_manager(&path) {
                    tracing::warn!("failed to open install path '{}': {e}", path);
                }
            });
        }
        detail_path.add_controller(path_click);

        selection.connect_selection_changed(move |sel, _, _| {
            if let Some(token) = size_scan_token.borrow_mut().take() {
                token.cancel();
            }

            let item = sel.selected_item();
            match item.and_downcast::<glib::BoxedAnyObject>() {
                Some(obj) => {
                    let pkg: std::cell::Ref<Package> = obj.borrow();
                    let selected = SelectedPackage {
                        canonical_id: pkg.canonical_id.clone(),
                        source: pkg.source.clone(),
                        install_method: pkg.install_method.clone(),
                        install_path: pkg.install_path.clone(),
                        desktop_file: pkg.desktop_file.clone(),
                        uninstall_command: pkg.uninstall_command.clone(),
                    };
                    *selected_pkg.borrow_mut() = Some(selected.clone());
                    size_request_id.set(size_request_id.get().saturating_add(1));
                    dn.set_subtitle(&glib::markup_escape_text(&pkg.name));

                    let version_req = version_request_id.get().saturating_add(1);
                    version_request_id.set(version_req);
                    if pkg.version.is_empty() {
                        dv.set_subtitle(pick(lang, "检测中...", "Resolving..."));

                        let (tx, rx) = async_channel::bounded::<(u64, Option<String>)>(1);
                        runtime::spawn(async move {
                            let detected = resolve_display_version(&selected).await;
                            let _ = tx.send((version_req, detected)).await;
                        });

                        let dv = dv.clone();
                        let version_request_id = version_request_id.clone();
                        glib::spawn_future_local(async move {
                            if let Ok((done_id, version)) = rx.recv().await {
                                if done_id != version_request_id.get() {
                                    return;
                                }
                                dv.set_subtitle(&glib::markup_escape_text(
                                    version.as_deref().unwrap_or("-"),
                                ));
                            }
                        });
                    } else {
                        dv.set_subtitle(&glib::markup_escape_text(&pkg.version));
                    }

                    dso.set_subtitle(&glib::markup_escape_text(&pkg.source));
                    dim.set_subtitle(&glib::markup_escape_text(
                        if pkg.install_method.is_empty() {
                            "-"
                        } else {
                            &pkg.install_method
                        },
                    ));
                    dsz.set_subtitle(pick(lang, "点击进入大小明细", "Click for size details"));
                    dsz.set_sensitive(true);
                    size_refresh_btn.set_sensitive(true);
                    size_cancel_btn.set_sensitive(false);
                    percent_mode_dropdown.set_selected(0);
                    percent_mode_state.set(PercentageMode::RelativeMax);
                    entry_view_dropdown.set_selected(0);
                    entry_view_state.set(EntryViewMode::Folders);
                    size_total_state.set(0);
                    size_entries_state.borrow_mut().clear();
                    size_summary_row.set_subtitle("-");
                    size_targets_row.set_subtitle("-");
                    size_targets_row.set_tooltip_text(None);
                    clear_list_box(&size_detail_list);
                    dpath.set_subtitle(&glib::markup_escape_text(
                        pkg.install_path.as_deref().unwrap_or("-"),
                    ));
                    *open_path_target.borrow_mut() =
                        resolve_install_path_directory(pkg.install_path.as_deref());
                    if let Some(command) = pkg.uninstall_command.as_deref() {
                        duninstall.set_subtitle(&glib::markup_escape_text(command));
                        duninstall.set_sensitive(true);
                        uninstall_copy_btn.set_sensitive(true);
                        *uninstall_command_target.borrow_mut() = Some(command.to_string());
                    } else {
                        duninstall.set_subtitle("-");
                        duninstall.set_sensitive(false);
                        uninstall_copy_btn.set_sensitive(false);
                        *uninstall_command_target.borrow_mut() = None;
                    }
                    dd.set_subtitle(&glib::markup_escape_text(if pkg.description.is_empty() {
                        "-"
                    } else {
                        &pkg.description
                    }));
                    di.set_subtitle(&glib::markup_escape_text(&pkg.canonical_id));
                    ds.set_visible_child_name("detail");
                }
                None => {
                    *selected_pkg.borrow_mut() = None;
                    size_request_id.set(size_request_id.get().saturating_add(1));
                    version_request_id.set(version_request_id.get().saturating_add(1));
                    dv.set_subtitle("-");
                    dsz.set_subtitle("-");
                    dsz.set_sensitive(false);
                    size_refresh_btn.set_sensitive(false);
                    size_cancel_btn.set_sensitive(false);
                    percent_mode_dropdown.set_selected(0);
                    percent_mode_state.set(PercentageMode::RelativeMax);
                    entry_view_dropdown.set_selected(0);
                    entry_view_state.set(EntryViewMode::Folders);
                    size_total_state.set(0);
                    size_entries_state.borrow_mut().clear();
                    size_summary_row.set_subtitle("-");
                    size_targets_row.set_subtitle("-");
                    size_targets_row.set_tooltip_text(None);
                    clear_list_box(&size_detail_list);
                    *open_path_target.borrow_mut() = None;
                    duninstall.set_subtitle("-");
                    duninstall.set_sensitive(false);
                    uninstall_copy_btn.set_sensitive(false);
                    *uninstall_command_target.borrow_mut() = None;
                    ds.set_visible_child_name("empty");
                }
            }
        });
    }

    // Start discovery
    let (tx, rx) = async_channel::bounded::<discovery::DiscoveryEvent>(32);

    let token_clone = token.clone();
    runtime::spawn(async move {
        discovery::discover_all(tx, token_clone).await;
    });

    let store_clone = list_store.clone();
    let stack_clone = stack.clone();
    let banner_clone = banner.clone();
    glib::spawn_future_local(async move {
        let mut total = 0u32;
        let mut all_warnings = Vec::new();
        let queue = Rc::new(RefCell::new(VecDeque::<Package>::new()));
        let idle_running = Rc::new(Cell::new(false));

        while let Ok(event) = rx.recv().await {
            {
                let mut q = queue.borrow_mut();
                for pkg in event.packages {
                    q.push_back(pkg);
                    total += 1;
                }
            }
            all_warnings.extend(event.warnings);

            if total > 0 {
                stack_clone.set_visible_child_name("list");
            }

            if !idle_running.get() {
                idle_running.set(true);

                let store_for_tick = store_clone.clone();
                let stack_for_tick = stack_clone.clone();
                let queue_for_tick = queue.clone();
                let idle_running_for_tick = idle_running.clone();

                glib::idle_add_local(move || {
                    const CHUNK_SIZE: usize = 200;

                    let mut q = queue_for_tick.borrow_mut();
                    let mut appended = 0usize;
                    for _ in 0..CHUNK_SIZE {
                        let Some(pkg) = q.pop_front() else {
                            break;
                        };
                        store_for_tick.append(&glib::BoxedAnyObject::new(pkg));
                        appended += 1;
                    }

                    let done = q.is_empty();
                    drop(q);

                    if appended > 0 {
                        stack_for_tick.set_visible_child_name("list");
                    }

                    if done {
                        idle_running_for_tick.set(false);
                        glib::ControlFlow::Break
                    } else {
                        glib::ControlFlow::Continue
                    }
                });
            }
        }

        if total == 0 {
            stack_clone.set_visible_child_name("empty");
        }

        if !all_warnings.is_empty() {
            let title = match lang {
                Language::ZhCn => format!("扫描过程中出现 {} 条告警", all_warnings.len()),
                Language::En => format!("{} warning(s) during scan", all_warnings.len()),
            };
            banner_clone.set_title(&title);
            banner_clone.set_revealed(true);
        }
    });

    search_entry.connect_search_changed({
        let query_state = query_state.clone();
        let filter = filter.clone();
        move |entry| {
            *query_state.borrow_mut() = entry.text().to_string().to_lowercase();
            filter.changed(gtk::FilterChange::Different);
        }
    });

    system_packages_toggle.connect_toggled({
        let show_system_state = show_system_state.clone();
        let filter = filter.clone();
        move |btn| {
            let expanded = btn.is_active();
            show_system_state.set(expanded);
            btn.set_label(system_packages_toggle_label(lang, expanded));
            filter.changed(gtk::FilterChange::Different);
        }
    });

    adw::NavigationPage::builder()
        .title(pick(lang, "软件全景", "Software Panorama"))
        .child(&vbox)
        .build()
}

fn format_size(bytes: u64) -> String {
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
        format!("{bytes} B")
    }
}

fn format_duration_ms(ms: u64) -> String {
    let total_secs = ms / 1000;
    let secs = total_secs % 60;
    let mins = (total_secs / 60) % 60;
    let hours = total_secs / 3600;
    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins}:{secs:02}")
    }
}

fn format_size_scan_progress(progress: &SizeScanProgress, lang: Language) -> String {
    let elapsed = format_duration_ms(progress.elapsed_ms);
    let eta = progress
        .eta_ms
        .filter(|v| *v > 0)
        .map(format_duration_ms);

    match progress.phase {
        SizeScanPhase::PreparingTargets => match lang {
            Language::ZhCn => format!("准备统计目标... · 已耗时 {elapsed}"),
            Language::En => format!("Preparing targets... · Elapsed {elapsed}"),
        },
        SizeScanPhase::ScanningTargets => {
            let count = if progress.targets_total > 0 {
                format!("{}/{}", progress.targets_done, progress.targets_total)
            } else {
                "-".to_string()
            };

            let current_part = progress
                .current_path
                .as_deref()
                .filter(|v| !v.is_empty())
                .map(|v| match lang {
                    Language::ZhCn => format!(" · 当前 {v}"),
                    Language::En => format!(" · Current {v}"),
                })
                .unwrap_or_default();

            let files_part = if progress.scanned_files > 0 {
                match lang {
                    Language::ZhCn => format!(" · 已扫描 {} 文件", progress.scanned_files),
                    Language::En => format!(" · {} files", progress.scanned_files),
                }
            } else {
                String::new()
            };

            let eta_part = eta
                .as_deref()
                .map(|v| match lang {
                    Language::ZhCn => format!(" · 预计剩余 {v}"),
                    Language::En => format!(" · ETA {v}"),
                })
                .unwrap_or_default();

            match lang {
                Language::ZhCn => {
                    format!("已完成 {count}{current_part}{files_part} · 已耗时 {elapsed}{eta_part}")
                }
                Language::En => {
                    format!("Done {count}{current_part}{files_part} · Elapsed {elapsed}{eta_part}")
                }
            }
        }
    }
}

fn is_system_package(pkg: &Package) -> bool {
    pkg.install_method == "apt" && pkg.desktop_file.is_none()
}

fn system_packages_toggle_label(lang: Language, expanded: bool) -> &'static str {
    match (lang, expanded) {
        (Language::ZhCn, false) => "展开系统自带软件",
        (Language::ZhCn, true) => "折叠系统自带软件",
        (Language::En, false) => "Expand system packages",
        (Language::En, true) => "Collapse system packages",
    }
}

fn copy_text_to_clipboard(text: &str) -> bool {
    let Some(display) = gtk::gdk::Display::default() else {
        return false;
    };

    display.clipboard().set_text(text);
    true
}

async fn calculate_package_size_details(
    pkg: &SelectedPackage,
    token: CancellationToken,
    progress_tx: async_channel::Sender<SizeScanEvent>,
    request_id: u64,
) -> Result<SizeScanResult, String> {
    let started = Instant::now();

    let _ = progress_tx.try_send(SizeScanEvent::Progress {
        request_id,
        progress: SizeScanProgress {
            phase: SizeScanPhase::PreparingTargets,
            current_path: None,
            targets_done: 0,
            targets_total: 0,
            scanned_files: 0,
            elapsed_ms: 0,
            eta_ms: None,
        },
    });

    let targets = token
        .run_until_cancelled(collect_candidate_paths(pkg))
        .await
        .ok_or_else(|| SIZE_SCAN_CANCELLED.to_string())?;
    if targets.is_empty() {
        return Ok(SizeScanResult {
            total_size: 0,
            entries: Vec::new(),
            targets: Vec::new(),
            truncated: false,
            permission_denied: false,
        });
    }

    let mut visited = BTreeSet::<String>::new();
    let mut effective_targets = Vec::<SizePathCandidate>::new();
    for target in targets {
        let key = normalize_path_key(&target.path);
        if visited.insert(key) {
            effective_targets.push(target);
        }
    }

    if effective_targets.is_empty() {
        return Ok(SizeScanResult {
            total_size: 0,
            entries: Vec::new(),
            targets: Vec::new(),
            truncated: false,
            permission_denied: false,
        });
    }

    let targets_total = effective_targets.len();
    let mut all_files: HashMap<String, FileRecord> = HashMap::new();
    let mut dir_seen_files: HashMap<String, HashSet<String>> = HashMap::new();
    let mut truncated = false;
    let mut permission_denied = false;
    let mut completed_target_ms_total = 0_u64;
    let mut completed_target_count = 0_u32;

    for (idx, target) in effective_targets.iter().cloned().enumerate() {
        if token.is_cancelled() {
            return Err(SIZE_SCAN_CANCELLED.to_string());
        }

        let avg_ms = if completed_target_count == 0 {
            None
        } else {
            Some(completed_target_ms_total / u64::from(completed_target_count))
        };
        let eta_ms = avg_ms.map(|avg| {
            avg.saturating_mul(
                u64::try_from(targets_total.saturating_sub(idx)).unwrap_or(u64::MAX),
            )
        });

        let _ = progress_tx.try_send(SizeScanEvent::Progress {
            request_id,
            progress: SizeScanProgress {
                phase: SizeScanPhase::ScanningTargets,
                current_path: Some(target.path.clone()),
                targets_done: idx,
                targets_total,
                scanned_files: 0,
                elapsed_ms: started.elapsed().as_millis() as u64,
                eta_ms,
            },
        });

        let path = target.path;
        let recursive = target.recursive;

        let scan_started = Instant::now();
        let token_for_worker = token.clone();
        let tx_for_progress = progress_tx.clone();
        let request_id_for_progress = request_id;
        let started_for_progress = started;
        let idx_for_progress = idx;
        let targets_total_for_progress = targets_total;
        let avg_ms_for_progress = avg_ms;
        let scanned = tokio::task::spawn_blocking(move || {
            collect_target_files(&path, recursive, &token_for_worker, |file_count| {
                let eta_ms = avg_ms_for_progress.map(|avg| {
                    avg.saturating_mul(u64::try_from(
                        targets_total_for_progress.saturating_sub(idx_for_progress),
                    )
                    .unwrap_or(u64::MAX))
                });

                let _ = tx_for_progress.try_send(SizeScanEvent::Progress {
                    request_id: request_id_for_progress,
                    progress: SizeScanProgress {
                        phase: SizeScanPhase::ScanningTargets,
                        current_path: Some(path.clone()),
                        targets_done: idx_for_progress,
                        targets_total: targets_total_for_progress,
                        scanned_files: file_count,
                        elapsed_ms: started_for_progress.elapsed().as_millis() as u64,
                        eta_ms,
                    },
                });
            })
        })
        .await
        .map_err(|e| format!("scan worker failed: {e}"))??;

        truncated = truncated || scanned.truncated;
        permission_denied = permission_denied || scanned.permission_denied;

        for (file_path, record) in scanned.files {
            all_files
                .entry(file_path.clone())
                .and_modify(|existing| {
                    if record.size > existing.size {
                        *existing = record;
                    }
                })
                .or_insert(record);

            let parent = parent_dir_of(&file_path).unwrap_or_else(|| "/".to_string());
            dir_seen_files.entry(parent).or_default().insert(file_path);
        }

        completed_target_ms_total =
            completed_target_ms_total.saturating_add(scan_started.elapsed().as_millis() as u64);
        completed_target_count = completed_target_count.saturating_add(1);

        let avg_ms = Some(completed_target_ms_total / u64::from(completed_target_count));
        let remaining = targets_total.saturating_sub(idx.saturating_add(1));
        let eta_ms = avg_ms.map(|avg| avg.saturating_mul(u64::try_from(remaining).unwrap_or(0)));

        let _ = progress_tx.try_send(SizeScanEvent::Progress {
            request_id,
            progress: SizeScanProgress {
                phase: SizeScanPhase::ScanningTargets,
                current_path: None,
                targets_done: idx.saturating_add(1),
                targets_total,
                scanned_files: 0,
                elapsed_ms: started.elapsed().as_millis() as u64,
                eta_ms,
            },
        });
    }

    let mut entries = Vec::<SizeEntry>::new();

    for (dir, files) in &dir_seen_files {
        let mut size = 0_u64;
        for file in files {
            if let Some(record) = all_files.get(file) {
                size = size.saturating_add(record.size);
            }
        }

        if size > 0 {
            entries.push(SizeEntry {
                path: dir.clone(),
                size,
                is_dir: true,
            });
        }
    }

    for (path, record) in &all_files {
        entries.push(SizeEntry {
            path: path.clone(),
            size: record.size,
            is_dir: false,
        });
    }

    entries.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));

    let mut unique_inodes = HashSet::<(u64, u64)>::new();
    let mut unique_paths = HashSet::<String>::new();
    let mut total_size = 0_u64;
    for (path, record) in &all_files {
        if record.dev != 0 && record.ino != 0 {
            if unique_inodes.insert((record.dev, record.ino)) {
                total_size = total_size.saturating_add(record.size);
            }
            continue;
        }
        if unique_paths.insert(path.clone()) {
            total_size = total_size.saturating_add(record.size);
        }
    }

    Ok(SizeScanResult {
        total_size,
        entries,
        targets: effective_targets,
        truncated,
        permission_denied,
    })
}

fn render_size_entries_incremental(
    list: gtk::Box,
    entries: Vec<SizeEntry>,
    total_size: u64,
    mode: PercentageMode,
    view_mode: EntryViewMode,
    lang: Language,
) {
    const MAX_RENDER_ENTRIES: usize = 1500;
    const CHUNK_SIZE: usize = 120;

    let filtered_entries: Vec<SizeEntry> = entries
        .into_iter()
        .filter(|entry| match view_mode {
            EntryViewMode::Folders => entry.is_dir,
            EntryViewMode::Files => !entry.is_dir,
        })
        .collect();

    if filtered_entries.is_empty() {
        let empty = gtk::Label::new(Some(match (lang, view_mode) {
            (Language::ZhCn, EntryViewMode::Folders) => "当前结果没有可展示的文件夹项",
            (Language::ZhCn, EntryViewMode::Files) => "当前结果没有可展示的文件项",
            (Language::En, EntryViewMode::Folders) => "No folder entries for current result",
            (Language::En, EntryViewMode::Files) => "No file entries for current result",
        }));
        empty.set_halign(gtk::Align::Start);
        empty.add_css_class("dim-label");
        empty.set_margin_top(8);
        list.append(&empty);
        return;
    }

    let max_size = filtered_entries.first().map_or(0_u64, |entry| entry.size);
    let omitted = filtered_entries.len().saturating_sub(MAX_RENDER_ENTRIES);
    let queue = Rc::new(RefCell::new(
        filtered_entries
            .into_iter()
            .take(MAX_RENDER_ENTRIES)
            .collect::<VecDeque<_>>(),
    ));

    let list_for_tick = list.clone();
    let queue_for_tick = queue.clone();
    let max_size_for_tick = max_size;
    let total_size_for_tick = total_size;
    let mode_for_tick = mode;
    glib::idle_add_local(move || {
        let mut q = queue_for_tick.borrow_mut();
        for _ in 0..CHUNK_SIZE {
            let Some(entry) = q.pop_front() else {
                drop(q);
                if omitted > 0 {
                    let note = gtk::Label::new(Some(&match lang {
                        Language::ZhCn => format!("为保证界面流畅，已省略 {} 条明细", omitted),
                        Language::En => {
                            format!("Omitted {omitted} entries for UI responsiveness")
                        }
                    }));
                    note.set_halign(gtk::Align::Start);
                    note.add_css_class("dim-label");
                    note.set_margin_top(6);
                    list_for_tick.append(&note);
                }
                return glib::ControlFlow::Break;
            };
            append_size_entry_row(
                &list_for_tick,
                &entry,
                max_size_for_tick,
                total_size_for_tick,
                mode_for_tick,
                lang,
            );
        }
        glib::ControlFlow::Continue
    });
}

struct TargetFiles {
    files: HashMap<String, FileRecord>,
    truncated: bool,
    permission_denied: bool,
}

fn collect_target_files(
    path: &str,
    recursive: bool,
    token: &CancellationToken,
    mut on_progress: impl FnMut(usize),
) -> Result<TargetFiles, String> {
    const MAX_FILES_PER_TARGET: usize = 200_000;

    if token.is_cancelled() {
        return Err(SIZE_SCAN_CANCELLED.to_string());
    }

    let mut files = HashMap::new();
    let mut truncated = false;
    let mut permission_denied = false;
    let mut last_emit = Instant::now();
    on_progress(0);
    let p = Path::new(path);
    let meta = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                permission_denied = true;
            }
            on_progress(0);
            return Ok(TargetFiles {
                files,
                truncated,
                permission_denied,
            });
        }
    };

    if meta.is_file() {
        if let Some(record) = metadata_to_record(&meta) {
            files.insert(path.to_string(), record);
        }
        on_progress(files.len());
        return Ok(TargetFiles {
            files,
            truncated,
            permission_denied,
        });
    }

    if !meta.is_dir() || !recursive {
        on_progress(files.len());
        return Ok(TargetFiles {
            files,
            truncated,
            permission_denied,
        });
    }

    for entry in walkdir::WalkDir::new(p).follow_links(false).into_iter() {
        if token.is_cancelled() {
            return Err(SIZE_SCAN_CANCELLED.to_string());
        }

        let entry = match entry {
            Ok(v) => v,
            Err(err) => {
                if err
                    .io_error()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
                {
                    permission_denied = true;
                }
                continue;
            }
        };

        if files.len() >= MAX_FILES_PER_TARGET {
            truncated = true;
            break;
        }

        let file_meta = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                if err
                    .io_error()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
                {
                    permission_denied = true;
                }
                continue;
            }
        };
        if !file_meta.is_file() {
            continue;
        }

        let file_path = entry.path().to_string_lossy().into_owned();
        if let Some(record) = metadata_to_record(&file_meta) {
            files
                .entry(file_path)
                .and_modify(|existing| {
                    if record.size > existing.size {
                        *existing = record;
                    }
                })
                .or_insert(record);
        }

        if last_emit.elapsed() >= Duration::from_millis(450) {
            on_progress(files.len());
            last_emit = Instant::now();
        }
    }

    on_progress(files.len());
    Ok(TargetFiles {
        files,
        truncated,
        permission_denied,
    })
}

fn metadata_to_record(meta: &std::fs::Metadata) -> Option<FileRecord> {
    #[cfg(unix)]
    {
        let allocated = meta.blocks().saturating_mul(512);
        let size = if allocated > 0 { allocated } else { meta.len() };
        Some(FileRecord {
            dev: meta.dev(),
            ino: meta.ino(),
            size,
        })
    }

    #[cfg(not(unix))]
    {
        if meta.is_file() {
            Some(FileRecord {
                dev: 0,
                ino: 0,
                size: meta.len(),
            })
        } else {
            None
        }
    }
}

fn parent_dir_of(path: &str) -> Option<String> {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
}

fn normalize_loose_key(text: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in text.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn xdg_dir(env_key: &str, fallback_suffix: &str) -> Option<PathBuf> {
    if let Ok(raw) = std::env::var(env_key) {
        let trimmed = raw.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }

    let Ok(home) = std::env::var("HOME") else {
        return None;
    };
    let home = home.trim().trim_end_matches('/');
    if home.is_empty() {
        return None;
    }
    Some(Path::new(home).join(fallback_suffix))
}

fn is_dot_dir_key_too_generic(key: &str) -> bool {
    let k = normalize_loose_key(key);
    matches!(
        k.as_str(),
        "cache" | "config" | "local" | "share" | "state" | "tmp" | "temp" | "downloads" | "desktop"
    )
}

fn find_matching_child_dirs(base: &Path, key: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if key.trim().is_empty() {
        return out;
    }

    let direct = base.join(key);
    if direct.is_dir() {
        out.push(direct);
    }

    let base_iter = match std::fs::read_dir(base) {
        Ok(v) => v,
        Err(_) => return out,
    };

    let want = normalize_loose_key(key);
    if want.is_empty() {
        return out;
    }

    for entry in base_iter.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.is_empty() {
            continue;
        }
        if normalize_loose_key(&name) != want {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }

        let path = entry.path();
        if !out.iter().any(|p| p == &path) {
            out.push(path);
        }
    }

    out
}

fn app_key_candidates(pkg: &SelectedPackage) -> Vec<String> {
    let mut keys = BTreeSet::<String>::new();

    if let Some(name) = canonical_name(&pkg.canonical_id) {
        keys.insert(name);
    }

    if let Some(desktop_file) = pkg.desktop_file.as_deref() {
        if let Some(stem) = Path::new(desktop_file)
            .file_stem()
            .and_then(|v| v.to_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            keys.insert(stem.to_string());
        }
    }

    if let Some(path) = pkg.install_path.as_deref() {
        let p = Path::new(path);
        if let Some(name) = p
            .file_name()
            .and_then(|v| v.to_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            keys.insert(name.to_string());
        }
        if let Some(stem) = p
            .file_stem()
            .and_then(|v| v.to_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            keys.insert(stem.to_string());
        }
    }

    keys.into_iter().collect()
}

fn collect_user_data_targets(pkg: &SelectedPackage) -> Vec<SizePathCandidate> {
    let keys = app_key_candidates(pkg);
    if keys.is_empty() {
        return Vec::new();
    }

    let config_home = xdg_dir("XDG_CONFIG_HOME", ".config");
    let data_home = xdg_dir("XDG_DATA_HOME", ".local/share");
    let cache_home = xdg_dir("XDG_CACHE_HOME", ".cache");
    let state_home = xdg_dir("XDG_STATE_HOME", ".local/state");

    let mut found = BTreeSet::<String>::new();
    let mut targets = Vec::<SizePathCandidate>::new();

    for key in &keys {
        for base in [
            config_home.as_deref(),
            data_home.as_deref(),
            cache_home.as_deref(),
            state_home.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            for dir in find_matching_child_dirs(base, key) {
                let path = dir.to_string_lossy().trim_end_matches('/').to_string();
                if path.starts_with('/') && found.insert(path.clone()) {
                    targets.push(SizePathCandidate {
                        path,
                        recursive: true,
                    });
                }
            }
        }
    }

    if let Ok(home) = std::env::var("HOME") {
        let home = home.trim().trim_end_matches('/');
        if !home.is_empty() {
            let home_p = Path::new(home);
            for key in &keys {
                if is_dot_dir_key_too_generic(key) {
                    continue;
                }
                let dot = format!(".{key}");
                for dir in find_matching_child_dirs(home_p, &dot) {
                    let path = dir.to_string_lossy().trim_end_matches('/').to_string();
                    if path.starts_with('/') && found.insert(path.clone()) {
                        targets.push(SizePathCandidate {
                            path,
                            recursive: true,
                        });
                    }
                }
            }
        }
    }

    targets
}

fn collect_var_data_targets(pkg: &SelectedPackage) -> Vec<SizePathCandidate> {
    if pkg.source != "apt" && pkg.source != "desktop" {
        return Vec::new();
    }

    let keys = app_key_candidates(pkg);
    if keys.is_empty() {
        return Vec::new();
    }

    let bases = ["/var/lib", "/var/cache", "/var/log"];
    let mut found = BTreeSet::<String>::new();
    let mut targets = Vec::<SizePathCandidate>::new();

    for base in bases {
        let base_p = Path::new(base);
        for key in &keys {
            for dir in find_matching_child_dirs(base_p, key) {
                let path = dir.to_string_lossy().trim_end_matches('/').to_string();
                if path.starts_with('/') && found.insert(path.clone()) {
                    targets.push(SizePathCandidate {
                        path,
                        recursive: true,
                    });
                }
            }
        }
    }

    targets
}

fn desktop_workdir_from_file(desktop_file: &str) -> Option<String> {
    let path = Path::new(desktop_file);
    let de = DesktopEntry::from_path(path, None::<&[&str]>).ok()?;
    let raw = de.path()?.trim();
    if !raw.starts_with('/') {
        return None;
    }
    let normalized = raw.trim_end_matches('/');
    if normalized.is_empty() {
        return None;
    }
    let p = Path::new(normalized);
    if !p.exists() || !p.is_dir() {
        return None;
    }

    // 避免把整个主目录当作“安装路径”去递归统计，风险太大且容易误触。
    if let Ok(home) = std::env::var("HOME") {
        let home = home.trim_end_matches('/');
        if !home.is_empty() && normalized == home {
            return None;
        }
    }

    if normalized == "/" || normalized == "/usr" {
        return None;
    }

    Some(normalized.to_string())
}

fn extract_env_assignment(exec_raw: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    exec_raw
        .split_whitespace()
        .map(|token| token.trim_matches('"').trim_matches('\''))
        .find_map(|token| token.strip_prefix(&prefix).map(ToString::to_string))
        .map(|v| v.trim_matches('"').trim_matches('\'').to_string())
}

fn wine_prefix_from_desktop_file(desktop_file: &str) -> Option<String> {
    let path = Path::new(desktop_file);
    let de = DesktopEntry::from_path(path, None::<&[&str]>).ok()?;
    let exec_raw = de.exec().unwrap_or_default();
    let prefix = extract_env_assignment(exec_raw, "WINEPREFIX")?;
    if !prefix.starts_with('/') {
        return None;
    }
    let normalized = prefix.trim_end_matches('/').to_string();
    let p = Path::new(&normalized);
    if p.exists() && p.is_dir() {
        Some(normalized)
    } else {
        None
    }
}

fn wine_prefix_from_pkg(pkg: &SelectedPackage) -> Option<String> {
    if let Some(desktop_file) = pkg.desktop_file.as_deref() {
        if let Some(prefix) = wine_prefix_from_desktop_file(desktop_file) {
            return Some(prefix);
        }
    }

    if let Some(cmd) = pkg.uninstall_command.as_deref() {
        if let Some(prefix) = extract_env_assignment(cmd, "WINEPREFIX") {
            if prefix.starts_with('/') {
                let normalized = prefix.trim_end_matches('/').to_string();
                let p = Path::new(&normalized);
                if p.exists() && p.is_dir() {
                    return Some(normalized);
                }
            }
        }
    }

    let Ok(home) = std::env::var("HOME") else {
        return None;
    };
    let candidate = format!("{}/.wine", home.trim_end_matches('/'));
    let p = Path::new(&candidate);
    if p.exists() && p.is_dir() {
        Some(candidate)
    } else {
        None
    }
}

fn conda_root_from_install_path(path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return None;
    }

    let markers = ["anaconda3", "miniconda3", "mambaforge", "miniforge3"];
    let mut root: Option<String> = None;
    for marker in markers {
        let needle = format!("/{marker}/");
        if let Some(pos) = path.find(&needle) {
            root = Some(format!("{}{}", &path[..pos], format!("/{marker}")));
            break;
        }
        let suffix = format!("/{marker}");
        if path.ends_with(&suffix) {
            root = Some(path.to_string());
            break;
        }
    }

    let mut root = root?;

    // 若命中 envs/<name>，优先统计该环境目录，而不是整个 base。
    if let Some(envs_pos) = path.find("/envs/") {
        if envs_pos > 0 {
            let envs_rest = &path[envs_pos + "/envs/".len()..];
            let env_name = envs_rest.split('/').next().unwrap_or("").trim();
            if !env_name.is_empty() {
                root = format!("{root}/envs/{env_name}");
            }
        }
    }

    let normalized = root.trim_end_matches('/').to_string();
    let p = Path::new(&normalized);
    if !p.exists() || !p.is_dir() {
        return None;
    }

    // 避免误把整个主目录当作 conda 根目录。
    if let Ok(home) = std::env::var("HOME") {
        let home = home.trim_end_matches('/');
        if !home.is_empty() && normalized == home {
            return None;
        }
    }

    Some(normalized)
}

fn bundle_root_from_install_path(path: &str) -> Option<String> {
    let p = Path::new(path);
    if !path.starts_with('/') {
        return None;
    }

    let mut current = p.parent()?;
    while let Some(parent) = current.parent() {
        let Some(name) = current.file_name().and_then(|v| v.to_str()) else {
            current = parent;
            continue;
        };
        if name != "bin" {
            current = parent;
            continue;
        }

        let root = parent;
        let root_str = root.to_string_lossy().trim_end_matches('/').to_string();
        if root_str.is_empty() || root_str == "/" || root_str == "/usr" {
            return None;
        }

        if let Ok(home) = std::env::var("HOME") {
            let home = home.trim_end_matches('/');
            if !home.is_empty() && root_str == home {
                return None;
            }
        }

        if !root.exists() || !root.is_dir() {
            return None;
        }

        let has_hint = root.join("lib").is_dir()
            || root.join("lib64").is_dir()
            || root.join("share").is_dir()
            || root.join("resources").is_dir()
            || root.join("conda-meta").exists();
        if has_hint {
            return Some(root_str);
        }
        return None;
    }

    None
}

async fn collect_candidate_paths(pkg: &SelectedPackage) -> Vec<SizePathCandidate> {
    let desktop_targets = |targets: &mut Vec<SizePathCandidate>| {
        let Some(desktop_file) = pkg.desktop_file.as_deref() else {
            return;
        };

        if let Some(workdir) = desktop_workdir_from_file(desktop_file) {
            targets.push(SizePathCandidate {
                path: workdir,
                recursive: true,
            });
        }

        targets.push(SizePathCandidate {
            path: desktop_file.to_string(),
            recursive: false,
        });
    };

    // 1) 优先使用 dpkg 精准枚举（仅在可用时）。
    if pkg.source == "apt" && command_exists("dpkg-query") {
        let (_, name) = parse_canonical_id(&pkg.canonical_id);
        let mut targets = collect_dpkg_package_paths(name).await;
        if !targets.is_empty() {
            targets.extend(collect_var_data_targets(pkg));
            targets.extend(collect_user_data_targets(pkg));
            desktop_targets(&mut targets);
            return targets;
        }
    }

    if command_exists("dpkg-query") {
        if let Some(path) = pkg.install_path.as_deref() {
            if let Some(owner_pkg) = resolve_dpkg_owner_from_path(path).await {
                let mut targets = collect_dpkg_package_paths(&owner_pkg).await;
                if !targets.is_empty() {
                    targets.extend(collect_var_data_targets(pkg));
                    targets.extend(collect_user_data_targets(pkg));
                    desktop_targets(&mut targets);
                    return targets;
                }
            }
        }
    }

    // 2) 其次按来源补充更“贴近真实占用”的路径（即使相关命令缺失，也会回退到通用路径）。
    let mut targets = Vec::new();
    if pkg.source == "flatpak" && command_exists("flatpak") {
        if let Some(app_id) = canonical_name(&pkg.canonical_id) {
            if let Ok(output) =
                run_command("flatpak", &["info", "--show-location", &app_id], 20).await
            {
                let location = output.stdout.trim();
                if location.starts_with('/') {
                    targets.push(SizePathCandidate {
                        path: location.to_string(),
                        recursive: true,
                    });
                }
            }

            if let Ok(output) =
                run_command("flatpak", &["info", "--show-runtime", &app_id], 20).await
            {
                let runtime = output.stdout.trim();
                if !runtime.is_empty() {
                    targets.push(SizePathCandidate {
                        path: format!("/var/lib/flatpak/runtime/{runtime}"),
                        recursive: true,
                    });
                }
            }

            if let Ok(home) = std::env::var("HOME") {
                let user_data = format!(
                    "{}/.var/app/{app_id}",
                    home.trim_end_matches('/')
                );
                let p = Path::new(&user_data);
                if p.exists() && p.is_dir() {
                    targets.push(SizePathCandidate {
                        path: user_data,
                        recursive: true,
                    });
                }
            }
        }
    }

    if pkg.source == "snap" && command_exists("snap") {
        if let Some(name) = canonical_name(&pkg.canonical_id) {
            if let Ok(output) = run_command("snap", &["info", &name], 20).await {
                if let Some(installed_size) = parse_snap_installed_size(&output.stdout) {
                    targets.push(SizePathCandidate {
                        path: format!("/snap/{name}"),
                        recursive: true,
                    });
                    if installed_size > 0 {
                        targets.push(SizePathCandidate {
                            path: format!("/var/snap/{name}"),
                            recursive: true,
                        });
                    }
                }
            }

            if let Ok(home) = std::env::var("HOME") {
                let user_data = format!("{}/snap/{name}", home.trim_end_matches('/'));
                let p = Path::new(&user_data);
                if p.exists() && p.is_dir() {
                    targets.push(SizePathCandidate {
                        path: user_data,
                        recursive: true,
                    });
                }
            }
        }
    }

    if pkg.install_method == "wine" {
        if let Some(prefix) = wine_prefix_from_pkg(pkg) {
            targets.push(SizePathCandidate {
                path: prefix,
                recursive: true,
            });
        }
    }

    // 3) 通用回退：安装路径 + 常见扩展目录 + desktop 文件（避免出现“无可统计项”）。
    if let Some(path) = pkg.install_path.as_deref() {
        if pkg.install_method == "conda" {
            if let Some(root) = conda_root_from_install_path(path) {
                targets.push(SizePathCandidate {
                    path: root,
                    recursive: true,
                });
            }
        }

        if let Some(root) = bundle_root_from_install_path(path) {
            targets.push(SizePathCandidate {
                path: root,
                recursive: true,
            });
        }

        let trimmed = path.trim_end_matches('/');
        let broad_dir = trimmed == "/" || trimmed == "/usr";
        let recursive = if broad_dir {
            false
        } else {
            std::fs::metadata(trimmed).map_or(true, |m| m.is_dir())
        };
        targets.push(SizePathCandidate {
            path: path.to_string(),
            recursive,
        });

        if let Some(opt_root) = extract_opt_root(path) {
            targets.push(SizePathCandidate {
                path: opt_root,
                recursive: true,
            });
        }

        if let Some(name) = Path::new(path)
            .file_stem()
            .and_then(|v| v.to_str())
            .filter(|v| !v.is_empty())
        {
            for extra in [
                format!("/usr/lib/{name}"),
                format!("/usr/share/{name}"),
                format!("/usr/local/lib/{name}"),
                format!("/usr/local/share/{name}"),
            ] {
                targets.push(SizePathCandidate {
                    path: extra,
                    recursive: true,
                });
            }
        }
    }

    targets.extend(collect_var_data_targets(pkg));
    targets.extend(collect_user_data_targets(pkg));

    desktop_targets(&mut targets);

    targets
}

async fn collect_dpkg_package_paths(package_name: &str) -> Vec<SizePathCandidate> {
    let mut targets = Vec::new();
    if let Ok(output) = run_command("dpkg-query", &["-L", package_name], 20).await {
        for line in output.stdout.lines() {
            let path = line.trim();
            if path.starts_with('/') {
                targets.push(SizePathCandidate {
                    path: path.to_string(),
                    recursive: false,
                });
            }
        }
    }
    targets
}

async fn resolve_dpkg_owner_from_path(path: &str) -> Option<String> {
    let mut candidates = vec![path.to_string()];
    if let Ok(real) = std::fs::canonicalize(path) {
        let resolved = real.to_string_lossy().into_owned();
        if resolved.starts_with('/') && resolved != path {
            candidates.push(resolved);
        }
    }

    for candidate in candidates {
        if let Ok(output) = run_command("dpkg-query", &["-S", &candidate], 10).await {
            if let Some(owner) = parse_dpkg_owner_output(&output.stdout) {
                return Some(owner);
            }
        }
    }
    None
}

fn parse_dpkg_owner_output(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("dpkg-query:") {
            continue;
        }
        let (left, _) = trimmed.split_once(':')?;
        let package = left.split(',').next()?.trim();
        if !package.is_empty() {
            return Some(package.to_string());
        }
    }
    None
}

fn parse_dpkg_version_output(stdout: &str) -> Option<String> {
    let version = stdout.lines().next()?.trim();
    if version.is_empty() || version == "(none)" {
        None
    } else {
        Some(version.to_string())
    }
}

async fn resolve_display_version(pkg: &SelectedPackage) -> Option<String> {
    if pkg.source == "apt" {
        let (_, name) = parse_canonical_id(&pkg.canonical_id);
        return run_command("dpkg-query", &["-W", "-f=${Version}", name], 8)
            .await
            .ok()
            .and_then(|output| parse_dpkg_version_output(&output.stdout));
    }

    if let Some(path) = pkg.install_path.as_deref() {
        if let Some(owner) = resolve_dpkg_owner_from_path(path).await {
            let detected = run_command("dpkg-query", &["-W", "-f=${Version}", &owner], 8)
                .await
                .ok()
                .and_then(|output| parse_dpkg_version_output(&output.stdout));
            if let Some(v) = detected {
                return Some(v);
            }
        }
    }

    None
}

fn canonical_name(canonical_id: &str) -> Option<String> {
    let (_, name) = parse_canonical_id(canonical_id);
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn parse_snap_installed_size(text: &str) -> Option<u64> {
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.to_ascii_lowercase().starts_with("installed:") {
            continue;
        }
        let value = trimmed
            .split_once(':')
            .map(|(_, right)| right.trim())
            .unwrap_or("");
        if value.is_empty() {
            continue;
        }
        if let Some(bytes) = parse_decimal_human_size(value) {
            return Some(bytes);
        }
    }
    None
}

fn parse_decimal_human_size(text: &str) -> Option<u64> {
    let compact: String = text.trim().chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() {
        return None;
    }

    let end = compact
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(compact.len());
    let number = compact[..end].parse::<f64>().ok()?;
    let unit = compact[end..].to_ascii_lowercase();

    let mul = match unit.as_str() {
        "" | "b" => 1.0,
        "kb" | "k" => 1_000.0,
        "mb" | "m" => 1_000_000.0,
        "gb" | "g" => 1_000_000_000.0,
        "tb" | "t" => 1_000_000_000_000.0,
        _ => return None,
    };

    Some((number * mul) as u64)
}

fn extract_opt_root(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/opt/")?;
    let app = rest.split('/').next()?;
    if app.is_empty() {
        return None;
    }
    Some(format!("/opt/{app}"))
}

fn normalize_path_key(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn clear_list_box(list: &gtk::Box) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn append_size_entry_row(
    list: &gtk::Box,
    entry: &SizeEntry,
    max_size: u64,
    total_size: u64,
    mode: PercentageMode,
    lang: Language,
) {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 4);
    row.add_css_class("size-entry-card");
    row.set_margin_top(3);
    row.set_margin_bottom(3);
    row.set_margin_start(6);
    row.set_margin_end(6);

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 10);

    let path_label = gtk::Label::new(Some(&if entry.is_dir {
        format!("{} {}", pick(lang, "目录", "Dir"), entry.path)
    } else {
        format!("{} {}", pick(lang, "文件", "File"), entry.path)
    }));
    path_label.set_halign(gtk::Align::Start);
    path_label.set_hexpand(true);
    path_label.set_wrap(true);
    path_label.set_xalign(0.0);

    let pct = match mode {
        PercentageMode::RelativeMax => {
            if max_size > 0 {
                (entry.size as f64 * 100.0 / max_size as f64).clamp(0.0, 100.0)
            } else {
                0.0
            }
        }
        PercentageMode::RelativeTotal => {
            if total_size > 0 {
                (entry.size as f64 * 100.0 / total_size as f64).clamp(0.0, 100.0)
            } else {
                0.0
            }
        }
    };
    let size_text = format!("{} · {pct:.1}%", format_size(entry.size));
    let value_label = gtk::Label::new(Some(&size_text));
    value_label.set_halign(gtk::Align::End);
    value_label.add_css_class("size-entry-value");
    value_label.add_css_class("monospace");

    top.append(&path_label);
    top.append(&value_label);

    let bar = gtk::ProgressBar::new();
    bar.add_css_class("size-entry-bar");
    if pct >= 66.0 {
        bar.add_css_class("size-entry-bar-high");
    } else if pct >= 33.0 {
        bar.add_css_class("size-entry-bar-mid");
    } else {
        bar.add_css_class("size-entry-bar-low");
    }
    bar.set_fraction((pct / 100.0).clamp(0.0, 1.0));
    bar.set_show_text(false);

    let meta = gtk::Label::new(Some(if entry.is_dir {
        pick(
            lang,
            "目录聚合项（含子文件）",
            "Directory aggregate (includes children)",
        )
    } else {
        pick(lang, "文件明细项", "File detail")
    }));
    meta.add_css_class("size-entry-meta");
    meta.set_xalign(0.0);
    meta.set_halign(gtk::Align::Start);

    row.append(&top);
    row.append(&bar);
    row.append(&meta);
    list.append(&row);
}

fn ensure_panorama_style() {
    PANORAMA_STYLE_ONCE.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(PANORAMA_PAGE_CSS);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
}

fn resolve_install_path_directory(path: Option<&str>) -> Option<String> {
    let raw = path?.trim();
    if raw.is_empty() || raw == "-" {
        return None;
    }

    let input = Path::new(raw);
    if input.is_dir() {
        return Some(raw.to_string());
    }
    if input.is_file() {
        return input.parent().map(|p| p.to_string_lossy().into_owned());
    }

    let mut current = Some(input);
    while let Some(node) = current {
        if node.is_dir() {
            return Some(node.to_string_lossy().into_owned());
        }
        current = node.parent();
    }

    None
}

fn open_path_in_file_manager(path: &str) -> Result<(), std::io::Error> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
}
