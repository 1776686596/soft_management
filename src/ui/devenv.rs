use adw::prelude::*;
use gtk::glib;
use std::collections::BTreeMap;

use crate::i18n::{pick, Language};
use crate::runtime;
use crate::services::environment;

pub fn build(token: tokio_util::sync::CancellationToken, lang: Language) -> adw::NavigationPage {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let spinner = gtk::Spinner::new();
    spinner.set_spinning(true);
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.set_vexpand(true);

    let status_page = adw::StatusPage::builder()
        .title(pick(
            lang,
            "正在扫描开发环境...",
            "Scanning development environment...",
        ))
        .child(&spinner)
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&status_page, Some("loading"));

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let content_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content_box.set_margin_top(12);
    content_box.set_margin_bottom(12);
    content_box.set_margin_start(12);
    content_box.set_margin_end(12);
    scrolled.set_child(Some(&content_box));

    stack.add_named(&scrolled, Some("content"));

    let empty_page = adw::StatusPage::builder()
        .title(pick(lang, "未发现开发工具", "No development tools found"))
        .icon_name("utilities-terminal-symbolic")
        .build();
    stack.add_named(&empty_page, Some("empty"));

    vbox.append(&stack);

    let (tx, rx) = async_channel::bounded::<environment::EnvEvent>(32);

    let token_clone = token.clone();
    runtime::spawn(async move {
        environment::scan_all(tx, token_clone).await;
    });

    let content_clone = content_box.clone();
    let stack_clone = stack.clone();
    glib::spawn_future_local(async move {
        let mut found_any = false;

        while let Ok(event) = rx.recv().await {
            let has_data = !event.runtimes.is_empty()
                || !event.version_managers.is_empty()
                || !event.global_packages.is_empty();

            if !has_data {
                continue;
            }
            found_any = true;

            let group = adw::PreferencesGroup::new();
            group.set_title(&capitalize(&event.language));

            for rt in &event.runtimes {
                let title_text = glib::markup_escape_text(&format!(
                    "{} {}",
                    capitalize(&rt.language),
                    &rt.version
                ));
                let subtitle_text = glib::markup_escape_text(&format!(
                    "{} ({})",
                    &rt.path,
                    &rt.install_method
                ));
                let row = adw::ActionRow::builder()
                    .title(title_text)
                    .subtitle(subtitle_text)
                    .build();
                group.add(&row);
            }

            for vm in &event.version_managers {
                let versions_str = if vm.managed_versions.is_empty() {
                    pick(lang, "已安装", "installed").to_string()
                } else {
                    vm.managed_versions
                        .iter()
                        .map(|v| {
                            if v.active {
                                format!("{} {}", v.version, pick(lang, "(当前)", "(active)"))
                            } else {
                                v.version.clone()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&vm.name))
                    .subtitle(glib::markup_escape_text(&versions_str))
                    .build();
                group.add(&row);
            }

            if !event.global_packages.is_empty() {
                let mut by_manager: BTreeMap<&str, Vec<&crate::models::GlobalPackageInfo>> =
                    BTreeMap::new();
                for pkg in &event.global_packages {
                    by_manager.entry(pkg.manager.as_str()).or_default().push(pkg);
                }

                for (manager, mut pkgs) in by_manager {
                    pkgs.sort_by(|a, b| a.name.cmp(&b.name));

                    let label = global_packages_label(lang, manager, pkgs.len());
                    let expander = adw::ExpanderRow::builder().title(&label).build();
                    for pkg in pkgs {
                        let subtitle = if pkg.version.is_empty() {
                            manager.to_string()
                        } else {
                            format!("{} {}", manager, pkg.version)
                        };
                        let title_text = glib::markup_escape_text(&pkg.name);
                        let subtitle_text = glib::markup_escape_text(&subtitle);
                        let child = adw::ActionRow::builder()
                            .title(title_text)
                            .subtitle(subtitle_text)
                            .build();
                        expander.add_row(&child);
                    }
                    group.add(&expander);
                }
            }

            content_clone.append(&group);
            stack_clone.set_visible_child_name("content");
        }

        if !found_any {
            stack_clone.set_visible_child_name("empty");
        }
    });

    adw::NavigationPage::builder()
        .title(pick(lang, "开发环境", "Dev Environment"))
        .child(&vbox)
        .build()
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

fn global_packages_label(lang: Language, manager: &str, count: usize) -> String {
    match lang {
        Language::ZhCn => match manager {
            "pip" => format!("pip 全局包（{count}）"),
            "pipx" => format!("pipx 应用（{count}）"),
            "uv" => format!("uv 工具（{count}）"),
            "npm" => format!("npm 全局包（{count}）"),
            "cargo" => format!("cargo 工具（{count}）"),
            _ => format!("{manager}（{count}）"),
        },
        Language::En => match manager {
            "pip" => format!("pip packages ({count})"),
            "pipx" => format!("pipx apps ({count})"),
            "uv" => format!("uv tools ({count})"),
            "npm" => format!("npm packages ({count})"),
            "cargo" => format!("cargo tools ({count})"),
            _ => format!("{manager} ({count})"),
        },
    }
}
