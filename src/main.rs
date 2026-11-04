#![cfg_attr(windows, windows_subsystem = "windows")]
//! VramDisk: один диск из оперативной памяти и видеопамяти.

mod gui;

use vramdisk::logger::{self, log};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let autostart = args.iter().any(|a| a == "--autostart");
    let demo = cfg!(not(windows)) || args.iter().any(|a| a == "--demo");
    let log_ok = logger::init(&vramdisk::settings::app_dir());
    log(format!(
        "VramDisk {} запущен{}{}",
        vramdisk::VERSION,
        if autostart { " (автозапуск)" } else { "" },
        if demo { " (демо)" } else { "" }
    ));

    // Слои Vulkan от оверлеев (Steam, OBS, RTSS…) диску не нужны и иногда
    // ломают создание устройства — отключаем неявные слои для своего процесса.
    std::env::set_var("VK_LOADER_LAYERS_DISABLE", "~implicit~");

    #[cfg(windows)]
    let _instance = {
        use vramdisk::win::{shutdown, sys};
        if !demo && !sys::is_elevated() {
            // Один перезапуск с запросом UAC; метка не даёт зациклиться,
            // если права так и не выдали.
            if !args.iter().any(|a| a == "--elevated") {
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
        let Some(guard) = sys::single_instance() else {
            log("уже запущен — показываю окно");
            sys::activate_window(vramdisk::APP_TITLE);
            return;
        };
        shutdown::start_watcher();
        guard
    };
    if let Err(e) = &log_ok {
        log(e);
    }
    // Версия 1.0 хранила настройки и журнал в профиле пользователя.
    for m in vramdisk::settings::migrate_old() {
        log(m);
    }

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(format!("СБОЙ: {info}"));
        #[cfg(windows)]
        if std::thread::current().name() != Some("imdisk-proxy") {
            // Пока сервер жив, диск можно снять корректно.
            vramdisk::win::shutdown::run_cleanup("сбой программы");
        }
        default_hook(info);
    }));

    if let Err(e) = gui::run(autostart, demo) {
        log(format!("окно: {e}"));
        #[cfg(windows)]
        vramdisk::win::sys::error_box("VramDisk", &format!("Не удалось открыть окно: {e}"));
    }
    #[cfg(windows)]
    vramdisk::win::shutdown::run_cleanup("выход из программы");
    log("VramDisk завершён");
}
