use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Once;
use std::time::{Duration, Instant};

use crate::i18n::{pick, Language};
use crate::runtime;
use crate::services::process_manager;
use crate::services::process_manager::{ProcessInfo, TerminateError, TerminateSignal};

static SCAN_SEQ: AtomicU64 = AtomicU64::new(1);
static PROCESS_STYLE_ONCE: Once = Once::new();

const PROCESS_PAGE_CSS: &str = r#"
.pm-hero {
  padding: 12px 12px;
  border-radius: 16px;
  background: alpha(@accent_color, 0.095);
  border: 1px solid alpha(@accent_color, 0.18);
  box-shadow: 0 1px 2px alpha(@window_fg_color, 0.08);
}

.pm-hero-title {
  font-weight: 800;
  font-size: 1.18em;
}

.pm-hero-subtitle {
  opacity: 0.78;
  font-size: 0.95em;
}

.pm-card {
  padding: 10px 12px;
  border-radius: 14px;
  background: alpha(@window_fg_color, 0.035);
  border: 1px solid alpha(@window_fg_color, 0.08);
  box-shadow: 0 1px 2px alpha(@window_fg_color, 0.06);
}

.pm-card-title {
  opacity: 0.80;
  font-size: 0.92em;
}

.pm-card-value {
  font-weight: 760;
  font-size: 1.20em;
}

.pm-card-sub {
  opacity: 0.74;
  font-size: 0.92em;
}

.pm-chip {
  border-radius: 9999px;
  padding: 3px 10px;
  background: alpha(@window_fg_color, 0.030);
  border: 1px solid alpha(@window_fg_color, 0.10);
}

.pm-chip:checked {
  background: alpha(@accent_color, 0.22);
  border-color: alpha(@accent_color, 0.35);
}

.pm-badge {
  padding: 4px 10px;
  border-radius: 9999px;
  background: alpha(@accent_color, 0.11);
  border: 1px solid alpha(@accent_color, 0.18);
  font-size: 0.92em;
}

.pm-confirm {
  padding: 8px 10px;
  border-radius: 12px;
  background: alpha(@window_fg_color, 0.035);
  border: 1px solid alpha(@window_fg_color, 0.08);
}

.pm-footer {
  padding: 8px 10px;
  border-radius: 12px;
  background: alpha(@window_fg_color, 0.030);
  border: 1px solid alpha(@window_fg_color, 0.08);
}

.pm-usage-bar {
  min-width: 160px;
}

.pm-usage-bar trough,
.pm-usage-bar progress {
  min-height: 7px;
  border-radius: 999px;
}

.pm-usage-bar-high progress {
  background: #e95420;
}

.pm-usage-bar-mid progress {
  background: #f6ad55;
}

.pm-usage-bar-low progress {
  background: #4fd1c5;
}

.pm-row-rss {
  font-weight: 650;
}
"#;

fn ensure_process_style() {
    PROCESS_STYLE_ONCE.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(PROCESS_PAGE_CSS);

        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
}

pub fn build(token: tokio_util::sync::CancellationToken, lang: Language) -> adw::NavigationPage {
    ensure_process_style();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let banner = adw::Banner::new("");
    banner.set_revealed(false);
    vbox.append(&banner);

    let hero = gtk::Box::new(gtk::Orientation::Vertical, 10);
    hero.set_margin_top(10);
    hero.set_margin_bottom(6);
    hero.set_margin_start(12);
    hero.set_margin_end(12);
    hero.add_css_class("pm-hero");

    let hero_header = gtk::Box::new(gtk::Orientation::Horizontal, 12);

    let hero_icon = gtk::Image::from_icon_name("speedometer-symbolic");
    hero_icon.set_pixel_size(34);
    hero_icon.set_valign(gtk::Align::Start);

    let hero_text = gtk::Box::new(gtk::Orientation::Vertical, 4);
    hero_text.set_hexpand(true);

    let hero_title = gtk::Label::new(Some(pick(lang, "内存加速", "Memory Boost")));
    hero_title.set_halign(gtk::Align::Start);
    hero_title.add_css_class("pm-hero-title");

    let hero_subtitle = gtk::Label::new(Some(pick(
        lang,
        "扫描后台进程，选择后结束以释放内存，让系统更流畅",
        "Scan background processes and terminate selected ones to free memory",
    )));
    hero_subtitle.set_halign(gtk::Align::Start);
    hero_subtitle.set_wrap(true);
    hero_subtitle.add_css_class("pm-hero-subtitle");

    hero_text.append(&hero_title);
    hero_text.append(&hero_subtitle);

    let boost_content = adw::ButtonContent::builder()
        .icon_name("media-playback-start-symbolic")
        .label(pick(lang, "一键加速", "Boost"))
        .build();
    let boost_btn = gtk::Button::new();
    boost_btn.set_child(Some(&boost_content));
    boost_btn.add_css_class("suggested-action");
    boost_btn.set_sensitive(false);

    hero_header.append(&hero_icon);
    hero_header.append(&hero_text);
    hero_header.append(&boost_btn);
    hero.append(&hero_header);

    let metrics = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    metrics.set_homogeneous(true);

    let mem_card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    mem_card.add_css_class("pm-card");
    let mem_head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let mem_title = gtk::Label::new(Some(pick(lang, "内存", "Memory")));
    mem_title.set_halign(gtk::Align::Start);
    mem_title.add_css_class("pm-card-title");
    let mem_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    mem_spacer.set_hexpand(true);
    let mem_badge = gtk::Label::new(Some("-"));
    mem_badge.add_css_class("pm-badge");
    mem_head.append(&mem_title);
    mem_head.append(&mem_spacer);
    mem_head.append(&mem_badge);
    let mem_value = gtk::Label::new(Some("-"));
    mem_value.set_halign(gtk::Align::Start);
    mem_value.add_css_class("pm-card-value");
    let mem_sub = gtk::Label::new(Some("-"));
    mem_sub.set_halign(gtk::Align::Start);
    mem_sub.add_css_class("pm-card-sub");
    let mem_bar = gtk::ProgressBar::new();
    mem_bar.add_css_class("pm-usage-bar");
    mem_card.append(&mem_head);
    mem_card.append(&mem_value);
    mem_card.append(&mem_sub);
    mem_card.append(&mem_bar);

    let swap_card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    swap_card.add_css_class("pm-card");
    let swap_head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let swap_title = gtk::Label::new(Some(pick(lang, "交换分区", "Swap")));
    swap_title.set_halign(gtk::Align::Start);
    swap_title.add_css_class("pm-card-title");
    let swap_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    swap_spacer.set_hexpand(true);
    let swap_badge = gtk::Label::new(Some("-"));
    swap_badge.add_css_class("pm-badge");
    swap_head.append(&swap_title);
    swap_head.append(&swap_spacer);
    swap_head.append(&swap_badge);
    let swap_value = gtk::Label::new(Some("-"));
    swap_value.set_halign(gtk::Align::Start);
    swap_value.add_css_class("pm-card-value");
    let swap_sub = gtk::Label::new(Some("-"));
    swap_sub.set_halign(gtk::Align::Start);
    swap_sub.add_css_class("pm-card-sub");
    let swap_bar = gtk::ProgressBar::new();
    swap_bar.add_css_class("pm-usage-bar");
    swap_card.append(&swap_head);
    swap_card.append(&swap_value);
    swap_card.append(&swap_sub);
    swap_card.append(&swap_bar);

    metrics.append(&mem_card);
    metrics.append(&swap_card);
    hero.append(&metrics);

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);

    let search_entry = gtk::SearchEntry::new();
    search_entry.set_placeholder_text(Some(pick(lang, "搜索进程...", "Search processes...")));
    search_entry.set_hexpand(true);

    let scope_toggle = gtk::ToggleButton::with_label(scope_toggle_label(lang, false));
    scope_toggle.add_css_class("pm-chip");
    scope_toggle.set_active(false);
    scope_toggle.set_tooltip_text(Some(pick(
        lang,
        "切换显示范围：仅可结束 ↔ 全部（不可结束的会置灰）",
        "Toggle scope: terminatable ↔ all (non-terminatable will be dimmed)",
    )));

    let rescan_content = adw::ButtonContent::builder()
        .icon_name("view-refresh-symbolic")
        .label(pick(lang, "重新扫描", "Rescan"))
        .build();
    let rescan_btn = gtk::Button::new();
    rescan_btn.set_child(Some(&rescan_content));
    rescan_btn.add_css_class("flat");

    let progress_badge = gtk::Label::new(Some(pick(lang, "准备中", "Preparing")));
    progress_badge.add_css_class("pm-badge");
    progress_badge.add_css_class("dim-label");

    controls.append(&search_entry);
    controls.append(&scope_toggle);
    controls.append(&rescan_btn);
    controls.append(&progress_badge);
    hero.append(&controls);

    vbox.append(&hero);

    let confirm_revealer = gtk::Revealer::new();
    confirm_revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    confirm_revealer.set_reveal_child(false);

    let confirm_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    confirm_box.set_margin_top(6);
    confirm_box.set_margin_bottom(6);
    confirm_box.set_margin_start(12);
    confirm_box.set_margin_end(12);
    confirm_box.add_css_class("pm-confirm");

    let confirm_label = gtk::Label::new(None);
    confirm_label.set_halign(gtk::Align::Start);
    confirm_label.set_hexpand(true);

    let confirm_cancel_btn = gtk::Button::with_label(pick(lang, "取消", "Cancel"));
    confirm_cancel_btn.add_css_class("flat");
    let confirm_ok_btn = gtk::Button::with_label(pick(lang, "确认结束", "Terminate"));
    confirm_ok_btn.add_css_class("destructive-action");

    confirm_box.append(&confirm_label);
    confirm_box.append(&confirm_cancel_btn);
    confirm_box.append(&confirm_ok_btn);
    confirm_revealer.set_child(Some(&confirm_box));
    vbox.append(&confirm_revealer);

    let spinner = gtk::Spinner::new();
    spinner.set_spinning(true);
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.set_vexpand(true);

    let loading_page = adw::StatusPage::builder()
        .title(pick(lang, "正在扫描进程...", "Scanning processes..."))
        .child(&spinner)
        .build();

    let empty_page = adw::StatusPage::builder()
        .title(pick(lang, "未发现进程", "No processes found"))
        .description(pick(lang, "请尝试重新扫描", "Try rescan"))
        .icon_name("utilities-terminal-symbolic")
        .build();

    let content_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content_box.set_margin_top(6);
    content_box.set_margin_bottom(12);
    content_box.set_margin_start(12);
    content_box.set_margin_end(12);

    let list_title = gtk::Label::new(Some(pick(lang, "后台进程", "Background processes")));
    list_title.set_halign(gtk::Align::Start);
    list_title.add_css_class("caption");
    content_box.append(&list_title);

    let list = create_process_list_box();
    let list_holder = Rc::new(RefCell::new(list.clone()));

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    content_box.append(&scrolled);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let summary_label = gtk::Label::new(Some(pick(lang, "等待数据", "Waiting data")));
    summary_label.set_halign(gtk::Align::Start);
    summary_label.set_hexpand(true);
    summary_label.add_css_class("caption");

    let term_btn = gtk::Button::with_label(pick(lang, "结束选中", "Terminate selected"));
    term_btn.add_css_class("destructive-action");
    term_btn.set_sensitive(false);

    let kill_btn = gtk::Button::with_label(pick(lang, "强制结束", "Force kill"));
    kill_btn.add_css_class("destructive-action");
    kill_btn.set_sensitive(false);

    footer.add_css_class("pm-footer");
    footer.append(&summary_label);
    footer.append(&term_btn);
    footer.append(&kill_btn);
    content_box.append(&footer);

    let stack = gtk::Stack::new();
    stack.add_named(&loading_page, Some("loading"));
    stack.add_named(&empty_page, Some("empty"));
    stack.add_named(&content_box, Some("content"));
    vbox.append(&stack);

    let current_uid = process_manager::current_uid();
    let self_pid = process_manager::self_pid();

    let (tx, rx) = async_channel::bounded::<process_manager::ProcessScanEvent>(8);
    let tx_for_rescan = tx.clone();

    let active_scan_id = Rc::new(RefCell::new(0_u64));
    let processes_state = Rc::new(RefCell::new(Vec::<ProcessInfo>::new()));
    let memory_state = Rc::new(RefCell::new(process_manager::MemorySnapshot::default()));
    let selection_state = Rc::new(RefCell::new(HashSet::<u32>::new()));
    let show_all_state = Rc::new(Cell::new(false));
    let pending_signal = Rc::new(Cell::new(TerminateSignal::Term));
    let pending_targets = Rc::new(RefCell::new(Vec::<u32>::new()));
    let render_seq = Rc::new(Cell::new(1_u64));
    let scan_in_progress = Rc::new(Cell::new(false));

    let update_overview: Rc<dyn Fn()> = Rc::new({
        let memory_state = memory_state.clone();
        let mem_badge = mem_badge.clone();
        let mem_value = mem_value.clone();
        let mem_sub = mem_sub.clone();
        let mem_bar = mem_bar.clone();
        let swap_badge = swap_badge.clone();
        let swap_value = swap_value.clone();
        let swap_sub = swap_sub.clone();
        let swap_bar = swap_bar.clone();
        move || {
            let m = memory_state.borrow().clone();

            if let (Some(total), Some(avail)) = (m.mem_total, m.mem_available) {
                if total > 0 {
                    let used = total.saturating_sub(avail);
                    let fraction = used as f64 / total as f64;
                    mem_value.set_label(&format!("{:.0}%", fraction * 100.0));
                    mem_sub.set_label(&format_memory_overview(&m, lang));
                    mem_badge.set_label(usage_level_label(lang, fraction, 0.85, 0.70));
                    set_pm_usage_bar(&mem_bar, fraction, 0.85, 0.70);
                } else {
                    mem_value.set_label("-");
                    mem_sub.set_label("-");
                    mem_badge.set_label("-");
                    set_pm_usage_bar(&mem_bar, 0.0, 0.85, 0.70);
                }
            } else {
                mem_value.set_label("-");
                mem_sub.set_label("-");
                mem_badge.set_label("-");
                set_pm_usage_bar(&mem_bar, 0.0, 0.85, 0.70);
            }

            match m.swap_total {
                Some(total) if total > 0 => {
                    let used = m.swap_used().unwrap_or(0);
                    let fraction = used as f64 / total as f64;
                    swap_value.set_label(&format!("{:.0}%", fraction * 100.0));
                    swap_sub.set_label(&format_swap_overview(&m));
                    swap_badge.set_label(usage_level_label(lang, fraction, 0.50, 0.25));
                    set_pm_usage_bar(&swap_bar, fraction, 0.50, 0.25);
                }
                Some(_) => {
                    swap_value.set_label(pick(lang, "未启用", "Disabled"));
                    swap_sub.set_label(pick(lang, "系统未配置交换分区", "No swap configured"));
                    swap_badge.set_label(pick(lang, "关闭", "Off"));
                    set_pm_usage_bar(&swap_bar, 0.0, 0.50, 0.25);
                }
                None => {
                    swap_value.set_label("-");
                    swap_sub.set_label("-");
                    swap_badge.set_label("-");
                    set_pm_usage_bar(&swap_bar, 0.0, 0.50, 0.25);
                }
            }
        }
    });

    let update_summary: Rc<dyn Fn()> = Rc::new({
        let processes_state = processes_state.clone();
        let selection_state = selection_state.clone();
        let summary_label = summary_label.clone();
        let term_btn = term_btn.clone();
        let kill_btn = kill_btn.clone();
        move || {
            let selected = selection_state.borrow();
            let mut count = 0usize;
            let mut total_rss: u64 = 0;

            for p in processes_state.borrow().iter() {
                if !selected.contains(&p.pid) {
                    continue;
                }
                count += 1;
                if let Some(rss) = p.rss_bytes {
                    total_rss = total_rss.saturating_add(rss);
                }
            }

            let label = match lang {
                Language::ZhCn => {
                    format!("已选择 {count} 项 · RSS 合计 {}", format_size(total_rss))
                }
                Language::En => format!("Selected {count} · total RSS {}", format_size(total_rss)),
            };
            summary_label.set_label(&label);
            let has_any = count > 0;
            term_btn.set_sensitive(has_any);
            kill_btn.set_sensitive(has_any);
        }
    });

    let rebuild_list: Rc<dyn Fn()> = Rc::new({
        let scrolled = scrolled.clone();
        let list_holder = list_holder.clone();
        let processes_state = processes_state.clone();
        let selection_state = selection_state.clone();
        let memory_state = memory_state.clone();
        let show_all_state = show_all_state.clone();
        let render_seq = render_seq.clone();
        let query_state = Rc::new(RefCell::new(String::new()));
        let update_summary = update_summary.clone();

        {
            let query_state = query_state.clone();
            let memory_state = memory_state.clone();
            let show_all_state = show_all_state.clone();
            let render_seq = render_seq.clone();
            let processes_state = processes_state.clone();
            let selection_state = selection_state.clone();
            let scrolled = scrolled.clone();
            let list_holder = list_holder.clone();
            let update_summary = update_summary.clone();

            search_entry.connect_search_changed(move |entry| {
                *query_state.borrow_mut() = entry.text().to_string().to_lowercase();
                render_seq.set(render_seq.get().saturating_add(1));
                let mem_total = memory_state.borrow().mem_total;
                render_process_list(
                    &scrolled,
                    &list_holder,
                    &processes_state,
                    &selection_state,
                    query_state.borrow().as_str(),
                    current_uid,
                    self_pid,
                    show_all_state.get(),
                    mem_total,
                    lang,
                    render_seq.get(),
                    render_seq.clone(),
                    update_summary.clone(),
                );
            });
        }

        move || {
            render_seq.set(render_seq.get().saturating_add(1));
            let q = query_state.borrow();
            let mem_total = memory_state.borrow().mem_total;
            render_process_list(
                &scrolled,
                &list_holder,
                &processes_state,
                &selection_state,
                q.as_str(),
                current_uid,
                self_pid,
                show_all_state.get(),
                mem_total,
                lang,
                render_seq.get(),
                render_seq.clone(),
                update_summary.clone(),
            );
        }
    });

    {
        let show_all_state = show_all_state.clone();
        let rebuild_list = rebuild_list.clone();
        scope_toggle.connect_toggled(move |btn| {
            let active = btn.is_active();
            show_all_state.set(active);
            btn.set_label(scope_toggle_label(lang, active));
            rebuild_list();
        });
    }

    let start_scan: Rc<dyn Fn()> = Rc::new({
        let token = token.clone();
        let active_scan_id = active_scan_id.clone();
        let processes_state = processes_state.clone();
        let selection_state = selection_state.clone();
        let memory_state = memory_state.clone();
        let stack = stack.clone();
        let banner = banner.clone();
        let confirm_revealer = confirm_revealer.clone();
        let pending_targets = pending_targets.clone();
        let progress_badge = progress_badge.clone();
        let boost_btn = boost_btn.clone();
        let scan_in_progress = scan_in_progress.clone();
        let update_overview = update_overview.clone();
        let rebuild_list = rebuild_list.clone();
        let update_summary = update_summary.clone();
        let tx_for_start = tx_for_rescan.clone();

        move || {
            banner.set_revealed(false);
            confirm_revealer.set_reveal_child(false);
            pending_targets.borrow_mut().clear();
            selection_state.borrow_mut().clear();
            processes_state.borrow_mut().clear();
            *memory_state.borrow_mut() = process_manager::MemorySnapshot::default();
            update_overview();
            update_summary();

            let scan_id = SCAN_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            *active_scan_id.borrow_mut() = scan_id;
            scan_in_progress.set(true);

            progress_badge.set_label(pick(lang, "扫描中...", "Scanning..."));
            boost_btn.set_sensitive(false);
            stack.set_visible_child_name("loading");

            let tx_clone = tx_for_start.clone();
            let token_clone = token.clone();
            runtime::spawn(async move {
                process_manager::scan_all(tx_clone, token_clone, scan_id).await;
            });

            // 先清空列表，避免旧数据残留。
            rebuild_list();
        }
    });

    let refresh_scan: Rc<dyn Fn()> = Rc::new({
        let token = token.clone();
        let tx_for_rescan = tx_for_rescan.clone();
        let active_scan_id = active_scan_id.clone();
        let progress_badge = progress_badge.clone();
        let boost_btn = boost_btn.clone();
        let scan_in_progress = scan_in_progress.clone();
        move || {
            if scan_in_progress.get() || token.is_cancelled() {
                return;
            }

            let scan_id = SCAN_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            *active_scan_id.borrow_mut() = scan_id;
            scan_in_progress.set(true);

            progress_badge.set_label(pick(lang, "更新中...", "Updating..."));
            boost_btn.set_sensitive(false);

            let tx_clone = tx_for_rescan.clone();
            let token_clone = token.clone();
            runtime::spawn(async move {
                process_manager::scan_all(tx_clone, token_clone, scan_id).await;
            });
        }
    });

    {
        let start_scan = start_scan.clone();
        rescan_btn.connect_clicked(move |_| start_scan());
    }

    boost_btn.connect_clicked({
        let banner = banner.clone();
        let confirm_revealer = confirm_revealer.clone();
        let confirm_label = confirm_label.clone();
        let processes_state = processes_state.clone();
        let selection_state = selection_state.clone();
        let pending_signal = pending_signal.clone();
        let pending_targets = pending_targets.clone();
        let scope_toggle = scope_toggle.clone();
        let rebuild_list = rebuild_list.clone();
        let update_summary = update_summary.clone();
        move |_| {
            // 切回“仅可结束”，减少噪音，便于用户确认即将结束的目标。
            scope_toggle.set_active(false);

            let mut candidates: Vec<(u32, u64)> = processes_state
                .borrow()
                .iter()
                .filter_map(|p| {
                    if !process_manager::can_terminate(current_uid, self_pid, p) {
                        return None;
                    }
                    let rss = p.rss_bytes?;
                    if rss < 50 * 1024 * 1024 {
                        return None;
                    }
                    if is_boost_protected_process(p) {
                        return None;
                    }
                    Some((p.pid, rss))
                })
                .collect();
            candidates.sort_by(|a, b| b.1.cmp(&a.1));

            let targets: Vec<u32> = candidates.into_iter().take(6).map(|(pid, _)| pid).collect();

            if targets.is_empty() {
                banner.set_title(pick(
                    lang,
                    "没有可推荐的一键加速目标（已避开桌面/音频等关键进程）",
                    "No recommended boost targets (critical processes are excluded)",
                ));
                banner.set_revealed(true);
                return;
            }

            {
                let mut selected = selection_state.borrow_mut();
                selected.clear();
                for pid in targets.iter().copied() {
                    selected.insert(pid);
                }
            }

            pending_signal.set(TerminateSignal::Term);
            *pending_targets.borrow_mut() = targets.clone();

            rebuild_list();
            update_summary();

            confirm_label.set_label(&confirm_text(&targets, TerminateSignal::Term, lang));
            confirm_revealer.set_reveal_child(true);
        }
    });

    {
        let token = token.clone();
        let vbox = vbox.clone();
        let memory_state = memory_state.clone();
        let update_overview = update_overview.clone();
        glib::timeout_add_local(Duration::from_secs(2), move || {
            if token.is_cancelled() {
                return glib::ControlFlow::Break;
            }
            if !vbox.is_visible() {
                return glib::ControlFlow::Continue;
            }
            *memory_state.borrow_mut() = process_manager::read_memory_snapshot();
            update_overview();
            glib::ControlFlow::Continue
        });
    }

    {
        let token = token.clone();
        let vbox = vbox.clone();
        let scrolled = scrolled.clone();
        let search_entry = search_entry.clone();
        let refresh_scan = refresh_scan.clone();
        glib::timeout_add_local(Duration::from_secs(4), move || {
            if token.is_cancelled() {
                return glib::ControlFlow::Break;
            }
            if !vbox.is_visible() {
                return glib::ControlFlow::Continue;
            }
            if search_entry.has_focus() {
                return glib::ControlFlow::Continue;
            }
            if scrolled.vadjustment().value() > 1.0 {
                return glib::ControlFlow::Continue;
            }
            refresh_scan();
            glib::ControlFlow::Continue
        });
    }

    start_scan();

    {
        let confirm_revealer = confirm_revealer.clone();
        confirm_cancel_btn.connect_clicked(move |_| confirm_revealer.set_reveal_child(false));
    }

    term_btn.connect_clicked({
        let banner = banner.clone();
        let confirm_revealer = confirm_revealer.clone();
        let confirm_label = confirm_label.clone();
        let processes_state = processes_state.clone();
        let selection_state = selection_state.clone();
        let pending_signal = pending_signal.clone();
        let pending_targets = pending_targets.clone();
        move |_| {
            let targets = selected_pids(&processes_state, &selection_state);
            if targets.is_empty() {
                banner.set_title(pick(lang, "未选择任何进程", "Nothing selected"));
                banner.set_revealed(true);
                return;
            }
            pending_signal.set(TerminateSignal::Term);
            *pending_targets.borrow_mut() = targets.clone();
            confirm_label.set_label(&confirm_text(&targets, TerminateSignal::Term, lang));
            confirm_revealer.set_reveal_child(true);
        }
    });

    kill_btn.connect_clicked({
        let banner = banner.clone();
        let confirm_revealer = confirm_revealer.clone();
        let confirm_label = confirm_label.clone();
        let processes_state = processes_state.clone();
        let selection_state = selection_state.clone();
        let pending_signal = pending_signal.clone();
        let pending_targets = pending_targets.clone();
        move |_| {
            let targets = selected_pids(&processes_state, &selection_state);
            if targets.is_empty() {
                banner.set_title(pick(lang, "未选择任何进程", "Nothing selected"));
                banner.set_revealed(true);
                return;
            }
            pending_signal.set(TerminateSignal::Kill);
            *pending_targets.borrow_mut() = targets.clone();
            confirm_label.set_label(&confirm_text(&targets, TerminateSignal::Kill, lang));
            confirm_revealer.set_reveal_child(true);
        }
    });

    confirm_ok_btn.connect_clicked({
        let confirm_revealer = confirm_revealer.clone();
        let banner = banner.clone();
        let progress_badge = progress_badge.clone();
        let rescan_btn = rescan_btn.clone();
        let search_entry = search_entry.clone();
        let scope_toggle = scope_toggle.clone();
        let boost_btn = boost_btn.clone();
        let term_btn = term_btn.clone();
        let kill_btn = kill_btn.clone();
        let pending_signal = pending_signal.clone();
        let pending_targets = pending_targets.clone();
        let processes_state = processes_state.clone();
        let selection_state = selection_state.clone();
        let rebuild_list = rebuild_list.clone();
        let update_summary = update_summary.clone();
        let refresh_scan = refresh_scan.clone();
        move |_| {
            confirm_revealer.set_reveal_child(false);
            let signal = pending_signal.get();
            let targets = pending_targets.borrow().clone();
            if targets.is_empty() {
                return;
            }

            // 执行期间禁用交互，避免重复触发或引入并发结束操作。
            rescan_btn.set_sensitive(false);
            search_entry.set_sensitive(false);
            scope_toggle.set_sensitive(false);
            boost_btn.set_sensitive(false);
            term_btn.set_sensitive(false);
            kill_btn.set_sensitive(false);

            progress_badge.set_label(pick(lang, "执行中...", "Running..."));

            let (run_tx, run_rx) = async_channel::bounded::<TerminateEvent>(32);
            runtime::spawn(async move {
                terminate_selected(run_tx, targets, signal, current_uid, self_pid, lang).await;
            });

            let banner = banner.clone();
            let progress_badge = progress_badge.clone();
            let rescan_btn = rescan_btn.clone();
            let search_entry = search_entry.clone();
            let scope_toggle = scope_toggle.clone();
            let boost_btn = boost_btn.clone();
            let processes_state = processes_state.clone();
            let selection_state = selection_state.clone();
            let rebuild_list = rebuild_list.clone();
            let update_summary = update_summary.clone();
            let refresh_scan = refresh_scan.clone();
            glib::spawn_future_local(async move {
                while let Ok(event) = run_rx.recv().await {
                    match event {
                        TerminateEvent::Progress {
                            current,
                            total,
                            pid,
                        } => {
                            progress_badge.set_label(&match lang {
                                Language::ZhCn => format!("执行中（{current}/{total}）"),
                                Language::En => format!("Running ({current}/{total})"),
                            });
                            banner.set_title(&match lang {
                                Language::ZhCn => format!("正在结束 PID={pid}"),
                                Language::En => format!("Terminating PID={pid}"),
                            });
                            banner.set_revealed(true);
                        }
                        TerminateEvent::Finished {
                            terminated,
                            still_running,
                            failed,
                            not_found,
                            removed_pids,
                        } => {
                            let title = match lang {
                                Language::ZhCn => format!("执行完成：已结束 {terminated} · 仍在运行 {still_running} · 失败 {failed} · 已退出 {not_found}"),
                                Language::En => format!(
                                    "Done: terminated {terminated}, still running {still_running}, failed {failed}, not found {not_found}"
                                ),
                            };
                            banner.set_title(&title);
                            banner.set_revealed(true);

                            rescan_btn.set_sensitive(true);
                            search_entry.set_sensitive(true);
                            scope_toggle.set_sensitive(true);
                            boost_btn.set_sensitive(true);
                            progress_badge.set_label(pick(lang, "执行完成", "Done"));

                            if !removed_pids.is_empty() {
                                let removed: HashSet<u32> = removed_pids.into_iter().collect();
                                processes_state
                                    .borrow_mut()
                                    .retain(|p| !removed.contains(&p.pid));
                                selection_state.borrow_mut().retain(|pid| !removed.contains(pid));
                                rebuild_list();
                                update_summary();
                            } else {
                                update_summary();
                            }

                            refresh_scan();
                            break;
                        }
                        TerminateEvent::Message { text } => {
                            banner.set_title(&text);
                            banner.set_revealed(true);
                        }
                    }
                }
            });
        }
    });

    glib::spawn_future_local({
        let active_scan_id = active_scan_id.clone();
        let processes_state = processes_state.clone();
        let memory_state = memory_state.clone();
        let selection_state = selection_state.clone();
        let progress_badge = progress_badge.clone();
        let stack = stack.clone();
        let boost_btn = boost_btn.clone();
        let scan_in_progress = scan_in_progress.clone();
        let update_overview = update_overview.clone();
        let rebuild_list = rebuild_list.clone();
        let update_summary = update_summary.clone();

        async move {
            while let Ok(event) = rx.recv().await {
                if event.scan_id != *active_scan_id.borrow() {
                    continue;
                }
                scan_in_progress.set(false);

                *memory_state.borrow_mut() = event.memory;
                *processes_state.borrow_mut() = event.processes;
                // 剔除已不存在的选择项，避免对新 PID 误操作。
                let existing: HashSet<u32> =
                    processes_state.borrow().iter().map(|p| p.pid).collect();
                selection_state
                    .borrow_mut()
                    .retain(|pid| existing.contains(pid));

                update_overview();
                rebuild_list();
                update_summary();

                progress_badge.set_label(pick(lang, "已更新", "Updated"));
                boost_btn.set_sensitive(!processes_state.borrow().is_empty());
                if processes_state.borrow().is_empty() {
                    stack.set_visible_child_name("empty");
                } else {
                    stack.set_visible_child_name("content");
                }
            }
        }
    });

    adw::NavigationPage::builder()
        .title(pick(lang, "进程管理", "Process Manager"))
        .child(&vbox)
        .build()
}

enum TerminateEvent {
    Message {
        text: String,
    },
    Progress {
        current: usize,
        total: usize,
        pid: u32,
    },
    Finished {
        terminated: usize,
        still_running: usize,
        failed: usize,
        not_found: usize,
        removed_pids: Vec<u32>,
    },
}

async fn terminate_selected(
    tx: async_channel::Sender<TerminateEvent>,
    pids: Vec<u32>,
    signal: TerminateSignal,
    current_uid: u32,
    self_pid: u32,
    lang: Language,
) {
    let total = pids.len();
    let mut failed = 0usize;
    let mut not_found = 0usize;
    let mut signal_sent: Vec<u32> = Vec::new();
    let mut removed_pids: Vec<u32> = Vec::new();

    for (idx, pid) in pids.iter().copied().enumerate() {
        let current = idx + 1;
        let _ = tx
            .send(TerminateEvent::Progress {
                current,
                total,
                pid,
            })
            .await;

        let result = tokio::task::spawn_blocking(move || {
            process_manager::terminate_process(pid, signal, current_uid, self_pid)
        })
        .await;

        let outcome = match result {
            Ok(v) => v,
            Err(e) => Err(TerminateError::System(e.to_string())),
        };

        match outcome {
            Ok(()) => signal_sent.push(pid),
            Err(TerminateError::NotFound) => {
                not_found += 1;
                removed_pids.push(pid);
            }
            Err(e) => {
                failed += 1;
                let _ = tx
                    .send(TerminateEvent::Message {
                        text: match lang {
                            Language::ZhCn => format!("PID={pid} 失败：{e}"),
                            Language::En => format!("PID={pid} failed: {e}"),
                        },
                    })
                    .await;
            }
        }
    }

    let (timeout_ms, step_ms) = match signal {
        TerminateSignal::Term => (900_u64, 90_u64),
        TerminateSignal::Kill => (350_u64, 70_u64),
    };

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut pending: HashSet<u32> = signal_sent.iter().copied().collect();
    while !pending.is_empty() && Instant::now() < deadline {
        pending.retain(|pid| process_exists(*pid));
        if pending.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(step_ms)).await;
    }

    let terminated_pids: Vec<u32> = signal_sent
        .into_iter()
        .filter(|pid| !pending.contains(pid))
        .collect();
    let terminated = terminated_pids.len();
    let still_running = pending.len();
    removed_pids.extend(terminated_pids);

    let _ = tx
        .send(TerminateEvent::Finished {
            terminated,
            still_running,
            failed,
            not_found,
            removed_pids,
        })
        .await;
}

fn selected_pids(
    processes_state: &Rc<RefCell<Vec<ProcessInfo>>>,
    selection_state: &Rc<RefCell<HashSet<u32>>>,
) -> Vec<u32> {
    let selected = selection_state.borrow();
    let mut pids: Vec<u32> = processes_state
        .borrow()
        .iter()
        .filter(|p| selected.contains(&p.pid))
        .map(|p| p.pid)
        .collect();
    pids.sort_unstable();
    pids
}

fn confirm_text(pids: &[u32], signal: TerminateSignal, lang: Language) -> String {
    let count = pids.len();
    let sig = match signal {
        TerminateSignal::Term => "SIGTERM",
        TerminateSignal::Kill => "SIGKILL",
    };
    match lang {
        Language::ZhCn => {
            format!("将对 {count} 个进程发送 {sig}。\n可能导致未保存数据丢失，确定继续吗？")
        }
        Language::En => {
            format!("Send {sig} to {count} process(es).\nUnsaved data may be lost. Continue?")
        }
    }
}

fn scope_toggle_label(lang: Language, show_all: bool) -> &'static str {
    match (lang, show_all) {
        (Language::ZhCn, false) => "显示全部",
        (Language::ZhCn, true) => "仅可结束",
        (Language::En, false) => "Show all",
        (Language::En, true) => "Terminatable only",
    }
}

fn usage_level_label(
    lang: Language,
    fraction: f64,
    high_threshold: f64,
    mid_threshold: f64,
) -> &'static str {
    if fraction >= high_threshold {
        pick(lang, "压力高", "High")
    } else if fraction >= mid_threshold {
        pick(lang, "压力中", "Medium")
    } else {
        pick(lang, "压力低", "Low")
    }
}

fn set_pm_usage_bar(
    bar: &gtk::ProgressBar,
    fraction: f64,
    high_threshold: f64,
    mid_threshold: f64,
) {
    let fraction = fraction.clamp(0.0, 1.0);
    bar.set_show_text(false);
    bar.set_fraction(fraction);

    bar.remove_css_class("pm-usage-bar-high");
    bar.remove_css_class("pm-usage-bar-mid");
    bar.remove_css_class("pm-usage-bar-low");

    if fraction >= high_threshold {
        bar.add_css_class("pm-usage-bar-high");
    } else if fraction >= mid_threshold {
        bar.add_css_class("pm-usage-bar-mid");
    } else {
        bar.add_css_class("pm-usage-bar-low");
    }
}

fn process_exists(pid: u32) -> bool {
    let path = std::path::Path::new("/proc").join(pid.to_string());
    match std::fs::metadata(&path) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

fn is_boost_protected_process(p: &ProcessInfo) -> bool {
    const KEYWORDS: [&str; 27] = [
        "gnome-shell",
        "gnome-session",
        "plasmashell",
        "kwin_x11",
        "kwin_wayland",
        "ksmserver",
        "kded5",
        "kded6",
        "xfce4-session",
        "xfce4-panel",
        "cinnamon",
        "mate-session",
        "mate-panel",
        "lxqt-panel",
        "openbox",
        "sway",
        "i3",
        "wayfire",
        "weston",
        "xorg",
        "xwayland",
        "dbus-daemon",
        "systemd",
        "pipewire",
        "wireplumber",
        "pulseaudio",
        "xdg-desktop-portal",
    ];

    let name = p.name.to_ascii_lowercase();
    if KEYWORDS.iter().any(|kw| name.contains(kw)) {
        return true;
    }
    let cmd = p.cmdline.as_deref().unwrap_or("").to_ascii_lowercase();
    KEYWORDS.iter().any(|kw| cmd.contains(kw))
}

fn create_process_list_box() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list
}

fn render_process_list(
    scrolled: &gtk::ScrolledWindow,
    list_holder: &Rc<RefCell<gtk::ListBox>>,
    processes_state: &Rc<RefCell<Vec<ProcessInfo>>>,
    selection_state: &Rc<RefCell<HashSet<u32>>>,
    query: &str,
    current_uid: u32,
    self_pid: u32,
    show_all: bool,
    mem_total: Option<u64>,
    lang: Language,
    expected_seq: u64,
    render_seq: Rc<Cell<u64>>,
    update_summary: Rc<dyn Fn()>,
) {
    let next_list = create_process_list_box();

    let mut filtered: Vec<ProcessInfo> = processes_state
        .borrow()
        .iter()
        .filter(|p| matches_query(p, query))
        .filter(|p| show_all || process_manager::can_terminate(current_uid, self_pid, p))
        .cloned()
        .collect();

    filtered.sort_by(|a, b| match (a.rss_bytes, b.rss_bytes) {
        (Some(a_s), Some(b_s)) => b_s.cmp(&a_s),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.pid.cmp(&b.pid),
    });

    let max_rss = filtered
        .iter()
        .filter_map(|p| p.rss_bytes)
        .max()
        .unwrap_or(0);
    let queue = Rc::new(RefCell::new(VecDeque::from(filtered)));

    // 不立即清空旧列表，避免刷新时出现短暂白屏。等新列表渲染完成后再原子替换。
    let scrolled_for_tick = scrolled.clone();
    let list_holder_for_tick = list_holder.clone();
    let list_for_tick = next_list.clone();
    let queue_for_tick = queue.clone();
    let selection_for_tick = selection_state.clone();
    let render_seq_for_tick = render_seq.clone();
    glib::idle_add_local(move || {
        const CHUNK_SIZE: usize = 120;

        if render_seq_for_tick.get() != expected_seq {
            return glib::ControlFlow::Break;
        }

        let mut q = queue_for_tick.borrow_mut();
        for _ in 0..CHUNK_SIZE {
            let Some(p) = q.pop_front() else {
                drop(q);
                scrolled_for_tick.set_child(Some(&list_for_tick));
                *list_holder_for_tick.borrow_mut() = list_for_tick.clone();
                update_summary();
                return glib::ControlFlow::Break;
            };

            let allowed = process_manager::can_terminate(current_uid, self_pid, &p);

            let title_text = glib::markup_escape_text(&format!("{} (PID {})", p.name, p.pid));
            let subtitle_text =
                glib::markup_escape_text(&process_subtitle(&p, allowed, lang));
            let row = adw::ActionRow::builder()
                .title(title_text)
                .subtitle(subtitle_text)
                .build();
            row.set_tooltip_text(p.cmdline.as_deref());

            let check = gtk::CheckButton::new();
            check.set_valign(gtk::Align::Center);
            check.set_sensitive(allowed);
            check.set_active(selection_for_tick.borrow().contains(&p.pid));
            row.add_prefix(&check);

            let icon_name = p
                .icon_name
                .as_deref()
                .unwrap_or("application-x-executable-symbolic");
            let icon = gtk::Image::from_icon_name(icon_name);
            icon.set_pixel_size(20);
            icon.set_valign(gtk::Align::Center);
            row.add_prefix(&icon);

            let suffix_box = gtk::Box::new(gtk::Orientation::Vertical, 3);
            suffix_box.set_halign(gtk::Align::End);

            let top_line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            top_line.set_halign(gtk::Align::End);

            let share_text = match (mem_total, p.rss_bytes) {
                (Some(total), Some(rss)) if total > 0 => {
                    let pct = rss as f64 * 100.0 / total as f64;
                    if pct < 0.1 {
                        "<0.1%".to_string()
                    } else {
                        format!("{pct:.1}%")
                    }
                }
                _ => "-".to_string(),
            };
            let share_label = gtk::Label::new(Some(&share_text));
            share_label.add_css_class("caption");
            share_label.add_css_class("dim-label");

            let rss_label =
                gtk::Label::new(Some(&p.rss_bytes.map_or_else(|| "-".into(), format_size)));
            rss_label.add_css_class("pm-row-rss");
            rss_label.add_css_class("monospace");
            rss_label.set_xalign(1.0);

            top_line.append(&share_label);
            top_line.append(&rss_label);

            let usage_bar = gtk::ProgressBar::new();
            usage_bar.add_css_class("pm-usage-bar");
            let fraction = match (p.rss_bytes, max_rss) {
                (Some(rss), max) if max > 0 => (rss as f64 / max as f64).clamp(0.0, 1.0),
                _ => 0.0,
            };
            set_pm_usage_bar(&usage_bar, fraction, 0.66, 0.33);

            suffix_box.append(&top_line);
            suffix_box.append(&usage_bar);
            row.add_suffix(&suffix_box);

            if !allowed {
                row.add_css_class("dim-label");
            }

            {
                let selection_for_toggle = selection_for_tick.clone();
                let pid = p.pid;
                let update_summary = update_summary.clone();
                check.connect_toggled(move |btn| {
                    if btn.is_active() {
                        selection_for_toggle.borrow_mut().insert(pid);
                    } else {
                        selection_for_toggle.borrow_mut().remove(&pid);
                    }
                    update_summary();
                });
            }

            if allowed {
                row.set_activatable(true);
                let check = check.clone();
                row.connect_activated(move |_| {
                    check.set_active(!check.is_active());
                });
            }

            list_for_tick.append(&row);
        }

        glib::ControlFlow::Continue
    });
}

fn matches_query(p: &ProcessInfo, query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    let q = q.to_lowercase();
    if p.name.to_lowercase().contains(&q) {
        return true;
    }
    p.cmdline
        .as_deref()
        .unwrap_or("")
        .to_lowercase()
        .contains(&q)
}

fn process_subtitle(p: &ProcessInfo, allowed: bool, lang: Language) -> String {
    let hint = if allowed {
        "".to_string()
    } else if p.pid == process_manager::self_pid() {
        pick(lang, "不可结束自身进程", "Cannot terminate self").to_string()
    } else {
        pick(lang, "需要管理员权限", "Requires admin privilege").to_string()
    };

    let cmd = p
        .cmdline
        .as_deref()
        .map(|v| compact_text(v, 96))
        .unwrap_or_else(|| "-".into());

    match (lang, hint.is_empty()) {
        (Language::ZhCn, true) => cmd,
        (Language::ZhCn, false) => format!("{hint} · {cmd}"),
        (Language::En, true) => cmd,
        (Language::En, false) => format!("{hint} · {cmd}"),
    }
}

fn format_memory_overview(m: &process_manager::MemorySnapshot, lang: Language) -> String {
    let used = m.mem_used().map(format_size).unwrap_or_else(|| "-".into());
    let total = m.mem_total.map(format_size).unwrap_or_else(|| "-".into());
    let avail = m
        .mem_available
        .map(format_size)
        .unwrap_or_else(|| "-".into());

    match lang {
        Language::ZhCn => format!("{used} / {total} · 可用 {avail}"),
        Language::En => format!("{used} / {total} · avail {avail}"),
    }
}

fn format_swap_overview(m: &process_manager::MemorySnapshot) -> String {
    let used = m.swap_used().map(format_size).unwrap_or_else(|| "-".into());
    let total = m.swap_total.map(format_size).unwrap_or_else(|| "-".into());
    format!("{used} / {total}")
}

fn compact_text(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars.saturating_sub(1) {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_text_adds_ellipsis() {
        let s = "abcdefghijklmnopqrstuvwxyz";
        let out = compact_text(s, 10);
        assert!(out.ends_with('…'));
        assert!(out.len() <= 12);
    }

    #[test]
    fn matches_query_checks_name_and_cmdline_case_insensitive() {
        let p = ProcessInfo {
            pid: 1,
            name: "Chrome".into(),
            uid: 1000,
            rss_bytes: Some(123),
            cmdline: Some("/usr/bin/google-chrome --type=renderer".into()),
            icon_name: None,
        };
        assert!(matches_query(&p, "chrome"));
        assert!(matches_query(&p, "RENDERER"));
        assert!(!matches_query(&p, "firefox"));
    }
}
