use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

use crate::i18n::{pick, Language};
use crate::runtime;
use crate::services::disk;

static SCAN_SEQ: AtomicU64 = AtomicU64::new(1);
static DISK_STYLE_ONCE: Once = Once::new();

const DISK_PAGE_CSS: &str = r#"
.disk-toolbar {
  padding: 8px 10px;
  border-radius: 12px;
  background: alpha(@window_fg_color, 0.045);
}

.disk-badge {
  padding: 4px 8px;
  border-radius: 9999px;
  background: alpha(@accent_color, 0.10);
  font-size: 0.92em;
}

.disk-breadcrumbs {
  padding: 2px 0;
}

.disk-crumb-sep {
  opacity: 0.55;
}

.disk-treemap-frame {
  border-radius: 12px;
  background: alpha(@window_fg_color, 0.03);
  padding: 8px;
}

.disk-panel-title {
  font-weight: 700;
  font-size: 1.03em;
}

.disk-list-header {
  padding: 0 6px;
}

.disk-col-right {
  opacity: 0.7;
}

.disk-col-btn {
  padding: 2px 4px;
}

.disk-row-size {
  font-weight: 600;
}

.disk-filter-row {
  padding: 2px 0;
}

.disk-filter-btn {
  border-radius: 9999px;
  padding: 3px 10px;
}

.disk-filter-btn:checked {
  background: alpha(@accent_color, 0.22);
}

.disk-usage-bar {
  min-width: 126px;
}

.disk-usage-bar trough,
.disk-usage-bar progress {
  min-height: 6px;
  border-radius: 999px;
}
"#;

#[derive(Clone)]
struct TreemapTile {
    name: String,
    path: String,
    size: u64,
    is_dir: bool,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryFilter {
    All,
    Dirs,
    Files,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntrySort {
    SizeDesc,
    SizeAsc,
    NameAsc,
    NameDesc,
}

pub fn build(token: tokio_util::sync::CancellationToken, lang: Language) -> adw::NavigationPage {
    ensure_disk_style();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let spinner = gtk::Spinner::new();
    spinner.set_spinning(true);
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.set_vexpand(true);

    let loading_page = adw::StatusPage::builder()
        .title(pick(lang, "正在分析磁盘占用...", "Analyzing disk usage..."))
        .description(pick(
            lang,
            "首次扫描会稍慢，结果会持续更新",
            "First scan may take a while; results update continuously",
        ))
        .child(&spinner)
        .build();

    let empty_page = adw::StatusPage::builder()
        .title(pick(
            lang,
            "未发现可分析目录",
            "No analyzable folders found",
        ))
        .description(pick(
            lang,
            "请切换完整扫描后重试",
            "Try enabling full scan and rescan",
        ))
        .icon_name("drive-harddisk-symbolic")
        .build();

    let cancelled_page = adw::StatusPage::builder()
        .title(pick(lang, "已取消", "Cancelled"))
        .description(pick(
            lang,
            "扫描已取消，可点击“重新扫描”再试",
            "Scan cancelled; click “Rescan” to try again",
        ))
        .icon_name("process-stop-symbolic")
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&loading_page, Some("loading"));
    stack.add_named(&empty_page, Some("empty"));
    stack.add_named(&cancelled_page, Some("cancelled"));

    let main_paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    main_paned.set_wide_handle(true);
    // 初始默认给中间主视图区约 70% 宽度。
    main_paned.set_position(700);
    // 窗口变宽时优先扩展左侧主视图区，而不是右侧明细栏。
    main_paned.set_resize_start_child(true);
    main_paned.set_resize_end_child(false);
    main_paned.set_shrink_start_child(true);
    main_paned.set_shrink_end_child(true);

    let left_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    left_box.set_margin_top(12);
    left_box.set_margin_bottom(12);
    left_box.set_margin_start(12);
    left_box.set_margin_end(12);

    let toolbar_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    toolbar_row.add_css_class("disk-toolbar");

    let home_content = adw::ButtonContent::builder()
        .icon_name("go-home-symbolic")
        .label(pick(lang, "主目录", "Home"))
        .build();
    let home_button = gtk::Button::new();
    home_button.set_child(Some(&home_content));
    home_button.add_css_class("flat");

    let up_content = adw::ButtonContent::builder()
        .icon_name("go-up-symbolic")
        .label(pick(lang, "上一级", "Up"))
        .build();
    let up_button = gtk::Button::new();
    up_button.set_child(Some(&up_content));
    up_button.add_css_class("flat");

    let mode_toggle = gtk::ToggleButton::with_label(pick(lang, "完整扫描", "Full Scan"));
    mode_toggle.set_tooltip_text(Some(pick(
        lang,
        "关闭=快速（缓存+主目录），开启=完整（含 / 全盘，较慢）",
        "Off=Fast (cache+home), On=Full (includes /, slower)",
    )));

    let rescan_button = gtk::Button::with_label(pick(lang, "重新扫描", "Rescan"));
    rescan_button.add_css_class("suggested-action");

    let cancel_button = gtk::Button::with_label(pick(lang, "取消", "Cancel"));
    cancel_button.add_css_class("flat");
    cancel_button.set_sensitive(false);

    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);

    let mode_badge = gtk::Label::new(Some(pick(lang, "快速模式", "Fast Mode")));
    mode_badge.add_css_class("disk-badge");

    let summary_badge = gtk::Label::new(Some(pick(lang, "等待数据", "Waiting data")));
    summary_badge.add_css_class("disk-badge");

    let scan_badge = gtk::Label::new(Some(pick(lang, "准备中", "Preparing")));
    scan_badge.add_css_class("disk-badge");
    scan_badge.add_css_class("dim-label");

    toolbar_row.append(&home_button);
    toolbar_row.append(&up_button);
    toolbar_row.append(&mode_toggle);
    toolbar_row.append(&rescan_button);
    toolbar_row.append(&cancel_button);
    toolbar_row.append(&spacer);
    toolbar_row.append(&mode_badge);
    toolbar_row.append(&summary_badge);
    toolbar_row.append(&scan_badge);

    let breadcrumb_host = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    breadcrumb_host.add_css_class("disk-breadcrumbs");

    let breadcrumb_scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hexpand(true)
        .child(&breadcrumb_host)
        .build();

    let status_label = gtk::Label::new(None);
    status_label.set_halign(gtk::Align::Start);
    status_label.add_css_class("caption");

    let scan_progress = gtk::ProgressBar::new();
    scan_progress.set_hexpand(true);
    scan_progress.set_show_text(true);
    scan_progress.add_css_class("disk-usage-bar");
    scan_progress.set_fraction(0.0);
    scan_progress.set_text(Some(pick(lang, "准备中", "Preparing")));
    scan_progress.set_visible(false);

    let treemap_area = gtk::DrawingArea::new();
    treemap_area.set_hexpand(true);
    treemap_area.set_vexpand(true);
    // 避免将窗口最小尺寸抬得过高，影响全屏/缩放行为。
    treemap_area.set_content_width(640);
    treemap_area.set_content_height(360);

    let treemap_scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&treemap_area)
        .build();

    let treemap_frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    treemap_frame.add_css_class("disk-treemap-frame");
    treemap_frame.set_vexpand(true);
    treemap_frame.append(&treemap_scrolled);

    left_box.append(&toolbar_row);
    left_box.append(&breadcrumb_scrolled);
    left_box.append(&scan_progress);
    left_box.append(&status_label);
    left_box.append(&treemap_frame);

    let right_scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    right_scrolled.set_hexpand(true);

    let right_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    right_box.set_margin_top(12);
    right_box.set_margin_bottom(12);
    right_box.set_margin_start(12);
    right_box.set_margin_end(12);

    let details_title = gtk::Label::new(Some(pick(lang, "目录明细", "Folder Details")));
    details_title.set_halign(gtk::Align::Start);
    details_title.add_css_class("disk-panel-title");

    let details_hint = gtk::Label::new(Some(pick(
        lang,
        "展示当前目录下所有文件与子目录占用（左键下钻，右键更多操作）",
        "Shows all files and subfolders in current path (left-click drill down, right-click actions)",
    )));
    details_hint.set_halign(gtk::Align::Start);
    details_hint.add_css_class("dim-label");

    let filter_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    filter_row.add_css_class("disk-filter-row");

    let filter_all_btn = gtk::ToggleButton::with_label(pick(lang, "全部", "All"));
    filter_all_btn.add_css_class("disk-filter-btn");
    filter_all_btn.set_active(true);

    let filter_dir_btn = gtk::ToggleButton::with_label(pick(lang, "仅目录", "Dirs"));
    filter_dir_btn.add_css_class("disk-filter-btn");

    let filter_file_btn = gtk::ToggleButton::with_label(pick(lang, "仅文件", "Files"));
    filter_file_btn.add_css_class("disk-filter-btn");

    filter_row.append(&filter_all_btn);
    filter_row.append(&filter_dir_btn);
    filter_row.append(&filter_file_btn);

    let list_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    list_header.add_css_class("disk-list-header");

    let name_col_label = gtk::Label::new(None);
    name_col_label.set_xalign(0.0);
    name_col_label.add_css_class("caption");

    let name_col_btn = gtk::Button::new();
    name_col_btn.add_css_class("flat");
    name_col_btn.add_css_class("disk-col-btn");
    name_col_btn.set_hexpand(true);
    name_col_btn.set_halign(gtk::Align::Fill);
    name_col_btn.set_child(Some(&name_col_label));

    let size_col_label = gtk::Label::new(None);
    size_col_label.set_xalign(1.0);
    size_col_label.add_css_class("caption");
    size_col_label.add_css_class("disk-col-right");

    let size_col_btn = gtk::Button::new();
    size_col_btn.add_css_class("flat");
    size_col_btn.add_css_class("disk-col-btn");
    size_col_btn.set_halign(gtk::Align::End);
    size_col_btn.set_child(Some(&size_col_label));

    list_header.append(&name_col_btn);
    list_header.append(&size_col_btn);

    let details_list = gtk::ListBox::new();
    details_list.add_css_class("boxed-list");
    details_list.set_selection_mode(gtk::SelectionMode::None);

    let details_scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&details_list)
        .build();

    let details_footer = gtk::Label::new(None);
    details_footer.set_halign(gtk::Align::Start);
    details_footer.add_css_class("caption");

    right_box.append(&details_title);
    right_box.append(&details_hint);
    right_box.append(&filter_row);
    right_box.append(&list_header);
    right_box.append(&details_scrolled);
    right_box.append(&details_footer);

    right_scrolled.set_child(Some(&right_box));

    main_paned.set_start_child(Some(&left_box));
    main_paned.set_end_child(Some(&right_scrolled));
    let initial_split_applied = Rc::new(Cell::new(false));
    let clamp_guard = Rc::new(Cell::new(false));
    let enforce_min_middle_ratio: Rc<dyn Fn(&gtk::Paned)> = Rc::new({
        let clamp_guard = clamp_guard.clone();
        move |paned: &gtk::Paned| {
            if clamp_guard.get() {
                return;
            }

            let disk_width = paned.width();
            if disk_width <= 0 {
                return;
            }

            // “总宽度”按窗口主分栏（左侧导航 + 当前页内容）来理解：
            // 中间主视图（磁盘分析主区域）宽度至少占总宽度的 2/3。
            // 使用 ceil(2/3 * width) 避免整数除法向下取整导致比例略小于 2/3。
            let total_width = paned
                .ancestor(gtk::Paned::static_type())
                .and_then(|widget| widget.downcast::<gtk::Paned>().ok())
                .map(|outer| outer.width())
                .unwrap_or(disk_width);
            if total_width <= 0 {
                return;
            }

            let mut min_pos = (((total_width as i64) * 2 + 2) / 3) as i32;
            // 防止把右侧明细栏挤到不可见（极窄窗口时无法满足 2/3 约束）。
            let max_pos = disk_width.saturating_sub(1);
            min_pos = min_pos.clamp(0, max_pos);
            let current_pos = paned.position();
            if current_pos >= min_pos {
                return;
            }

            clamp_guard.set(true);
            paned.set_position(min_pos);
            clamp_guard.set(false);
        }
    });

    main_paned.connect_notify_local(Some("position"), {
        let initial_split_applied = initial_split_applied.clone();
        let enforce_min_middle_ratio = enforce_min_middle_ratio.clone();
        move |paned, _| {
            if !initial_split_applied.get() {
                return;
            }
            enforce_min_middle_ratio(paned);
        }
    });

    let last_disk_width = Rc::new(Cell::new(0));
    main_paned.add_tick_callback({
        let initial_split_applied = initial_split_applied.clone();
        let enforce_min_middle_ratio = enforce_min_middle_ratio.clone();
        let last_disk_width = last_disk_width.clone();
        move |paned, _| {
            let disk_width = paned.width();
            if disk_width <= 0 {
                return glib::ControlFlow::Continue;
            }

            if !initial_split_applied.get() {
                // 首次渲染：默认约 70%，再套用“中间栏 ≥ 2/3 总宽度”的约束。
                let default_pos = disk_width.saturating_mul(7) / 10;
                paned.set_position(default_pos);
                initial_split_applied.set(true);
                enforce_min_middle_ratio(paned);
                last_disk_width.set(disk_width);
                return glib::ControlFlow::Continue;
            }

            if last_disk_width.get() != disk_width {
                enforce_min_middle_ratio(paned);
                last_disk_width.set(disk_width);
            }

            glib::ControlFlow::Continue
        }
    });

    stack.add_named(&main_paned, Some("content"));
    root.append(&stack);

    let rects_state: Rc<RefCell<Vec<TreemapTile>>> = Rc::new(RefCell::new(Vec::new()));
    let visible_children_state: Rc<RefCell<Vec<disk::FolderUsage>>> =
        Rc::new(RefCell::new(Vec::new()));
    let hover_path_state: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let entry_filter_state = Rc::new(RefCell::new(EntryFilter::All));
    let entry_sort_state = Rc::new(RefCell::new(EntrySort::SizeDesc));

    treemap_area.set_draw_func({
        let rects_state = rects_state.clone();
        let visible_children_state = visible_children_state.clone();
        let hover_path_state = hover_path_state.clone();

        move |_, cr, width, height| {
            let items = visible_children_state.borrow().clone();
            let total_size: u64 = items.iter().map(|item| item.size).sum();
            let tiles = build_treemap_tiles(&items, f64::from(width), f64::from(height));
            let label_priority = priority_label_paths(&tiles, 10);
            let hovered = hover_path_state.borrow().clone();

            *rects_state.borrow_mut() = tiles.clone();

            cr.set_source_rgba(0.0, 0.0, 0.0, 0.05);
            cr.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
            let _ = cr.fill();

            for tile in &tiles {
                let is_hovered = hovered.as_deref() == Some(tile.path.as_str());
                let (mut r, mut g, mut b) = tile_color(tile, total_size);
                if is_hovered {
                    (r, g, b) = boost_color(r, g, b, 0.09);
                }

                let radius = tile.w.min(tile.h).mul_add(0.06, 3.0).clamp(2.0, 10.0);

                if tile.w >= 22.0 && tile.h >= 16.0 {
                    rounded_rect(cr, tile.x + 0.8, tile.y + 0.8, tile.w, tile.h, radius);
                    cr.set_source_rgba(0.0, 0.0, 0.0, 0.09);
                    let _ = cr.fill();
                }

                rounded_rect(cr, tile.x, tile.y, tile.w, tile.h, radius);
                cr.set_source_rgb(r, g, b);
                let _ = cr.fill_preserve();

                if is_hovered {
                    cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
                    cr.set_line_width(2.0);
                } else {
                    cr.set_source_rgba(0.0, 0.0, 0.0, 0.30);
                    cr.set_line_width(1.0);
                }
                let _ = cr.stroke();

                let force_compact_label = label_priority.contains(&tile.path);
                draw_tile_label(cr, tile, total_size, force_compact_label);
            }
        }
    });

    let current_root = Rc::new(RefCell::new(String::from("/")));
    let dataset_state: Rc<RefCell<Option<DiskDataset>>> = Rc::new(RefCell::new(None));
    let render_cell: Rc<RefCell<Option<Box<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    let active_scan_id = Rc::new(RefCell::new(0u64));
    let scan_task_token = Rc::new(RefCell::new(None::<tokio_util::sync::CancellationToken>));
    treemap_area.set_has_tooltip(true);
    let motion = gtk::EventControllerMotion::new();
    {
        let rects_state = rects_state.clone();

        let treemap_area_for_motion = treemap_area.clone();
        let hover_path_state_for_motion = hover_path_state.clone();
        motion.connect_motion(move |_, x, y| {
            let hit = rects_state
                .borrow()
                .iter()
                .find(|tile| point_in_tile(tile, x, y))
                .cloned();

            let next_hover = hit.as_ref().map(|tile| tile.path.clone());
            let mut changed = false;
            {
                let mut hover = hover_path_state_for_motion.borrow_mut();
                if *hover != next_hover {
                    *hover = next_hover;
                    changed = true;
                }
            }
            if changed {
                treemap_area_for_motion.queue_draw();
            }

            match hit {
                Some(tile) => {
                    let kind = if tile.is_dir {
                        pick(lang, "目录", "Directory")
                    } else {
                        pick(lang, "文件", "File")
                    };
                    treemap_area_for_motion.set_tooltip_text(Some(&format!(
                        "{} · {} · {}",
                        format_size(tile.size),
                        kind,
                        tile.path,
                    )));
                }
                None => treemap_area_for_motion.set_tooltip_text(None),
            }
        });

        let treemap_area_for_leave = treemap_area.clone();
        let hover_path_state_for_leave = hover_path_state.clone();
        motion.connect_leave(move |_| {
            let mut hover = hover_path_state_for_leave.borrow_mut();
            if hover.is_some() {
                *hover = None;
                treemap_area_for_leave.queue_draw();
            }
            treemap_area_for_leave.set_tooltip_text(None);
        });
    }
    treemap_area.add_controller(motion);

    let click = gtk::GestureClick::new();
    click.set_button(1);
    {
        let rects_state = rects_state.clone();
        let current_root = current_root.clone();
        let render_cell_ref = render_cell.clone();
        click.connect_pressed(move |_, _, x, y| {
            let next_path = rects_state
                .borrow()
                .iter()
                .find(|tile| tile.is_dir && point_in_tile(tile, x, y))
                .map(|tile| tile.path.clone());

            if let Some(path) = next_path {
                *current_root.borrow_mut() = path;
                if let Some(render) = &*render_cell_ref.borrow() {
                    render();
                }
            }
        });
    }
    treemap_area.add_controller(click);

    let treemap_right_click = gtk::GestureClick::new();
    treemap_right_click.set_button(3);
    {
        let rects_state = rects_state.clone();
        let dataset_state = dataset_state.clone();
        let visible_children_state = visible_children_state.clone();
        let treemap_area = treemap_area.clone();

        treemap_right_click.connect_pressed(move |_, _, x, y| {
            let hit = rects_state
                .borrow()
                .iter()
                .find(|tile| point_in_tile(tile, x, y))
                .cloned();

            if let Some(tile) = hit {
                let shown_size: u64 = visible_children_state
                    .borrow()
                    .iter()
                    .map(|item| item.size)
                    .sum();
                let child_count = if tile.is_dir {
                    dataset_state.borrow().as_ref().and_then(|dataset| {
                        dataset
                            .children_map
                            .get(&normalize_path(&tile.path))
                            .map(Vec::len)
                    })
                } else {
                    None
                };

                let entry = disk::FolderUsage {
                    name: tile.name,
                    path: tile.path,
                    size: tile.size,
                    is_dir: tile.is_dir,
                };
                show_disk_entry_context_menu(
                    &treemap_area,
                    x,
                    y,
                    entry,
                    shown_size,
                    child_count,
                    lang,
                );
            }
        });
    }
    treemap_area.add_controller(treemap_right_click);

    {
        let breadcrumb_host = breadcrumb_host.clone();
        let status_label = status_label.clone();
        let details_list = details_list.clone();
        let details_footer = details_footer.clone();
        let treemap_area = treemap_area.clone();
        let current_root = current_root.clone();
        let dataset_state = dataset_state.clone();
        let render_cell_ref = render_cell.clone();
        let visible_children_state = visible_children_state.clone();
        let up_button = up_button.clone();
        let home_button = home_button.clone();
        let summary_badge = summary_badge.clone();
        let entry_filter_state = entry_filter_state.clone();
        let entry_sort_state = entry_sort_state.clone();
        let name_col_label = name_col_label.clone();
        let size_col_label = size_col_label.clone();

        *render_cell.borrow_mut() = Some(Box::new(move || {
            while let Some(child) = breadcrumb_host.first_child() {
                breadcrumb_host.remove(&child);
            }
            while let Some(child) = details_list.first_child() {
                details_list.remove(&child);
            }

            let root_path = normalize_path(current_root.borrow().as_str());
            home_button.set_sensitive(root_path != "/");
            up_button.set_sensitive(root_path != "/");

            let crumbs = breadcrumb_segments(&root_path, lang);
            for (idx, (label, target)) in crumbs.iter().enumerate() {
                let btn = gtk::Button::with_label(label);
                btn.add_css_class("flat");

                if target == &root_path {
                    btn.set_sensitive(false);
                } else {
                    let target = target.clone();
                    let current_root = current_root.clone();
                    let render_cell_ref = render_cell_ref.clone();
                    btn.connect_clicked(move |_| {
                        *current_root.borrow_mut() = target.clone();
                        if let Some(render) = &*render_cell_ref.borrow() {
                            render();
                        }
                    });
                }
                breadcrumb_host.append(&btn);

                if idx + 1 < crumbs.len() {
                    let sep = gtk::Label::new(Some("›"));
                    sep.add_css_class("disk-crumb-sep");
                    sep.add_css_class("dim-label");
                    breadcrumb_host.append(&sep);
                }
            }

            let dataset = dataset_state.borrow().clone();
            let Some(dataset) = dataset else {
                visible_children_state.borrow_mut().clear();
                status_label.set_label(pick(
                    lang,
                    "等待扫描结果...",
                    "Waiting for scan results...",
                ));
                details_footer.set_label(pick(lang, "暂无数据", "No data yet"));
                summary_badge.set_label(pick(lang, "等待数据", "Waiting data"));
                treemap_area.queue_draw();
                return;
            };

            let children = collect_children(
                &dataset.children_map,
                &dataset.roots,
                &dataset.root_labels,
                &root_path,
            );

            let filter = *entry_filter_state.borrow();
            let sort = *entry_sort_state.borrow();
            update_sort_header_labels(&name_col_label, &size_col_label, sort, lang);

            let mut filtered_children = filter_children(&children, filter);
            sort_children(&mut filtered_children, sort);

            let total_size: u64 = children.iter().map(|item| item.size).sum();
            let shown_size: u64 = filtered_children.iter().map(|item| item.size).sum();
            let summary = match lang {
                Language::ZhCn => format!(
                    "显示 {} / {} 项 · 过滤：{} · 排序：{} · 占用 {} / {}",
                    filtered_children.len(),
                    children.len(),
                    filter_label(filter, lang),
                    sort_label(sort, lang),
                    format_size(shown_size),
                    format_size(total_size)
                ),
                Language::En => format!(
                    "Showing {} / {} entries · Filter: {} · Sort: {} · {} / {}",
                    filtered_children.len(),
                    children.len(),
                    filter_label(filter, lang),
                    sort_label(sort, lang),
                    format_size(shown_size),
                    format_size(total_size)
                ),
            };
            status_label.set_label(&summary);
            summary_badge.set_label(&format!(
                "{} · {} · {}/{}",
                filter_label(filter, lang),
                sort_label(sort, lang),
                filtered_children.len(),
                children.len()
            ));

            *visible_children_state.borrow_mut() = filtered_children.clone();
            treemap_area.queue_draw();

            if filtered_children.is_empty() {
                let row = adw::ActionRow::builder()
                    .title(match filter {
                        EntryFilter::All => pick(
                            lang,
                            "该目录暂无可展示文件",
                            "No files to show in this folder",
                        ),
                        EntryFilter::Dirs => {
                            pick(lang, "当前目录下没有子目录", "No subfolders in this path")
                        }
                        EntryFilter::Files => {
                            pick(lang, "当前目录下没有文件", "No files in this path")
                        }
                    })
                    .subtitle(pick(
                        lang,
                        "可切换过滤器查看全部",
                        "Switch filter to see all entries",
                    ))
                    .build();
                details_list.append(&row);
                details_footer.set_label(pick(lang, "0 条结果", "0 results"));
                return;
            }

            for (idx, item) in filtered_children.iter().enumerate() {
                let kind = if item.is_dir {
                    pick(lang, "目录", "Directory")
                } else {
                    pick(lang, "文件", "File")
                };
                let icon_name = if item.is_dir {
                    "folder-symbolic"
                } else {
                    "text-x-generic-symbolic"
                };
                let share = if shown_size > 0 {
                    item.size as f64 * 100.0 / shown_size as f64
                } else {
                    0.0
                };

                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&format!("{}. {}", idx + 1, item.name)))
                    .subtitle(glib::markup_escape_text(&format!(
                        "{} · {}",
                        kind,
                        compact_path_for_label(&item.path, 72)
                    )))
                    .build();
                row.set_tooltip_text(Some(&item.path));
                row.add_prefix(&gtk::Image::from_icon_name(icon_name));

                let suffix_box = gtk::Box::new(gtk::Orientation::Vertical, 3);
                suffix_box.set_halign(gtk::Align::End);

                let top_line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let share_label = gtk::Label::new(Some(&format!("{share:.1}%")));
                share_label.add_css_class("caption");
                share_label.add_css_class("dim-label");
                let size_label = gtk::Label::new(Some(&format_size(item.size)));
                size_label.add_css_class("disk-row-size");
                size_label.add_css_class("monospace");

                let usage_bar = gtk::ProgressBar::new();
                usage_bar.add_css_class("disk-usage-bar");
                usage_bar.set_fraction((share / 100.0).clamp(0.0, 1.0));
                usage_bar.set_show_text(false);

                top_line.append(&share_label);
                top_line.append(&size_label);
                suffix_box.append(&top_line);
                suffix_box.append(&usage_bar);
                row.add_suffix(&suffix_box);

                if item.is_dir {
                    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                    row.set_activatable(true);

                    let path = item.path.clone();
                    let current_root = current_root.clone();
                    let render_cell_ref = render_cell_ref.clone();
                    row.connect_activated(move |_| {
                        *current_root.borrow_mut() = path.clone();
                        if let Some(render) = &*render_cell_ref.borrow() {
                            render();
                        }
                    });
                }

                let row_right_click = gtk::GestureClick::new();
                row_right_click.set_button(3);
                {
                    let row = row.clone();
                    let item = item.clone();
                    let child_count = if item.is_dir {
                        dataset
                            .children_map
                            .get(&normalize_path(&item.path))
                            .map(Vec::len)
                    } else {
                        None
                    };
                    row_right_click.connect_pressed(move |_, _, x, y| {
                        show_disk_entry_context_menu(
                            &row,
                            x,
                            y,
                            item.clone(),
                            shown_size,
                            child_count,
                            lang,
                        );
                    });
                }
                row.add_controller(row_right_click);

                details_list.append(&row);
            }

            let dir_count = filtered_children.iter().filter(|item| item.is_dir).count();
            let file_count = filtered_children.len().saturating_sub(dir_count);
            let footer = match lang {
                Language::ZhCn => {
                    format!(
                        "显示 {} / {} 项（目录 {} · 文件 {}）· 当前可见占用 {}",
                        filtered_children.len(),
                        children.len(),
                        dir_count,
                        file_count,
                        format_size(shown_size)
                    )
                }
                Language::En => {
                    format!(
                        "Showing {} / {} entries ({} dirs · {} files) · Visible size {}",
                        filtered_children.len(),
                        children.len(),
                        dir_count,
                        file_count,
                        format_size(shown_size)
                    )
                }
            };
            details_footer.set_label(&footer);
        }));
    }

    {
        let current_root = current_root.clone();
        let render_cell_ref = render_cell.clone();
        home_button.connect_clicked(move |_| {
            *current_root.borrow_mut() = "/".to_string();
            if let Some(render) = &*render_cell_ref.borrow() {
                render();
            }
        });
    }

    {
        let entry_filter_state = entry_filter_state.clone();
        let filter_all_btn = filter_all_btn.clone();
        let filter_dir_btn = filter_dir_btn.clone();
        let filter_file_btn = filter_file_btn.clone();
        let render_cell_ref = render_cell.clone();

        let filter_all_for_apply = filter_all_btn.clone();
        let filter_dir_for_apply = filter_dir_btn.clone();
        let filter_file_for_apply = filter_file_btn.clone();
        let apply_filter: Rc<dyn Fn(EntryFilter)> = Rc::new(move |filter: EntryFilter| {
            *entry_filter_state.borrow_mut() = filter;
            filter_all_for_apply.set_active(filter == EntryFilter::All);
            filter_dir_for_apply.set_active(filter == EntryFilter::Dirs);
            filter_file_for_apply.set_active(filter == EntryFilter::Files);
            if let Some(render) = &*render_cell_ref.borrow() {
                render();
            }
        });

        let apply_all = apply_filter.clone();
        filter_all_btn.connect_clicked(move |_| apply_all(EntryFilter::All));

        let apply_dirs = apply_filter.clone();
        filter_dir_btn.connect_clicked(move |_| apply_dirs(EntryFilter::Dirs));

        filter_file_btn.connect_clicked(move |_| apply_filter(EntryFilter::Files));
    }

    {
        let entry_sort_state = entry_sort_state.clone();
        let render_cell_ref = render_cell.clone();
        let name_col_btn = name_col_btn.clone();
        name_col_btn.connect_clicked(move |_| {
            let next = {
                let current = *entry_sort_state.borrow();
                toggle_name_sort(current)
            };
            *entry_sort_state.borrow_mut() = next;
            if let Some(render) = &*render_cell_ref.borrow() {
                render();
            }
        });
    }

    {
        let entry_sort_state = entry_sort_state.clone();
        let render_cell_ref = render_cell.clone();
        let size_col_btn = size_col_btn.clone();
        size_col_btn.connect_clicked(move |_| {
            let next = {
                let current = *entry_sort_state.borrow();
                toggle_size_sort(current)
            };
            *entry_sort_state.borrow_mut() = next;
            if let Some(render) = &*render_cell_ref.borrow() {
                render();
            }
        });
    }

    {
        let current_root = current_root.clone();
        let render_cell_ref = render_cell.clone();
        let dataset_state_for_up = dataset_state.clone();
        up_button.connect_clicked(move |_| {
            let current = current_root.borrow().clone();
            if current == "/" {
                return;
            }

            let dataset = dataset_state_for_up.borrow().clone();
            let parent = choose_up_target(&current, dataset.as_ref());

            *current_root.borrow_mut() = parent;
            if let Some(render) = &*render_cell_ref.borrow() {
                render();
            }
        });
    }

    let (tx, rx) = async_channel::bounded::<disk::DiskEvent>(32);
    let tx_for_rescan = tx.clone();

    let start_scan: Rc<dyn Fn(disk::ScanMode)> = {
        let token = token.clone();
        let active_scan_id = active_scan_id.clone();
        let scan_badge = scan_badge.clone();
        let stack = stack.clone();
        let dataset_state = dataset_state.clone();
        let scan_task_token = scan_task_token.clone();
        let cancel_button = cancel_button.clone();
        let scan_progress = scan_progress.clone();

        Rc::new(move |mode: disk::ScanMode| {
            if let Some(prev) = scan_task_token.borrow_mut().take() {
                prev.cancel();
            }

            let scan_token = token.child_token();
            *scan_task_token.borrow_mut() = Some(scan_token.clone());

            let scan_id = SCAN_SEQ.fetch_add(1, Ordering::Relaxed);
            *active_scan_id.borrow_mut() = scan_id;

            cancel_button.set_sensitive(true);
            scan_progress.set_fraction(0.0);
            scan_progress.set_text(Some(pick(lang, "准备中", "Preparing")));
            scan_progress.set_visible(true);

            scan_badge.set_label(match mode {
                disk::ScanMode::Fast => pick(lang, "扫描中（快速）", "Scanning (Fast)"),
                disk::ScanMode::Full => pick(lang, "扫描中（完整）", "Scanning (Full)"),
            });

            if dataset_state.borrow().is_none() {
                stack.set_visible_child_name("loading");
            }

            let tx_clone = tx_for_rescan.clone();
            runtime::spawn(async move {
                disk::scan_all(tx_clone, scan_token, mode, scan_id).await;
            });
        })
    };

    {
        let scan_task_token = scan_task_token.clone();
        let active_scan_id = active_scan_id.clone();
        let scan_badge = scan_badge.clone();
        let cancel_button = cancel_button.clone();
        let scan_progress = scan_progress.clone();
        let stack = stack.clone();
        let dataset_state = dataset_state.clone();

        cancel_button.connect_clicked({
            let cancel_button = cancel_button.clone();
            move |_| {
                if let Some(token) = scan_task_token.borrow_mut().take() {
                    token.cancel();
                }
                *active_scan_id.borrow_mut() = 0;

                cancel_button.set_sensitive(false);
                scan_progress.set_visible(false);
                scan_badge.set_label(pick(lang, "已取消", "Cancelled"));

                if dataset_state.borrow().is_none() {
                    stack.set_visible_child_name("cancelled");
                }
            }
        });
    }

    {
        let start_scan = start_scan.clone();
        let mode_toggle = mode_toggle.clone();
        rescan_button.connect_clicked(move |_| {
            let mode = if mode_toggle.is_active() {
                disk::ScanMode::Full
            } else {
                disk::ScanMode::Fast
            };
            start_scan(mode);
        });
    }

    {
        let start_scan = start_scan.clone();
        let mode_badge = mode_badge.clone();
        mode_toggle.connect_toggled(move |btn| {
            let mode = if btn.is_active() {
                mode_badge.set_label(pick(lang, "完整模式", "Full Mode"));
                disk::ScanMode::Full
            } else {
                mode_badge.set_label(pick(lang, "快速模式", "Fast Mode"));
                disk::ScanMode::Fast
            };
            start_scan(mode);
        });
    }

    start_scan(disk::ScanMode::Fast);

    glib::spawn_future_local(async move {
        while let Ok(event) = rx.recv().await {
            match event {
                disk::DiskEvent::Progress(progress) => {
                    if progress.scan_id != *active_scan_id.borrow() {
                        continue;
                    }

                    scan_progress.set_text(Some(&format_disk_progress(&progress, lang)));
                    scan_progress.set_visible(true);

                    let fraction = if progress.total == 0 || progress.total == u32::MAX {
                        0.0
                    } else if progress.stage == disk::DiskStage::Finished {
                        1.0
                    } else {
                        (f64::from(progress.done) / f64::from(progress.total)).clamp(0.0, 1.0)
                    };
                    scan_progress.set_fraction(fraction);

                    if progress.stage == disk::DiskStage::Finished {
                        cancel_button.set_sensitive(false);
                        scan_progress.set_visible(false);
                    }
                }
                disk::DiskEvent::Snapshot(snapshot) => {
                    if snapshot.scan_id != *active_scan_id.borrow() {
                        continue;
                    }

                    if snapshot.folder_usage.is_empty() {
                        *dataset_state.borrow_mut() = None;
                        summary_badge.set_label(pick(lang, "无数据", "No data"));
                        scan_badge.set_label(pick(lang, "扫描完成", "Scan finished"));
                        cancel_button.set_sensitive(false);
                        scan_progress.set_visible(false);
                        stack.set_visible_child_name("empty");
                        continue;
                    }

                    let mut roots = snapshot.roots;
                    roots.sort();
                    roots.dedup();

                    let root_labels: HashMap<String, String> = snapshot
                        .caches
                        .iter()
                        .map(|cache| (normalize_path(&cache.path), cache.name.clone()))
                        .collect();

                    let mut root_labels = root_labels;
                    root_labels
                        .entry("/".to_string())
                        .or_insert_with(|| pick(lang, "完整文件系统", "Full Filesystem").to_string());
                    if let Ok(home) = std::env::var("HOME") {
                        let home_key = normalize_path(&home);
                        root_labels
                            .entry(home_key)
                            .or_insert_with(|| pick(lang, "用户主目录", "Home Directory").to_string());
                    }

                    if snapshot.folder_usage.values().all(Vec::is_empty) {
                        *dataset_state.borrow_mut() = None;
                        summary_badge.set_label(pick(lang, "无数据", "No data"));
                        scan_badge.set_label(pick(lang, "扫描完成", "Scan finished"));
                        cancel_button.set_sensitive(false);
                        scan_progress.set_visible(false);
                        stack.set_visible_child_name("empty");
                        continue;
                    }

                    let new_dataset = DiskDataset {
                        children_map: snapshot.folder_usage,
                        roots,
                        root_labels,
                    };

                    let requested_root = normalize_path(current_root.borrow().as_str());
                    let next_root = if requested_root == "/"
                        || new_dataset.children_map.contains_key(&requested_root)
                        || new_dataset.roots.iter().any(|root| root == &requested_root)
                    {
                        requested_root
                    } else {
                        "/".to_string()
                    };

                    *dataset_state.borrow_mut() = Some(new_dataset);
                    *current_root.borrow_mut() = next_root;

                    if snapshot.is_final {
                        scan_badge.set_label(pick(lang, "扫描完成", "Scan finished"));
                        cancel_button.set_sensitive(false);
                        scan_progress.set_visible(false);
                    } else {
                        scan_badge.set_label(pick(
                            lang,
                            "快速结果已就绪，继续扫描 / ...",
                            "Fast result ready, scanning / ...",
                        ));
                        cancel_button.set_sensitive(true);
                        scan_progress.set_visible(true);
                    }

                    if let Some(render) = &*render_cell.borrow() {
                        render();
                    }

                    stack.set_visible_child_name("content");
                }
            }
        }
    });

    adw::NavigationPage::builder()
        .title(pick(lang, "磁盘分析", "Disk Analysis"))
        .child(&root)
        .build()
}

#[derive(Clone)]
struct DiskDataset {
    children_map: HashMap<String, Vec<disk::FolderUsage>>,
    roots: Vec<String>,
    root_labels: HashMap<String, String>,
}

fn ensure_disk_style() {
    DISK_STYLE_ONCE.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(DISK_PAGE_CSS);

        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
}

fn collect_children(
    children_map: &HashMap<String, Vec<disk::FolderUsage>>,
    roots: &[String],
    root_labels: &HashMap<String, String>,
    root: &str,
) -> Vec<disk::FolderUsage> {
    let normalized_root = normalize_path(root);

    if normalized_root == "/" {
        return roots
            .iter()
            .map(|root_path| {
                let size = children_map
                    .get(root_path)
                    .map(|children| children.iter().map(|item| item.size).sum())
                    .unwrap_or(0);

                let name = root_labels
                    .get(root_path)
                    .cloned()
                    .unwrap_or_else(|| display_name(root_path));

                disk::FolderUsage {
                    name,
                    path: root_path.clone(),
                    size,
                    is_dir: true,
                }
            })
            .collect();
    }

    if let Some(children) = children_map.get(&normalized_root) {
        return children.clone();
    }

    // 兜底：当用户通过面包屑跳到“未扫描的中间目录”（例如 /home、/var）时，
    // children_map 里可能不存在该 key。这里根据扫描 roots 补齐一层“虚拟子目录”，
    // 让用户能继续下钻到已扫描的根目录，避免界面看起来空白。
    collect_virtual_children(children_map, roots, &normalized_root)
}

fn collect_virtual_children(
    children_map: &HashMap<String, Vec<disk::FolderUsage>>,
    roots: &[String],
    root: &str,
) -> Vec<disk::FolderUsage> {
    if root == "/" {
        return Vec::new();
    }

    let prefix = format!("{root}/");
    let mut child_paths: HashSet<String> = HashSet::new();

    for scan_root in roots {
        if scan_root == root {
            continue;
        }
        if !scan_root.starts_with(&prefix) {
            continue;
        }

        let rest = &scan_root[prefix.len()..];
        let Some(segment) = rest.split('/').next() else {
            continue;
        };
        if segment.is_empty() {
            continue;
        }

        child_paths.insert(format!("{root}/{segment}"));
    }

    let mut out: Vec<disk::FolderUsage> = child_paths
        .into_iter()
        .map(|child_path| {
            let size = children_map
                .get(&child_path)
                .map(|children| children.iter().map(|item| item.size).sum())
                .unwrap_or_else(|| infer_virtual_size(children_map, roots, &child_path));

            disk::FolderUsage {
                name: display_name(&child_path),
                path: child_path,
                size,
                is_dir: true,
            }
        })
        .collect();

    // 确保稳定顺序，避免 UI 频繁抖动（后续仍会按用户选择的排序方式重排）。
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn infer_virtual_size(
    children_map: &HashMap<String, Vec<disk::FolderUsage>>,
    roots: &[String],
    path: &str,
) -> u64 {
    let prefix = format!("{path}/");
    let mut candidates: Vec<&String> = roots
        .iter()
        .filter(|root| root.as_str() == path || root.starts_with(&prefix))
        .collect();
    candidates.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));

    // 去掉“被更短 root 覆盖”的子 root，避免大小推断时重复计数。
    let mut selected: Vec<&String> = Vec::new();
    for candidate in candidates {
        let candidate_path = candidate.as_str();
        if selected.iter().any(|picked| {
            let picked = picked.as_str();
            candidate_path == picked || candidate_path.starts_with(&format!("{picked}/"))
        }) {
            continue;
        }
        selected.push(candidate);
    }

    selected
        .iter()
        .filter_map(|root| children_map.get(*root))
        .map(|children| children.iter().map(|item| item.size).sum::<u64>())
        .sum()
}

fn choose_up_target(current_path: &str, dataset: Option<&DiskDataset>) -> String {
    let current = normalize_path(current_path);
    if current == "/" {
        return "/".to_string();
    }

    let Some(dataset) = dataset else {
        return parent_path(&current);
    };

    if dataset.roots.iter().any(|root| root == &current) {
        return "/".to_string();
    }

    let mut candidate = parent_path(&current);
    while candidate != "/" {
        if dataset.children_map.contains_key(&candidate)
            || dataset.roots.iter().any(|root| root == &candidate)
        {
            return candidate;
        }
        candidate = parent_path(&candidate);
    }

    "/".to_string()
}

fn parent_path(path: &str) -> String {
    std::path::Path::new(path)
        .parent()
        .and_then(|v| v.to_str())
        .map(normalize_path)
        .unwrap_or_else(|| "/".to_string())
}

fn breadcrumb_segments(path: &str, lang: Language) -> Vec<(String, String)> {
    let normalized = normalize_path(path);
    let mut out = vec![(pick(lang, "根目录", "Root").to_string(), "/".to_string())];
    if normalized == "/" {
        return out;
    }

    let mut cursor = String::new();
    for segment in normalized.trim_start_matches('/').split('/') {
        if segment.is_empty() {
            continue;
        }
        cursor.push('/');
        cursor.push_str(segment);
        out.push((segment.to_string(), cursor.clone()));
    }

    out
}

fn filter_children(items: &[disk::FolderUsage], filter: EntryFilter) -> Vec<disk::FolderUsage> {
    items
        .iter()
        .filter(|item| match filter {
            EntryFilter::All => true,
            EntryFilter::Dirs => item.is_dir,
            EntryFilter::Files => !item.is_dir,
        })
        .cloned()
        .collect()
}

fn filter_label(filter: EntryFilter, lang: Language) -> &'static str {
    match filter {
        EntryFilter::All => pick(lang, "全部", "All"),
        EntryFilter::Dirs => pick(lang, "仅目录", "Dirs"),
        EntryFilter::Files => pick(lang, "仅文件", "Files"),
    }
}

fn sort_children(items: &mut [disk::FolderUsage], sort: EntrySort) {
    match sort {
        EntrySort::SizeDesc => {
            items.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
        }
        EntrySort::SizeAsc => {
            items.sort_by(|a, b| a.size.cmp(&b.size).then_with(|| a.name.cmp(&b.name)));
        }
        EntrySort::NameAsc => {
            items.sort_by(|a, b| {
                a.name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then_with(|| b.size.cmp(&a.size))
            });
        }
        EntrySort::NameDesc => {
            items.sort_by(|a, b| {
                b.name
                    .to_lowercase()
                    .cmp(&a.name.to_lowercase())
                    .then_with(|| b.size.cmp(&a.size))
            });
        }
    }
}

fn sort_label(sort: EntrySort, lang: Language) -> &'static str {
    match sort {
        EntrySort::SizeDesc => pick(lang, "占用↓", "Size ↓"),
        EntrySort::SizeAsc => pick(lang, "占用↑", "Size ↑"),
        EntrySort::NameAsc => pick(lang, "名称A→Z", "Name A→Z"),
        EntrySort::NameDesc => pick(lang, "名称Z→A", "Name Z→A"),
    }
}

fn toggle_name_sort(current: EntrySort) -> EntrySort {
    match current {
        EntrySort::NameAsc => EntrySort::NameDesc,
        _ => EntrySort::NameAsc,
    }
}

fn toggle_size_sort(current: EntrySort) -> EntrySort {
    match current {
        EntrySort::SizeDesc => EntrySort::SizeAsc,
        _ => EntrySort::SizeDesc,
    }
}

fn update_sort_header_labels(
    name_col_label: &gtk::Label,
    size_col_label: &gtk::Label,
    sort: EntrySort,
    lang: Language,
) {
    let name_base = pick(lang, "名称", "Name");
    let size_base = pick(lang, "占用 / 比例", "Size / Share");

    let name_text = match sort {
        EntrySort::NameAsc => format!("{name_base} ↑"),
        EntrySort::NameDesc => format!("{name_base} ↓"),
        _ => name_base.to_string(),
    };

    let size_text = match sort {
        EntrySort::SizeDesc => format!("{size_base} ↓"),
        EntrySort::SizeAsc => format!("{size_base} ↑"),
        _ => size_base.to_string(),
    };

    name_col_label.set_label(&name_text);
    size_col_label.set_label(&size_text);
}

fn build_treemap_tiles(items: &[disk::FolderUsage], width: f64, height: f64) -> Vec<TreemapTile> {
    let mut sorted: Vec<disk::FolderUsage> = items.to_vec();
    sorted.sort_by(|a, b| b.size.cmp(&a.size));

    let mut tiles = Vec::new();
    layout_treemap(
        &sorted,
        0.0,
        0.0,
        width.max(1.0),
        height.max(1.0),
        &mut tiles,
    );
    tiles
}

fn layout_treemap(
    items: &[disk::FolderUsage],
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    out: &mut Vec<TreemapTile>,
) {
    if items.is_empty() || w <= 1.0 || h <= 1.0 {
        return;
    }

    if items.len() == 1 || w <= 36.0 || h <= 24.0 {
        let item = &items[0];
        let gap = 2.0;
        let tile_w = (w - gap * 2.0).max(1.0);
        let tile_h = (h - gap * 2.0).max(1.0);
        out.push(TreemapTile {
            name: item.name.clone(),
            path: item.path.clone(),
            size: item.size,
            is_dir: item.is_dir,
            x: x + gap,
            y: y + gap,
            w: tile_w,
            h: tile_h,
        });
        return;
    }

    let total: u64 = items.iter().map(|item| item.size.max(1)).sum();
    if total == 0 {
        let each = if w >= h {
            w / items.len() as f64
        } else {
            h / items.len() as f64
        };

        for (idx, item) in items.iter().enumerate() {
            if w >= h {
                layout_treemap(
                    std::slice::from_ref(item),
                    x + each * idx as f64,
                    y,
                    each,
                    h,
                    out,
                );
            } else {
                layout_treemap(
                    std::slice::from_ref(item),
                    x,
                    y + each * idx as f64,
                    w,
                    each,
                    out,
                );
            }
        }
        return;
    }

    let mut acc = 0u64;
    let mut split_idx = 0usize;
    for (idx, item) in items.iter().enumerate() {
        acc = acc.saturating_add(item.size.max(1));
        if acc.saturating_mul(2) >= total {
            split_idx = idx + 1;
            break;
        }
    }

    if split_idx == 0 || split_idx >= items.len() {
        split_idx = items.len() / 2;
    }

    let (first, second) = items.split_at(split_idx);
    if second.is_empty() {
        layout_treemap(first, x, y, w, h, out);
        return;
    }

    let first_sum: u64 = first.iter().map(|item| item.size.max(1)).sum();
    let ratio = first_sum as f64 / total as f64;

    if w >= h {
        let w1 = (w * ratio).clamp(1.0, (w - 1.0).max(1.0));
        layout_treemap(first, x, y, w1, h, out);
        layout_treemap(second, x + w1, y, w - w1, h, out);
    } else {
        let h1 = (h * ratio).clamp(1.0, (h - 1.0).max(1.0));
        layout_treemap(first, x, y, w, h1, out);
        layout_treemap(second, x, y + h1, w, h - h1, out);
    }
}

fn point_in_tile(tile: &TreemapTile, x: f64, y: f64) -> bool {
    x >= tile.x && x <= tile.x + tile.w && y >= tile.y && y <= tile.y + tile.h
}

fn priority_label_paths(tiles: &[TreemapTile], max_count: usize) -> HashSet<String> {
    let mut ranked: Vec<&TreemapTile> = tiles.iter().collect();
    ranked.sort_by(|a, b| b.size.cmp(&a.size));

    ranked
        .into_iter()
        .filter(|tile| tile.w >= 42.0 && tile.h >= 14.0)
        .take(max_count)
        .map(|tile| tile.path.clone())
        .collect()
}

fn draw_tile_label(
    cr: &gtk::cairo::Context,
    tile: &TreemapTile,
    total_size: u64,
    force_compact_label: bool,
) {
    let area = tile.w * tile.h;
    let pct = if total_size > 0 {
        tile.size as f64 * 100.0 / total_size as f64
    } else {
        0.0
    };

    if tile.w < 34.0 || tile.h < 12.0 {
        return;
    }

    cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);

    if area >= 15_000.0 && tile.w >= 165.0 && tile.h >= 56.0 {
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.22);
        rounded_rect(
            cr,
            tile.x + 1.3,
            tile.y + 1.3,
            tile.w - 2.6,
            20.0,
            (tile.w * 0.03).clamp(3.0, 8.0),
        );
        let _ = cr.fill();

        cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
        cr.select_font_face(
            "Sans",
            gtk::cairo::FontSlant::Normal,
            gtk::cairo::FontWeight::Bold,
        );
        cr.set_font_size(12.0);
        let title = fit_label(&tile.name, tile.w - 12.0, 7.0);
        cr.move_to(tile.x + 6.0, tile.y + 15.0);
        let _ = cr.show_text(&title);

        cr.select_font_face(
            "Sans",
            gtk::cairo::FontSlant::Normal,
            gtk::cairo::FontWeight::Normal,
        );
        cr.set_font_size(10.0);
        let subtitle = fit_label(
            &format!("{} · {pct:.1}%", format_size(tile.size)),
            tile.w - 12.0,
            6.0,
        );
        cr.move_to(tile.x + 6.0, tile.y + 31.0);
        let _ = cr.show_text(&subtitle);

        let kind = if tile.is_dir { "DIR" } else { "FILE" };
        let tag = fit_label(kind, tile.w - 12.0, 7.0);
        cr.set_font_size(9.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.84);
        cr.move_to(tile.x + 6.0, tile.y + 44.0);
        let _ = cr.show_text(&tag);
        return;
    }

    if area >= 3_600.0 && tile.w >= 90.0 && tile.h >= 24.0 {
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.16);
        rounded_rect(
            cr,
            tile.x + 1.0,
            tile.y + 1.0,
            tile.w - 2.0,
            16.0,
            (tile.w * 0.03).clamp(2.0, 6.0),
        );
        let _ = cr.fill();

        cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
        cr.select_font_face(
            "Sans",
            gtk::cairo::FontSlant::Normal,
            gtk::cairo::FontWeight::Bold,
        );
        cr.set_font_size(10.2);
        let title = fit_label(&tile.name, tile.w - 10.0, 6.5);
        cr.move_to(tile.x + 5.0, tile.y + 13.0);
        let _ = cr.show_text(&title);

        if tile.h >= 36.0 {
            cr.select_font_face(
                "Sans",
                gtk::cairo::FontSlant::Normal,
                gtk::cairo::FontWeight::Normal,
            );
            cr.set_font_size(9.0);
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.84);
            let sub = fit_label(&format_size(tile.size), tile.w - 10.0, 6.0);
            cr.move_to(tile.x + 5.0, tile.y + 27.0);
            let _ = cr.show_text(&sub);
        }
        return;
    }

    if force_compact_label || (area >= 1_150.0 && tile.w >= 52.0 && tile.h >= 16.0) {
        cr.select_font_face(
            "Sans",
            gtk::cairo::FontSlant::Normal,
            gtk::cairo::FontWeight::Normal,
        );
        cr.set_font_size(8.8);
        let compact = if tile.w >= 76.0 {
            fit_label(&format!("{} {pct:.0}%", tile.name), tile.w - 8.0, 5.6)
        } else {
            fit_label(&tile.name, tile.w - 8.0, 5.6)
        };
        if !compact.is_empty() {
            cr.move_to(tile.x + 4.0, tile.y + 11.0);
            let _ = cr.show_text(&compact);
        }
    }
}

fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    let r = radius.max(0.0).min(w / 2.0).min(h / 2.0);
    if r <= f64::EPSILON {
        cr.rectangle(x, y, w, h);
        return;
    }

    let two_pi = std::f64::consts::PI * 2.0;

    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(
        x + r,
        y + h - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        x + r,
        y + r,
        r,
        std::f64::consts::PI,
        two_pi - std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
}

fn tile_color(tile: &TreemapTile, total_size: u64) -> (f64, f64, f64) {
    const DIR_PALETTE: [(f64, f64, f64); 6] = [
        (0.19, 0.47, 0.68),
        (0.16, 0.52, 0.62),
        (0.20, 0.44, 0.74),
        (0.14, 0.56, 0.70),
        (0.17, 0.50, 0.66),
        (0.22, 0.43, 0.64),
    ];
    const FILE_PALETTE: [(f64, f64, f64); 6] = [
        (0.46, 0.33, 0.56),
        (0.52, 0.31, 0.49),
        (0.42, 0.36, 0.58),
        (0.49, 0.30, 0.53),
        (0.45, 0.35, 0.50),
        (0.40, 0.37, 0.55),
    ];

    let pct = if total_size > 0 {
        tile.size as f64 / total_size as f64
    } else {
        0.0
    };

    let depth = pct.sqrt().clamp(0.10, 0.96);
    let palette = if tile.is_dir {
        &DIR_PALETTE
    } else {
        &FILE_PALETTE
    };

    let idx = stable_hash(&tile.path) as usize % palette.len();
    let (base_r, base_g, base_b) = palette[idx];

    let lift = if tile.is_dir {
        0.04 + depth * 0.16
    } else {
        0.03 + depth * 0.13
    };

    (
        (base_r + lift).clamp(0.0, 1.0),
        (base_g + lift).clamp(0.0, 1.0),
        (base_b + lift).clamp(0.0, 1.0),
    )
}

fn stable_hash(value: &str) -> u64 {
    let mut hash = 1_469_598_103_934_665_603u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    hash
}

fn boost_color(r: f64, g: f64, b: f64, amount: f64) -> (f64, f64, f64) {
    (
        (r + amount).clamp(0.0, 1.0),
        (g + amount).clamp(0.0, 1.0),
        (b + amount).clamp(0.0, 1.0),
    )
}

fn fit_label(text: &str, max_width: f64, approx_char_px: f64) -> String {
    let max_chars = (max_width / approx_char_px).floor() as usize;

    if max_chars < 4 {
        return String::new();
    }

    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }

    let keep = max_chars.saturating_sub(1);
    let prefix: String = chars.into_iter().take(keep).collect();
    format!("{}…", prefix)
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn display_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|v| v.to_str())
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| path.to_string())
}

fn compact_path_for_label(path: &str, max_chars: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= max_chars || max_chars < 10 {
        return path.to_string();
    }

    let keep_head = max_chars / 2 - 2;
    let keep_tail = max_chars.saturating_sub(keep_head + 1);
    let head: String = chars.iter().take(keep_head).collect();
    let tail: String = chars
        .iter()
        .rev()
        .take(keep_tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

fn open_target_path(path: &str, is_dir: bool) -> String {
    if is_dir {
        normalize_path(path)
    } else {
        parent_path(path)
    }
}

fn show_disk_entry_details_dialog(
    anchor: &impl IsA<gtk::Widget>,
    entry: &disk::FolderUsage,
    shown_size: u64,
    child_count: Option<usize>,
    lang: Language,
) {
    let kind = if entry.is_dir {
        pick(lang, "目录", "Directory")
    } else {
        pick(lang, "文件", "File")
    };
    let share = if shown_size > 0 {
        entry.size as f64 * 100.0 / shown_size as f64
    } else {
        0.0
    };

    let mut body = match lang {
        Language::ZhCn => format!(
            "名称：{}\n类型：{}\n路径：{}\n大小：{}\n可见占比：{share:.1}%",
            entry.name,
            kind,
            entry.path,
            format_size(entry.size),
        ),
        Language::En => format!(
            "Name: {}\nType: {}\nPath: {}\nSize: {}\nVisible share: {share:.1}%",
            entry.name,
            kind,
            entry.path,
            format_size(entry.size),
        ),
    };

    if entry.is_dir {
        let line = match lang {
            Language::ZhCn => format!("\n直接子项数：{}", child_count.unwrap_or(0)),
            Language::En => format!("\nDirect children: {}", child_count.unwrap_or(0)),
        };
        body.push_str(&line);
    }

    let heading = gtk::Label::new(Some(if entry.is_dir {
        pick(lang, "文件夹详细信息", "Folder Details")
    } else {
        pick(lang, "文件详细信息", "File Details")
    }));
    heading.set_halign(gtk::Align::Start);
    heading.add_css_class("title-4");

    let body_label = gtk::Label::new(Some(&body));
    body_label.set_halign(gtk::Align::Start);
    body_label.set_wrap(true);
    body_label.set_selectable(true);
    body_label.set_xalign(0.0);
    body_label.set_yalign(0.0);
    body_label.add_css_class("monospace");

    let close_btn = gtk::Button::with_label(pick(lang, "关闭", "Close"));
    close_btn.set_halign(gtk::Align::End);
    close_btn.add_css_class("flat");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(10);
    content.set_margin_end(10);
    content.append(&heading);
    content.append(&body_label);
    content.append(&close_btn);

    let popover = gtk::Popover::builder()
        .has_arrow(true)
        .autohide(true)
        .position(gtk::PositionType::Bottom)
        .build();
    popover.set_parent(anchor);
    popover.set_child(Some(&content));
    popover.connect_closed(|popover| {
        popover.unparent();
    });

    {
        let popover = popover.clone();
        close_btn.connect_clicked(move |_| {
            popover.popdown();
        });
    }

    popover.popup();
}

fn show_disk_entry_context_menu(
    anchor: &impl IsA<gtk::Widget>,
    x: f64,
    y: f64,
    entry: disk::FolderUsage,
    shown_size: u64,
    child_count: Option<usize>,
    lang: Language,
) {
    let anchor_widget: gtk::Widget = anchor.as_ref().clone();

    let popover = gtk::Popover::builder()
        .has_arrow(true)
        .autohide(true)
        .build();
    popover.set_parent(&anchor_widget);
    popover.connect_closed(|popover| {
        popover.unparent();
    });

    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(6);
    content.set_margin_end(6);

    let open_btn = gtk::Button::with_label(if entry.is_dir {
        pick(lang, "打开文件夹", "Open folder")
    } else {
        pick(lang, "打开所在文件夹", "Open containing folder")
    });
    open_btn.add_css_class("flat");
    open_btn.set_halign(gtk::Align::Start);

    let detail_btn = gtk::Button::with_label(pick(lang, "显示详细信息", "Show details"));
    detail_btn.add_css_class("flat");
    detail_btn.set_halign(gtk::Align::Start);

    content.append(&open_btn);
    content.append(&detail_btn);
    popover.set_child(Some(&content));

    {
        let popover = popover.clone();
        let entry = entry.clone();
        open_btn.connect_clicked(move |_| {
            popover.popdown();
            let target = open_target_path(&entry.path, entry.is_dir);
            if let Err(e) = open_path_in_file_manager(&target) {
                tracing::warn!("failed to open path '{}': {e}", target);
            }
        });
    }

    {
        let popover = popover.clone();
        let anchor_widget = anchor_widget.clone();
        detail_btn.connect_clicked(move |_| {
            popover.popdown();
            show_disk_entry_details_dialog(&anchor_widget, &entry, shown_size, child_count, lang);
        });
    }

    let rect = gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
    popover.set_pointing_to(Some(&rect));
    popover.popup();
}

fn open_path_in_file_manager(path: &str) -> Result<(), std::io::Error> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
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

fn format_disk_progress(progress: &disk::DiskProgress, lang: Language) -> String {
    let elapsed = format_duration_ms(progress.elapsed_ms);
    let eta = progress
        .eta_ms
        .filter(|v| *v > 0)
        .map(format_duration_ms);

    match progress.stage {
        disk::DiskStage::ScanningCaches => {
            let current = progress.current.as_deref().unwrap_or("-");
            match lang {
                Language::ZhCn => format!("扫描缓存：{current} · 已耗时 {elapsed}"),
                Language::En => format!("Scanning caches: {current} · Elapsed {elapsed}"),
            }
        }
        disk::DiskStage::AnalyzingRoots => {
            let count = if progress.total > 0 && progress.total != u32::MAX {
                format!("{}/{}", progress.done, progress.total)
            } else {
                "-".to_string()
            };

            let current_part = progress
                .current
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
        disk::DiskStage::Finished => match lang {
            Language::ZhCn => format!("完成 · 总耗时 {elapsed}"),
            Language::En => format!("Done · Elapsed {elapsed}"),
        },
    }
}
