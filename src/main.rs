#![cfg_attr(windows, windows_subsystem = "windows")]
//! VramDisk: один диск из оперативной памяти и видеопамяти.
//!
//! Один файл программы — две роли: окно (`vramdisk.exe`) и фоновая часть без
//! окна, которая держит диск: `--serve <запрос>` (её запускает окно) и
//! `--autostart` (при входе в Windows).

mod gui;

use vramdisk::logger::{self, log};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |a: &str| args.iter().any(|x| x == a);
    let serve = args.iter().position(|a| a == "--serve").map(|i| args.get(i + 1).cloned().unwrap_or_default());
    let autostart = has("--autostart");
    let demo = cfg!(not(windows)) || has("--demo");
    // Фоновая часть — только с настоящим диском (в демо-режиме всё в окне).
    let background = cfg!(windows) && !demo && (serve.is_some() || autostart);
    if background {
        logger::set_tag("фон");
    }
    let log_ok = logger::init(&vramdisk::settings::app_dir());
    let role = match (background, serve.is_some(), autostart) {
        (true, true, _) => " (фоновая часть)",
        (true, false, _) => " (автозапуск, без окна)",
        (false, _, true) => " (автозапуск)",
        _ => "",
    };
    log(format!("VramDisk {} запущен{role}{}", vramdisk::VERSION, if demo { " (демо)" } else { "" }));

    // Слои Vulkan от оверлеев (Steam, OBS, RTSS…) диску не нужны и иногда
    // ломают создание устройства — отключаем неявные слои для своего процесса.
    std::env::set_var("VK_LOADER_LAYERS_DISABLE", "~implicit~");

    #[cfg(windows)]
    let _instance = {
        use vramdisk::win::{shutdown, sys};
        if !demo && !sys::is_elevated() {
            // Фоновую часть запускает окно, у которого права уже есть.
            if serve.is_some() {
                log("фоновая часть запущена без прав администратора — выхожу");
                std::process::exit(5);
            }
            // Один перезапуск с запросом UAC; метка не даёт зациклиться,
            // если права так и не выдали.
            if !has("--elevated") {
                let mut a = args.clone();
                a.push("--elevated".into());
                if sys::relaunch_elevated(&a.join(" ")) {
                    return;
                }
            }
            sys::error_box(
                "VramDisk",
                "Нужны права администратора: ImDisk создаёт диски только от имени администратора.\nЗапустите VramDisk ещё раз и подтвердите запрос Windows.",
            );
            return;
        }
        // Окно — одно; фоновая часть следит за собой сама.
        let guard = if background {
            None
        } else {
            match sys::single_instance() {
                Some(g) => Some(g),
                None => {
                    log("уже запущен — показываю окно");
                    sys::activate_window(vramdisk::APP_TITLE);
                    return;
                }
            }
        };
        shutdown::start_watcher();
        guard
    };
    if let Err(e) = &log_ok {
        log(e);
    }
    // Версия 1.0 хранила настройки и журнал в профиле пользователя.
    if !background {
        for m in vramdisk::settings::migrate_old() {
            log(m);
        }
    }

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(format!("СБОЙ: {info}"));
        // Сбой в потоке сервера ImDisk — снимать диск нечем; сбой в потоках
        // связи окна с фоновой частью диску не мешает.
        #[cfg(windows)]
        if !matches!(std::thread::current().name(), Some("imdisk-proxy" | "pipe-server" | "remote-poll")) {
            // Пока сервер жив, диск можно снять корректно.
            vramdisk::win::shutdown::run_cleanup("сбой программы");
        }
        default_hook(info);
    }));

    #[cfg(windows)]
    if background {
        let code = match &serve {
            Some(json) => gui::serve(json),
            None => gui::autostart(),
        };
        vramdisk::win::shutdown::run_cleanup("выход фоновой части");
        log(format!("фоновая часть завершена (код {code})"));
        std::process::exit(code);
    }

    if let Err(e) = gui::run(autostart, demo) {
        log(format!("окно: {e}"));
        #[cfg(windows)]
        vramdisk::win::sys::error_box("VramDisk", &format!("Не удалось открыть окно: {e}"));
    }
    #[cfg(windows)]
    vramdisk::win::shutdown::run_cleanup("выход из программы");
    log("VramDisk завершён");
}
