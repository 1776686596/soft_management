use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::i18n::{pick, Language};
use crate::models::{CleanupSuggestion, RiskLevel};
use crate::runtime;
use crate::services::cleanup;
use crate::subprocess::run_command;

static SCAN_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn build(token: tokio_util::sync::CancellationToken, lang: Language) -> adw::NavigationPage {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let banner = adw::Banner::new("");
    banner.set_revealed(false);
    vbox.append(&banner);

    let confirm_revealer = gtk::Revealer::new();
    confirm_revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    confirm_revealer.set_reveal_child(false);

    let confirm_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    confirm_box.set_margin_top(6);
    confirm_box.set_margin_bottom(6);
    confirm_box.set_margin_start(12);
    confirm_box.set_margin_end(12);

    let confirm_label = gtk::Label::new(None);
    confirm_label.set_halign(gtk::Align::Start);
    confirm_label.set_hexpand(true);

    let confirm_cancel_btn = gtk::Button::with_label(pick(lang, "取消", "Cancel"));
    confirm_cancel_btn.add_css_class("flat");
    let confirm_ok_btn = gtk::Button::with_label(pick(lang, "确认执行", "Run"));
    confirm_ok_btn.add_css_class("suggested-action");

    confirm_box.append(&confirm_label);
    confirm_box.append(&confirm_cancel_btn);
    confirm_box.append(&confirm_ok_btn);
    confirm_revealer.set_child(Some(&confirm_box));
    vbox.append(&confirm_revealer);

    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    toolbar.set_margin_top(8);
    toolbar.set_margin_bottom(8);
    toolbar.set_margin_start(12);
    toolbar.set_margin_end(12);

    let rescan_btn = gtk::Button::with_label(pick(lang, "重新扫描", "Rescan"));
    rescan_btn.add_css_class("suggested-action");

    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);

    let progress_badge = gtk::Label::new(Some(pick(lang, "准备中", "Preparing")));
    progress_badge.add_css_class("disk-badge");
    progress_badge.add_css_class("dim-label");

    toolbar.append(&rescan_btn);
    toolbar.append(&spacer);
    toolbar.append(&progress_badge);
    vbox.append(&toolbar);

    let spinner = gtk::Spinner::new();
    spinner.set_spinning(true);
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.set_vexpand(true);

    let loading_page = adw::StatusPage::builder()
        .title(pick(lang, "正在扫描可清理项...", "Scanning cleanups..."))
        .child(&spinner)
        .build();

    let empty_page = adw::StatusPage::builder()
        .title(pick(lang, "未发现可清理项", "No cleanups found"))
        .description(pick(
            lang,
            "可能是相关工具未安装，或缓存占用为 0",
            "Tools missing or caches are already empty",
        ))
        .icon_name("edit-clear-all-symbolic")
        .build();

    let content_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content_box.set_margin_top(12);
    content_box.set_margin_bottom(12);
    content_box.set_margin_start(12);
    content_box.set_margin_end(12);

    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let summary_label = gtk::Label::new(Some(pick(lang, "等待数据", "Waiting data")));
    summary_label.set_halign(gtk::Align::Start);
    summary_label.set_hexpand(true);
    summary_label.add_css_class("caption");

    let copy_btn = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .label(pick(lang, "复制命令", "Copy commands"))
        .build();
    copy_btn.add_css_class("flat");
    copy_btn.set_sensitive(false);

    let run_btn = gtk::Button::with_label(pick(lang, "执行清理", "Run cleanup"));
    run_btn.add_css_class("suggested-action");
    run_btn.set_sensitive(false);

    footer.append(&summary_label);
    footer.append(&copy_btn);
    footer.append(&run_btn);

    content_box.append(&scrolled);
    content_box.append(&footer);

    let stack = gtk::Stack::new();
    stack.add_named(&loading_page, Some("loading"));
    stack.add_named(&empty_page, Some("empty"));
    stack.add_named(&content_box, Some("content"));
    vbox.append(&stack);

    let (tx, rx) = async_channel::bounded::<cleanup::CleanupEvent>(32);
    let tx_for_rescan = tx.clone();

    let active_scan_id = Rc::new(RefCell::new(0_u64));
    let sources_seen = Rc::new(RefCell::new(HashSet::<String>::new()));
    let suggestions_state = Rc::new(RefCell::new(Vec::<CleanupSuggestion>::new()));
    let selection_state = Rc::new(RefCell::new(HashSet::<String>::new()));
    let pending_run = Rc::new(RefCell::new(Vec::<CleanupSuggestion>::new()));

    let rebuild_list: Rc<dyn Fn()> = Rc::new({
        let list = list.clone();
        let suggestions_state = suggestions_state.clone();
        let selection_state = selection_state.clone();
        let summary_label = summary_label.clone();
        let copy_btn = copy_btn.clone();
        let run_btn = run_btn.clone();

        move || {
            clear_list_box(&list);
            selection_state.borrow_mut().clear();

            let mut suggestions = suggestions_state.borrow().clone();
            suggestions.sort_by(|a, b| b.estimated_bytes.cmp(&a.estimated_bytes));

            for s in &suggestions {
                let default_selected = matches!(s.risk_level, RiskLevel::Safe);

                let title_text = glib::markup_escape_text(&s.description);
                let subtitle_text = glib::markup_escape_text(&suggestion_subtitle(s, lang));
                let row = adw::ActionRow::builder()
                    .title(title_text)
                    .subtitle(subtitle_text)
                    .build();
                row.set_tooltip_text(Some(&s.command));

                let check = gtk::CheckButton::new();
                check.set_valign(gtk::Align::Center);
                check.set_active(default_selected);
                row.add_prefix(&check);

                let size_label = gtk::Label::new(Some(&format_size(s.estimated_bytes)));
                size_label.add_css_class("monospace");
                size_label.set_xalign(1.0);
                row.add_suffix(&size_label);

                if default_selected {
                    selection_state.borrow_mut().insert(s.command.clone());
                }

                let command_key = s.command.clone();
                let selection_state = selection_state.clone();
                let suggestions_state = suggestions_state.clone();
                let summary_label = summary_label.clone();
                let copy_btn = copy_btn.clone();
                let run_btn = run_btn.clone();
                check.connect_toggled(move |btn| {
                    if btn.is_active() {
                        selection_state.borrow_mut().insert(command_key.clone());
                    } else {
                        selection_state.borrow_mut().remove(&command_key);
                    }
                    update_summary(
                        &summary_label,
                        &copy_btn,
                        &run_btn,
                        &suggestions_state,
                        &selection_state,
                        lang,
                    );
                });

                list.append(&row);
            }

            update_summary(
                &summary_label,
                &copy_btn,
                &run_btn,
                &suggestions_state,
                &selection_state,
                lang,
            );
        }
    });

    let start_scan: Rc<dyn Fn()> = Rc::new({
        let token = token.clone();
        let active_scan_id = active_scan_id.clone();
        let sources_seen = sources_seen.clone();
        let suggestions_state = suggestions_state.clone();
        let selection_state = selection_state.clone();
        let pending_run = pending_run.clone();
        let stack = stack.clone();
        let banner = banner.clone();
        let confirm_revealer = confirm_revealer.clone();
        let progress_badge = progress_badge.clone();

        move || {
            banner.set_revealed(false);
            confirm_revealer.set_reveal_child(false);
            pending_run.borrow_mut().clear();
            selection_state.borrow_mut().clear();
            suggestions_state.borrow_mut().clear();
            sources_seen.borrow_mut().clear();

            let scan_id = SCAN_SEQ.fetch_add(1, Ordering::Relaxed);
            *active_scan_id.borrow_mut() = scan_id;

            progress_badge.set_label(pick(lang, "扫描中...", "Scanning..."));
            stack.set_visible_child_name("loading");

            let tx_clone = tx_for_rescan.clone();
            let token_clone = token.clone();
            runtime::spawn(async move {
                cleanup::scan_all(tx_clone, token_clone, scan_id).await;
            });
        }
    });

    {
        let start_scan = start_scan.clone();
        rescan_btn.connect_clicked(move |_| start_scan());
    }

    start_scan();

    {
        let confirm_revealer = confirm_revealer.clone();
        confirm_cancel_btn.connect_clicked(move |_| confirm_revealer.set_reveal_child(false));
    }

    confirm_ok_btn.connect_clicked({
        let confirm_revealer = confirm_revealer.clone();
        let pending_run = pending_run.clone();
        let banner = banner.clone();
        let progress_badge = progress_badge.clone();
        let start_scan = start_scan.clone();
        move |_| {
            confirm_revealer.set_reveal_child(false);
            let targets = pending_run.borrow().clone();
            if targets.is_empty() {
                return;
            }

            progress_badge.set_label(pick(lang, "清理中...", "Cleaning..."));

            let (run_tx, run_rx) = async_channel::bounded::<RunEvent>(32);
            runtime::spawn(async move {
                run_selected_cleanups(run_tx, targets, lang).await;
            });

            let banner = banner.clone();
            let progress_badge = progress_badge.clone();
            let start_scan_for_task = start_scan.clone();
            glib::spawn_future_local(async move {
                while let Ok(event) = run_rx.recv().await {
                    match event {
                        RunEvent::Progress { current, total, cmd } => {
                            progress_badge.set_label(&match lang {
                                Language::ZhCn => format!("清理中（{current}/{total}）"),
                                Language::En => format!("Cleaning ({current}/{total})"),
                            });
                            banner.set_title(&match lang {
                                Language::ZhCn => format!("正在执行：{cmd}"),
                                Language::En => format!("Running: {cmd}"),
                            });
                            banner.set_revealed(true);
                        }
                        RunEvent::Finished {
                            ok,
                            failed,
                            skipped_sudo,
                        } => {
                            let title = match lang {
                                Language::ZhCn => format!(
                                    "清理完成：成功 {ok} · 失败 {failed} · 需 sudo 跳过 {skipped_sudo}"
                                ),
                                Language::En => format!(
                                    "Cleanup done: ok {ok}, failed {failed}, skipped sudo {skipped_sudo}"
                                ),
                            };
                            banner.set_title(&title);
                            banner.set_revealed(true);
                            progress_badge.set_label(pick(lang, "扫描中...", "Scanning..."));
                            start_scan_for_task();
                        }
                        RunEvent::Message { text } => {
                            banner.set_title(&text);
                            banner.set_revealed(true);
                        }
                    }
                }
            });
        }
    });

    copy_btn.connect_clicked({
        let banner = banner.clone();
        let suggestions_state = suggestions_state.clone();
        let selection_state = selection_state.clone();
        move |_| {
            let text = build_copy_text(&suggestions_state, &selection_state);
            if text.is_empty() {
                banner.set_title(pick(lang, "未选择任何清理项", "Nothing selected"));
                banner.set_revealed(true);
                return;
            }
            if copy_text_to_clipboard(&text) {
                banner.set_title(pick(lang, "已复制到剪贴板", "Copied to clipboard"));
                banner.set_revealed(true);
            } else {
                banner.set_title(pick(lang, "复制失败：无可用显示", "Copy failed: no display"));
                banner.set_revealed(true);
            }
        }
    });

    run_btn.connect_clicked({
        let banner = banner.clone();
        let confirm_revealer = confirm_revealer.clone();
        let confirm_label = confirm_label.clone();
        let pending_run = pending_run.clone();
        let suggestions_state = suggestions_state.clone();
        let selection_state = selection_state.clone();
        move |_| {
            let selected = selected_suggestions(&suggestions_state, &selection_state);
            if selected.is_empty() {
                banner.set_title(pick(lang, "未选择任何清理项", "Nothing selected"));
                banner.set_revealed(true);
                return;
            }

            let (to_run, skipped_sudo, moderate) = split_run_targets(&selected);
            pending_run.borrow_mut().clear();
            pending_run.borrow_mut().extend(to_run);

            if pending_run.borrow().is_empty() {
                banner.set_title(pick(
                    lang,
                    "所选项目需要 sudo，建议复制命令到终端执行",
                    "Selected items require sudo; copy commands and run in terminal",
                ));
                banner.set_revealed(true);
                return;
            }

            let total_bytes: u64 = pending_run
                .borrow()
                .iter()
                .map(|s| s.estimated_bytes)
                .sum();
            let count = pending_run.borrow().len();

            let mut info = match lang {
                Language::ZhCn => format!(
                    "将执行 {count} 项清理（预计释放 {}）",
                    format_size(total_bytes)
                ),
                Language::En => format!(
                    "Run {count} cleanup(s) (est. free {})",
                    format_size(total_bytes)
                ),
            };
            if moderate > 0 {
                info.push_str(&match lang {
                    Language::ZhCn => format!(" · 中等风险 {moderate}"),
                    Language::En => format!(" · moderate {moderate}"),
                });
            }
            if skipped_sudo > 0 {
                info.push_str(&match lang {
                    Language::ZhCn => format!(" · 需 sudo 将跳过 {skipped_sudo}"),
                    Language::En => format!(" · skipped sudo {skipped_sudo}"),
                });
            }

            confirm_label.set_label(&info);
            confirm_revealer.set_reveal_child(true);
        }
    });

    glib::spawn_future_local({
        let active_scan_id = active_scan_id.clone();
        let sources_seen = sources_seen.clone();
        let suggestions_state = suggestions_state.clone();
        let stack = stack.clone();
        let progress_badge = progress_badge.clone();
        let rebuild_list = rebuild_list.clone();

        async move {
            const ADAPTER_TOTAL: usize = 6;

            while let Ok(event) = rx.recv().await {
                if event.scan_id != *active_scan_id.borrow() {
                    continue;
                }

                sources_seen.borrow_mut().insert(event.source);
                suggestions_state.borrow_mut().extend(event.suggestions);

                let done = sources_seen.borrow().len() >= ADAPTER_TOTAL;
                progress_badge.set_label(&match lang {
                    Language::ZhCn => format!("扫描中（{}/{}）", sources_seen.borrow().len(), ADAPTER_TOTAL),
                    Language::En => format!("Scanning ({}/{})", sources_seen.borrow().len(), ADAPTER_TOTAL),
                });

                if done {
                    progress_badge.set_label(pick(lang, "扫描完成", "Scan finished"));
                    if suggestions_state.borrow().is_empty() {
                        stack.set_visible_child_name("empty");
                    } else {
                        rebuild_list();
                        stack.set_visible_child_name("content");
                    }
                }
            }
        }
    });

    adw::NavigationPage::builder()
        .title(pick(lang, "清理助手", "Cleanup Assistant"))
        .child(&vbox)
        .build()
}

enum RunEvent {
    Message { text: String },
    Progress { current: usize, total: usize, cmd: String },
    Finished {
        ok: usize,
        failed: usize,
        skipped_sudo: usize,
    },
}

async fn run_selected_cleanups(
    tx: async_channel::Sender<RunEvent>,
    selected: Vec<CleanupSuggestion>,
    lang: Language,
) {
    let (to_run, skipped_sudo, _) = split_run_targets(&selected);
    let total = to_run.len();

    if total == 0 {
        let _ = tx
            .send(RunEvent::Finished {
                ok: 0,
                failed: 0,
                skipped_sudo,
            })
            .await;
        return;
    }

    let mut ok = 0usize;
    let mut failed = 0usize;

    for (idx, s) in to_run.iter().enumerate() {
        let current = idx + 1;
        let cmd = s.command.clone();
        let _ = tx
            .send(RunEvent::Progress {
                current,
                total,
                cmd: cmd.clone(),
            })
            .await;

        let parts: Vec<&str> = cmd.split_whitespace().collect();
        if parts.is_empty() {
            failed += 1;
            continue;
        }
        let cmd_bin = parts[0];
        let args = &parts[1..];

        match run_command(cmd_bin, args, 600).await {
            Ok(_) => ok += 1,
            Err(e) => {
                failed += 1;
                let _ = tx
                    .send(RunEvent::Message {
                        text: format!("{}: {}", pick(lang, "失败", "Failed"), e),
                    })
                    .await;
            }
        }
    }

    let _ = tx
        .send(RunEvent::Finished {
            ok,
            failed,
            skipped_sudo,
        })
        .await;
}

fn selected_suggestions(
    suggestions_state: &Rc<RefCell<Vec<CleanupSuggestion>>>,
    selection_state: &Rc<RefCell<HashSet<String>>>,
) -> Vec<CleanupSuggestion> {
    let selected = selection_state.borrow();
    suggestions_state
        .borrow()
        .iter()
        .filter(|s| selected.contains(&s.command))
        .cloned()
        .collect()
}

fn split_run_targets(selected: &[CleanupSuggestion]) -> (Vec<CleanupSuggestion>, usize, usize) {
    let mut to_run = Vec::new();
    let mut skipped_sudo = 0usize;
    let mut moderate = 0usize;
    for s in selected {
        if matches!(s.risk_level, RiskLevel::Moderate) {
            moderate += 1;
        }
        if s.requires_sudo {
            skipped_sudo += 1;
            continue;
        }
        to_run.push(s.clone());
    }
    (to_run, skipped_sudo, moderate)
}

fn update_summary(
    summary_label: &gtk::Label,
    copy_btn: &gtk::Button,
    run_btn: &gtk::Button,
    suggestions_state: &Rc<RefCell<Vec<CleanupSuggestion>>>,
    selection_state: &Rc<RefCell<HashSet<String>>>,
    lang: Language,
) {
    let selected = selection_state.borrow();
    let mut count = 0usize;
    let mut bytes: u64 = 0;
    let mut sudo = 0usize;
    for s in suggestions_state.borrow().iter() {
        if !selected.contains(&s.command) {
            continue;
        }
        count += 1;
        bytes = bytes.saturating_add(s.estimated_bytes);
        if s.requires_sudo {
            sudo += 1;
        }
    }

    let label = match lang {
        Language::ZhCn => format!("已选择 {count} 项（需 sudo {sudo}）· 预计释放 {}", format_size(bytes)),
        Language::En => format!("Selected {count} (sudo {sudo}) · est. free {}", format_size(bytes)),
    };
    summary_label.set_label(&label);
    let has_any = count > 0;
    copy_btn.set_sensitive(has_any);
    run_btn.set_sensitive(has_any && count > sudo);
}

fn suggestion_subtitle(s: &CleanupSuggestion, lang: Language) -> String {
    let risk = match (lang, s.risk_level) {
        (Language::ZhCn, RiskLevel::Safe) => "风险：安全",
        (Language::ZhCn, RiskLevel::Moderate) => "风险：中等",
        (Language::En, RiskLevel::Safe) => "Risk: safe",
        (Language::En, RiskLevel::Moderate) => "Risk: moderate",
    };
    let sudo = if s.requires_sudo {
        pick(lang, "需要 sudo", "Requires sudo")
    } else {
        pick(lang, "无需 sudo", "No sudo")
    };
    match lang {
        Language::ZhCn => format!("预计释放 {} · {risk} · {sudo}", format_size(s.estimated_bytes)),
        Language::En => format!("Est. free {} · {risk} · {sudo}", format_size(s.estimated_bytes)),
    }
}

fn build_copy_text(
    suggestions_state: &Rc<RefCell<Vec<CleanupSuggestion>>>,
    selection_state: &Rc<RefCell<HashSet<String>>>,
) -> String {
    let selected = selection_state.borrow();
    let mut lines: Vec<String> = suggestions_state
        .borrow()
        .iter()
        .filter(|s| selected.contains(&s.command))
        .map(|s| {
            if s.requires_sudo {
                format!("sudo {}", s.command)
            } else {
                s.command.clone()
            }
        })
        .collect();
    lines.sort();
    lines.dedup();
    lines.join("\n")
}

fn copy_text_to_clipboard(text: &str) -> bool {
    let Some(display) = gtk::gdk::Display::default() else {
        return false;
    };

    display.clipboard().set_text(text);
    true
}

fn clear_list_box(list: &gtk::ListBox) {
    let mut child = list.first_child();
    while let Some(row) = child {
        child = row.next_sibling();
        list.remove(&row);
    }
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
    fn split_run_targets_skips_sudo() {
        let items = vec![
            CleanupSuggestion {
                description: "a".into(),
                estimated_bytes: 1,
                command: "apt clean".into(),
                requires_sudo: true,
                risk_level: RiskLevel::Safe,
            },
            CleanupSuggestion {
                description: "b".into(),
                estimated_bytes: 2,
                command: "pip3 cache purge".into(),
                requires_sudo: false,
                risk_level: RiskLevel::Safe,
            },
        ];
        let (to_run, skipped, moderate) = split_run_targets(&items);
        assert_eq!(to_run.len(), 1);
        assert_eq!(to_run[0].command, "pip3 cache purge");
        assert_eq!(skipped, 1);
        assert_eq!(moderate, 0);
    }

    #[test]
    fn build_copy_text_adds_sudo_prefix() {
        let suggestions_state = Rc::new(RefCell::new(vec![CleanupSuggestion {
            description: "a".into(),
            estimated_bytes: 1,
            command: "apt clean".into(),
            requires_sudo: true,
            risk_level: RiskLevel::Safe,
        }]));
        let selection_state = Rc::new(RefCell::new(HashSet::from(["apt clean".to_string()])));
        assert_eq!(build_copy_text(&suggestions_state, &selection_state), "sudo apt clean");
    }
}
