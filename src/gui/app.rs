//! Главное окно.

use super::engine::{test_line, CreatingView, Event, MountedView, Notify, View};
use super::link::Link;
use super::plan::{self, bytes_gb, floor_step, gb_to_bytes, STEP_GB};
use super::platform;
use super::theme::{self, bold, font, Palette};
use super::widgets::{self as w, ButtonKind, Icon};
use eframe::egui::{self, vec2, Align, Color32, Id, Layout, Margin, RichText, ViewportCommand};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vramdisk::gpu::{self, Api, GpuInfo, GpuKind, GpuProbe};
use vramdisk::logger::{self, log};
use vramdisk::settings::{clean_label, FillFirst, FsKind, MountRecord, Settings, ThemeChoice};
use vramdisk::util::{size as fsize, speed, GIB, MIB};

const SAMPLES: usize = 120;

/// Результат фоновой задачи: появляется, когда она завершится.
type JobResult = Arc<Mutex<Option<Result<(), String>>>>;

enum GpuState {
    Loading,
    Ready(Vec<GpuInfo>),
    Missing(String),
}

#[derive(Default)]
struct Startup {
    done: bool,
    imdisk: Option<String>,
    autostart: bool,
    record: RecordCheck,
}

/// Что с записью о диске от прошлого запуска.
#[derive(Default, Clone)]
struct RecordCheck {
    /// Диск остался, а обслуживать его некому.
    stale: Option<MountRecord>,
    /// Запись устарела — удалить.
    clear: bool,
}

/// Запись о диске: её ведёт фоновая часть (в версии 2.0 — окно, в
/// настройках). Если фоновая часть жива, диск в порядке.
fn check_record(legacy: Option<MountRecord>) -> RecordCheck {
    let Some(r) = MountRecord::load().or(legacy) else { return RecordCheck::default() };
    if platform::server_alive(r.pid) {
        return RecordCheck::default();
    }
    if platform::stale_exists(r.letter, r.device, r.pid) {
        log(format!("найден зависший диск {}: от прошлого запуска", r.letter));
        RecordCheck { stale: Some(r), clear: false }
    } else {
        RecordCheck { stale: None, clear: true }
    }
}

enum Modal {
    ConfirmRemove,
    Busy(String),
    ConfirmClose,
    Error { title: String, text: String },
    Log,
}

pub struct VramApp {
    link: Link,
    /// Диск создаётся по кнопке в этом окне (а не автозапуском).
    created_here: bool,
    settings: Settings,
    demo: bool,
    autostart_pending: bool,
    minimize_after_create: bool,
    gpu: Arc<Mutex<GpuState>>,
    startup: Arc<Mutex<Startup>>,
    startup_applied: bool,
    imdisk_problem: Option<String>,
    /// Повторная проверка ImDisk в фоне (может ждать запуска драйвера).
    imdisk_recheck: Option<Arc<Mutex<Option<Option<String>>>>>,
    /// Не удаётся записать настройки или журнал рядом с программой.
    storage_problem: Option<String>,
    autostart_on: bool,
    stale: Option<MountRecord>,
    /// Фоновое снятие зависшего диска: результат появится здесь.
    stale_job: Option<JobResult>,
    /// Проверка записи о диске после того, как пропала фоновая часть.
    record_job: Option<Arc<Mutex<Option<RecordCheck>>>>,
    /// Строки журнала для окна «Журнал» и когда они прочитаны.
    log_lines: Option<(Instant, Vec<String>)>,
    /// Букву выбрали вручную — не менять её автоматически.
    letters_touched: bool,
    mem: (u64, u64),
    mem_at: Option<Instant>,
    letters: Vec<char>,
    ram_gb: f64,
    vram_gb: f64,
    sizes_ready: bool,
    modal: Option<Modal>,
    exit_after_remove: bool,
    allow_close: bool,
    samples: VecDeque<(f32, f32)>,
    last: Option<(Instant, u64, u64)>,
    rate: (f64, f64),
    space: Option<(u64, u64)>,
    space_at: Option<Instant>,
    hooked: bool,
    icon: egui::TextureHandle,
}

fn theme_pref(t: ThemeChoice) -> egui::ThemePreference {
    match t {
        ThemeChoice::System => egui::ThemePreference::System,
        ThemeChoice::Light => egui::ThemePreference::Light,
        ThemeChoice::Dark => egui::ThemePreference::Dark,
    }
}

fn ago(d: Duration) -> String {
    let m = d.as_secs() / 60;
    if m < 1 {
        "только что".into()
    } else if m < 60 {
        format!("{m} мин")
    } else {
        format!("{} ч {} мин", m / 60, m % 60)
    }
}

fn demo_gpu() -> GpuInfo {
    GpuInfo {
        index: 0,
        name: "AMD Radeon RX 5700 XT".into(),
        uuid: "demo".into(),
        kind: GpuKind::Discrete,
        vram_total: 8 * GIB,
        budget: Some(7 * GIB + 600 * MIB),
        usage: Some(0),
        api: "Vulkan 1.3.287".into(),
        backend: Api::Vulkan,
        transfer_queue: true,
        luid: None,
        d3d_fallback: true,
    }
}

impl VramApp {
    pub fn new(cc: &eframe::CreationContext<'_>, autostart: bool, demo: bool) -> Self {
        theme::install(&cc.egui_ctx);
        let settings = Settings::load();
        cc.egui_ctx.set_theme(theme_pref(settings.theme));
        let notify: Notify = {
            let ctx = cc.egui_ctx.clone();
            Arc::new(move || ctx.request_repaint())
        };
        let link = Link::new(notify, demo);

        // Видеокарты опрашиваются в фоне: запуск Vulkan занимает доли секунды.
        let gpu = Arc::new(Mutex::new(if demo { GpuState::Ready(vec![demo_gpu()]) } else { GpuState::Loading }));
        if !demo {
            let (g, ctx) = (gpu.clone(), cc.egui_ctx.clone());
            let _ = std::thread::Builder::new().name("gpu-probe".into()).spawn(move || match GpuProbe::new() {
                Ok(p) => loop {
                    let list = p.list();
                    *g.lock().unwrap() = if list.is_empty() {
                        GpuState::Missing("Видеокарта с Vulkan 1.1 или Direct3D 11 не найдена.".into())
                    } else {
                        GpuState::Ready(list)
                    };
                    ctx.request_repaint();
                    std::thread::sleep(Duration::from_secs(2));
                },
                Err(e) => {
                    log(format!("видеокарты: {e}"));
                    *g.lock().unwrap() = GpuState::Missing(e);
                    ctx.request_repaint();
                }
            });
        }

        // Проверки при запуске: ImDisk, автозапуск, зависший диск.
        let startup = Arc::new(Mutex::new(Startup::default()));
        {
            let (st, ctx, legacy) = (startup.clone(), cc.egui_ctx.clone(), settings.mounted.clone());
            let _ = std::thread::Builder::new().name("startup-checks".into()).spawn(move || {
                let mut s = Startup {
                    imdisk: platform::imdisk_problem(),
                    autostart: platform::autostart_enabled(),
                    record: check_record(legacy),
                    ..Default::default()
                };
                s.done = true;
                *st.lock().unwrap() = s;
                ctx.request_repaint();
            });
        }

        Self {
            link,
            created_here: false,
            ram_gb: bytes_gb(settings.ram_mb * MIB),
            vram_gb: bytes_gb(settings.vram_mb * MIB),
            settings,
            demo,
            autostart_pending: autostart,
            minimize_after_create: autostart,
            gpu,
            startup,
            startup_applied: false,
            imdisk_problem: None,
            imdisk_recheck: None,
            storage_problem: logger::problem(),
            autostart_on: false,
            stale: None,
            stale_job: None,
            record_job: None,
            log_lines: None,
            letters_touched: false,
            mem: (0, 0),
            mem_at: None,
            letters: Vec::new(),
            sizes_ready: false,
            modal: None,
            exit_after_remove: false,
            allow_close: false,
            samples: VecDeque::new(),
            last: None,
            rate: (0.0, 0.0),
            space: None,
            space_at: None,
            hooked: false,
            icon: w::load_app_icon(&cc.egui_ctx),
        }
    }

    fn save(&mut self) {
        match self.settings.save() {
            Ok(()) => self.storage_problem = logger::problem(),
            Err(e) => {
                if self.storage_problem.as_ref() != Some(&e) {
                    log(&e);
                }
                self.storage_problem = Some(e);
            }
        }
    }

    fn selected_gpu(&self) -> Option<GpuInfo> {
        match &*self.gpu.lock().unwrap() {
            GpuState::Ready(list) => self
                .settings
                .gpu_uuid
                .as_ref()
                .and_then(|u| list.iter().find(|g| &g.uuid == u))
                .or_else(|| gpu::pick_best(list))
                .cloned(),
            _ => None,
        }
    }

    fn gpu_loading(&self) -> bool {
        matches!(*self.gpu.lock().unwrap(), GpuState::Loading)
    }

    /// Наибольший предел ОЗУ: вся память без запаса системе. Память
    /// занимается по мере записи, поэтому предел может быть больше, чем
    /// свободно сейчас (об этом предупреждаем).
    fn max_ram_gb(&self) -> f64 {
        plan::max_ram_gb(self.mem.0, self.settings.ram_reserve_mb)
    }

    fn max_vram_gb(&self, g: Option<&GpuInfo>) -> f64 {
        plan::max_vram_gb(g, self.settings.vram_reserve_mb)
    }

    /// Размеры частей, которые получит диск по кнопке «Создать».
    fn planned(&self) -> (u64, u64) {
        let gpu = self.selected_gpu();
        let ram = gb_to_bytes(self.ram_gb.min(self.max_ram_gb()));
        let vram = if gpu.is_some() { gb_to_bytes(self.vram_gb.min(self.max_vram_gb(gpu.as_ref()))) } else { 0 };
        (ram, vram)
    }

    fn view(&self) -> View {
        self.link.view()
    }

    /// Фоновые результаты, события движка, замеры.
    fn poll(&mut self, ctx: &egui::Context, view: &View) {
        let rechecked = self.imdisk_recheck.as_ref().and_then(|slot| slot.lock().unwrap().take());
        if let Some(r) = rechecked {
            self.imdisk_recheck = None;
            match &r {
                Some(p) => log(format!("ImDisk (повторная проверка): {p}")),
                None => log("ImDisk готов к работе"),
            }
            self.imdisk_problem = r;
        }
        if !self.startup_applied {
            let s = self.startup.lock().unwrap();
            if s.done {
                self.imdisk_problem = s.imdisk.clone();
                self.autostart_on = s.autostart;
                self.stale = s.record.stale.clone();
                let clear = s.record.clear;
                drop(s);
                self.startup_applied = true;
                if let Some(p) = &self.imdisk_problem {
                    log(format!("ImDisk: {p}"));
                }
                if self.settings.autostart != self.autostart_on {
                    self.settings.autostart = self.autostart_on;
                    self.save();
                }
                if clear {
                    self.forget_record();
                }
            }
        }

        let checked = self.record_job.as_ref().and_then(|j| j.lock().unwrap().take());
        if let Some(c) = checked {
            self.record_job = None;
            if c.clear {
                self.forget_record();
            }
            self.stale = c.stale;
        }

        let finished = self.stale_job.as_ref().and_then(|j| j.lock().unwrap().take());
        if let Some(r) = finished {
            self.stale_job = None;
            let st = self.stale.clone();
            match (r, st) {
                (Ok(()), Some(st)) => {
                    log(format!("зависший диск {}: убран", st.letter));
                    self.stale = None;
                    MountRecord::clear();
                    self.settings.mounted = None;
                    // Буква освободилась — вернуть её, если её выбирали раньше.
                    self.letters = platform::free_letters();
                    self.mem_at = Some(Instant::now());
                    if self.letters.contains(&st.letter) && !self.letters_touched {
                        self.settings.letter = st.letter;
                    }
                    self.save();
                }
                (Err(e), _) => self.modal = Some(Modal::Error { title: "Не удалось убрать диск".into(), text: e }),
                _ => {}
            }
        }

        let now = Instant::now();
        if self.mem_at.is_none_or(|t| now - t > Duration::from_secs(2)) {
            self.mem = platform::memory();
            self.letters = platform::free_letters();
            self.mem_at = Some(now);
        }

        // Начальные размеры — когда известны память и видеокарта.
        if !self.sizes_ready && !self.gpu_loading() && self.mem.0 > 0 {
            let g = self.selected_gpu();
            let (max_r, max_v) = (self.max_ram_gb(), self.max_vram_gb(g.as_ref()));
            if self.settings.fresh {
                self.ram_gb = floor_step((bytes_gb(self.mem.0) / 4.0).min(4.0)).min(max_r);
                self.vram_gb = g.as_ref().map(|g| floor_step(bytes_gb(g.vram_total) / 2.0)).unwrap_or(0.0).min(max_v);
            } else {
                self.ram_gb = self.ram_gb.min(max_r);
                self.vram_gb = self.vram_gb.min(max_v);
            }
            if let Some(c) = plan::pick_letter(self.settings.letter, &self.letters) {
                self.settings.letter = c;
            }
            self.sizes_ready = true;
        }

        // Автозапуск: создать диск с сохранёнными настройками.
        if self.autostart_pending && self.sizes_ready && self.startup_applied && matches!(view, View::Idle) {
            self.autostart_pending = false;
            if self.imdisk_problem.is_none() && self.stale.is_none() {
                log("автозапуск: создаю диск");
                self.start_create(false);
            }
        }

        self.link.tick();
        for e in self.link.events() {
            // Буквы и память изменились — обновить сразу, а не через 2 с.
            self.mem_at = None;
            ctx.request_repaint();
            match e {
                Event::Created { letter, notes, .. } => {
                    if !self.link.background() {
                        for n in &notes {
                            log(n);
                        }
                    }
                    self.samples.clear();
                    self.last = None;
                    self.space_at = None;
                    // Диск, созданный без окна (автозапуском), Проводник не открывает.
                    if self.settings.open_explorer && self.created_here && !self.minimize_after_create {
                        platform::open_drive(letter);
                    }
                    self.created_here = false;
                    if self.minimize_after_create {
                        self.minimize_after_create = false;
                        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
                    }
                }
                Event::Failed { title, text } => {
                    self.minimize_after_create = false;
                    if !matches!(self.view(), View::Mounted(_)) {
                        self.created_here = false;
                    }
                    if self.exit_after_remove {
                        self.exit_after_remove = false;
                    }
                    self.modal = Some(Modal::Error { title, text });
                }
                Event::Busy { text } => {
                    if self.exit_after_remove {
                        self.link.remove(true);
                    } else {
                        self.modal = Some(Modal::Busy(text));
                    }
                }
                Event::Removed => {
                    self.mem_at = None;
                    self.space = None;
                    if self.exit_after_remove {
                        self.allow_close = true;
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                }
                Event::Lost { letter } => {
                    self.exit_after_remove = false;
                    self.modal = Some(Modal::Error {
                        title: "Фоновая часть VramDisk завершилась".into(),
                        text: format!(
                            "Диск {letter}: больше не обслуживается: процесс VramDisk, который его держал, закрылся — \
                             аварийно или через Диспетчер задач. Подробности — в журнале."
                        ),
                    });
                    // Остался ли диск — проверить и предложить убрать.
                    let job = Arc::new(Mutex::new(None));
                    let (j, ctx2, legacy) = (job.clone(), ctx.clone(), self.settings.mounted.clone());
                    let _ = std::thread::Builder::new().name("record-check".into()).spawn(move || {
                        *j.lock().unwrap() = Some(check_record(legacy));
                        ctx2.request_repaint();
                    });
                    self.record_job = Some(job);
                }
            }
        }

        if let View::Mounted(m) = view {
            let s = m.stats;
            let (rd, wr) = (s.read_ram + s.read_vram, s.write_ram + s.write_vram);
            match self.last {
                Some((t, r0, w0)) => {
                    let dt = (now - t).as_secs_f64();
                    if dt >= 0.5 {
                        self.rate = ((rd - r0) as f64 / dt, (wr - w0) as f64 / dt);
                        self.samples.push_back((self.rate.0 as f32, self.rate.1 as f32));
                        while self.samples.len() > SAMPLES {
                            self.samples.pop_front();
                        }
                        self.last = Some((now, rd, wr));
                    }
                }
                None => self.last = Some((now, rd, wr)),
            }
            if self.space_at.is_none_or(|t| now - t > Duration::from_secs(1)) {
                self.space = platform::disk_space(m.letter);
                self.space_at = Some(now);
            }
            ctx.request_repaint_after(Duration::from_millis(500));
        } else {
            ctx.request_repaint_after(Duration::from_secs(2));
        }
    }

    /// `remember` — сохранить размеры в настройках. При автозапуске не
    /// сохраняем: если памяти временно меньше (или видеокарта ещё не готова),
    /// диск будет меньше, но настройки на следующий раз останутся прежними.
    fn start_create(&mut self, remember: bool) {
        let g = self.selected_gpu();
        let ram = gb_to_bytes(self.ram_gb);
        let vram = if g.is_some() || self.demo { gb_to_bytes(self.vram_gb) } else { 0 };
        if ram + vram == 0 {
            if !remember {
                log("автозапуск: нет свободной памяти для диска");
            }
            return;
        }
        if !remember && g.is_none() && !self.demo && self.settings.vram_mb > 0 {
            log("автозапуск: видеокарта недоступна — диск будет только из оперативной памяти");
        }
        if remember {
            self.settings.ram_mb = ram / MIB;
            self.settings.vram_mb = vram / MIB;
        } else if ram / MIB < self.settings.ram_mb || vram / MIB < self.settings.vram_mb {
            log(format!(
                "автозапуск: памяти меньше, чем в настройках — диск {} вместо {}",
                fsize(ram + vram),
                fsize((self.settings.ram_mb + self.settings.vram_mb) * MIB)
            ));
        }
        self.settings.label = clean_label(&self.settings.label, self.settings.fs);
        if self.settings.fs != FsKind::Ntfs {
            self.settings.compress = false;
        }
        self.settings.fresh = false;
        self.save();
        self.created_here = true;
        self.link.create(plan::request(&self.settings, ram, vram, self.settings.letter, g.as_ref(), self.demo));
    }

    /// Убрать устаревшую запись о диске (и запись версии 2.0 в настройках).
    fn forget_record(&mut self) {
        MountRecord::clear();
        if self.settings.mounted.take().is_some() {
            self.save();
        }
    }

    fn handle_close(&mut self, ctx: &egui::Context, view: &View) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.allow_close {
            return;
        }
        match view {
            View::Idle => {}
            View::Creating(_) => {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.link.cancel();
            }
            // Диск живёт в фоновой части: окно просто закрывается.
            View::Mounted(_) | View::Removing { .. } if self.link.background() => {
                log("окно закрыто, диск работает в фоне");
            }
            View::Mounted(_) => {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.modal = Some(Modal::ConfirmClose);
            }
            View::Removing { .. } => {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.exit_after_remove = true;
            }
        }
    }

    // ---------- шапка и подвал ----------

    fn header(&self, ui: &mut egui::Ui, pal: &Palette, view: &View) {
        ui.horizontal(|ui| {
            w::app_badge(ui, &self.icon, 36.0);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("VramDisk").font(bold(19.0)).color(pal.text));
                ui.label(RichText::new("Диск из оперативной памяти и видеопамяти").font(font(12.5)).color(pal.weak));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (text, fg) = match view {
                    View::Idle => ("Диск не создан".to_string(), pal.weak),
                    View::Creating(c) => (format!("Создание {}:", c.letter), pal.accent),
                    View::Mounted(m) if !m.running => (format!("{}: не отвечает", m.letter), pal.danger),
                    View::Mounted(m) => (format!("Диск {}: работает", m.letter), pal.ok),
                    View::Removing { letter, .. } => (format!("Отключение {letter}:"), pal.warn),
                };
                let bg = if matches!(view, View::Idle) { pal.track } else { fg.gamma_multiply(0.14) };
                w::pill(ui, &text, fg, bg, !matches!(view, View::Idle));
            });
        });
    }

    fn footer(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        ui.horizontal(|ui| {
            let mut t = format!("Версия {}", vramdisk::VERSION);
            if self.demo {
                t.push_str(" · демо-режим");
            }
            ui.label(RichText::new(t).font(font(12.0)).color(pal.faint));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let link = |ui: &mut egui::Ui, text: &str| {
                    ui.add(egui::Button::new(RichText::new(text).font(font(12.5)).color(pal.weak)).frame(false))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                };
                if link(ui, "Журнал").clicked() {
                    self.modal = Some(Modal::Log);
                }
                let th = self.settings.theme;
                if link(ui, &format!("Тема: {}", th.label())).on_hover_text("Сменить тему оформления").clicked() {
                    self.settings.theme = th.next();
                    ui.ctx().set_theme(theme_pref(self.settings.theme));
                    self.save();
                }
                w::icon(ui, Icon::Contrast, 14.0, pal.weak);
            });
        });
    }

    // ---------- экран настройки ----------

    fn storage_banner(&self, ui: &mut egui::Ui, pal: &Palette) {
        if let Some(p) = &self.storage_problem {
            w::banner(ui, pal.warn, pal.warn_bg, Icon::Warning, |ui| {
                ui.label(RichText::new("Не удаётся записать файлы рядом с программой").font(bold(14.5)).color(pal.text));
                ui.label(RichText::new(p).font(font(13.0)).color(pal.weak));
                ui.label(
                    RichText::new(
                        "VramDisk хранит настройки и журнал в своей папке. Переместите vramdisk.exe в папку, куда можно записывать, \
                         например C:\\VramDisk.",
                    )
                    .font(font(13.0))
                    .color(pal.weak),
                );
            });
            ui.add_space(4.0);
        }
    }

    fn idle_view(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        self.storage_banner(ui, pal);
        if let Some(p) = self.imdisk_problem.clone() {
            w::banner(ui, pal.danger, pal.danger_bg, Icon::Warning, |ui| {
                ui.label(RichText::new("ImDisk не готов к работе").font(bold(14.5)).color(pal.text));
                ui.label(RichText::new(&p).font(font(13.0)).color(pal.weak));
                ui.label(
                    RichText::new("VramDisk подключает диск через драйвер ImDisk. Исправьте проблему и нажмите «Проверить снова».")
                        .font(font(13.0))
                        .color(pal.weak),
                );
                ui.horizontal(|ui| {
                    if ui.button("Скачать ImDisk Toolkit").clicked() {
                        platform::open_url(IMDISK_URL);
                    }
                    let busy = self.imdisk_recheck.is_some();
                    let text = if busy { "Проверяю…" } else { "Проверить снова" };
                    if ui.add_enabled(!busy, egui::Button::new(text)).clicked() {
                        let slot = Arc::new(Mutex::new(None));
                        let (s2, ctx) = (slot.clone(), ui.ctx().clone());
                        let _ = std::thread::Builder::new().name("imdisk-check".into()).spawn(move || {
                            *s2.lock().unwrap() = Some(platform::imdisk_problem());
                            ctx.request_repaint();
                        });
                        self.imdisk_recheck = Some(slot);
                    }
                });
            });
            ui.add_space(4.0);
        }
        if let Some(st) = self.stale.clone() {
            w::banner(ui, pal.warn, pal.warn_bg, Icon::Warning, |ui| {
                ui.label(RichText::new(format!("Остался диск {}: от прошлого запуска", st.letter)).font(bold(14.5)).color(pal.text));
                ui.label(
                    RichText::new(
                        "VramDisk был закрыт аварийно (или через Диспетчер задач), и этот диск больше не работает: обращения к нему \
                         зависают. Его стоит убрать.",
                    )
                        .font(font(13.0))
                        .color(pal.weak),
                );
                if self.stale_job.is_some() {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(16.0));
                        ui.label(RichText::new(format!("Убираю диск {}:…", st.letter)).color(pal.weak));
                    });
                } else if ui.button(format!("Убрать диск {}:", st.letter)).clicked() {
                    let job = Arc::new(Mutex::new(None));
                    let (j, ctx) = (job.clone(), ui.ctx().clone());
                    std::thread::spawn(move || {
                        let r = platform::remove_stale(st.letter, st.device);
                        *j.lock().unwrap() = Some(r);
                        ctx.request_repaint();
                    });
                    self.stale_job = Some(job);
                }
            });
            ui.add_space(4.0);
        }

        let gpu = self.selected_gpu();
        let max_r = self.max_ram_gb();
        let max_v = self.max_vram_gb(gpu.as_ref());
        // Пока память и видеокарта не определены, пределы нулевые — не
        // трогаем сохранённые размеры.
        if self.sizes_ready {
            self.ram_gb = self.ram_gb.min(max_r);
            self.vram_gb = self.vram_gb.min(max_v);
        }
        let ram_b = gb_to_bytes(self.ram_gb);
        let vram_b = if gpu.is_some() { gb_to_bytes(self.vram_gb) } else { 0 };

        // Видеокарта
        w::card(ui, pal, |ui| {
            let api = gpu.as_ref().map(|g| RichText::new(&g.api).font(font(12.5)).color(pal.faint));
            w::card_title(ui, pal, Icon::Gpu, pal.vram, "Видеокарта", api);
            let state = match &*self.gpu.lock().unwrap() {
                GpuState::Loading => Err(None),
                GpuState::Missing(e) => Err(Some(e.clone())),
                GpuState::Ready(list) => Ok(list.clone()),
            };
            match state {
                Err(None) => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(16.0));
                        ui.label(RichText::new("Поиск видеокарты…").color(pal.weak));
                    });
                }
                Err(Some(e)) => {
                    ui.label(RichText::new(e).color(pal.weak));
                    ui.label(RichText::new("Диск можно создать только из оперативной памяти.").font(font(13.0)).color(pal.faint));
                }
                Ok(list) => {
                    let g = gpu.clone().unwrap_or_else(|| list[0].clone());
                    if list.len() > 1 {
                        let mut sel = g.uuid.clone();
                        egui::ComboBox::from_id_salt("gpu")
                            .width(ui.available_width())
                            .selected_text(&g.name)
                            .show_ui(ui, |ui| {
                                for x in &list {
                                    ui.selectable_value(&mut sel, x.uuid.clone(), format!("{} · {} · {}", x.name, x.kind.label(), x.api));
                                }
                            });
                        if sel != g.uuid {
                            self.settings.gpu_uuid = Some(sel);
                            self.save();
                        }
                    } else {
                        ui.label(RichText::new(&g.name).font(bold(14.5)).color(pal.text));
                    }
                    let total = g.vram_total.max(1) as f64;
                    let avail = g.available().min(g.vram_total);
                    let others = g.vram_total - avail;
                    w::bar(ui, pal, 10.0, &[(others as f64 / total, pal.other), (vram_b as f64 / total, pal.vram)]);
                    ui.horizontal(|ui| {
                        w::legend(ui, pal, pal.other, &format!("занято {}", fsize(others)));
                        w::legend(ui, pal, pal.vram, &format!("диску до {}", fsize(vram_b)));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(
                                RichText::new(format!("свободно {} из {}", fsize(avail), fsize(g.vram_total)))
                                    .font(font(13.0))
                                    .color(pal.weak),
                            );
                        });
                    });
                    if g.backend == Api::D3d11 {
                        ui.label(
                            RichText::new("Vulkan для этой видеокарты недоступен — видеопамять подключится через Direct3D 11.")
                                .font(font(12.5))
                                .color(pal.faint),
                        );
                    }
                    if g.kind != GpuKind::Discrete && g.kind != GpuKind::Virtual {
                        ui.label(
                            RichText::new(format!(
                                "Это {} видеокарта: её «видеопамять» — та же оперативная память, выигрыша не будет.",
                                g.kind.label()
                            ))
                            .font(font(12.5))
                            .color(pal.warn),
                        );
                    }
                }
            }
        });

        // Оперативная память
        w::card(ui, pal, |ui| {
            w::card_title(ui, pal, Icon::Ram, pal.ram, "Оперативная память", None);
            let (total, avail) = self.mem;
            let t = total.max(1) as f64;
            let others = total.saturating_sub(avail);
            w::bar(ui, pal, 10.0, &[(others as f64 / t, pal.other), (ram_b as f64 / t, pal.ram)]);
            ui.horizontal(|ui| {
                w::legend(ui, pal, pal.other, &format!("занято {}", fsize(others)));
                w::legend(ui, pal, pal.ram, &format!("диску до {}", fsize(ram_b)));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("свободно {} из {}", fsize(avail), fsize(total))).font(font(13.0)).color(pal.weak),
                    );
                });
            });
        });

        // Размер
        let free_ram = self.mem.1;
        let free_vram = gpu.as_ref().map(|g| g.available());
        w::card(ui, pal, |ui| {
            let total = ram_b + vram_b;
            w::card_title(
                ui,
                pal,
                Icon::Disk,
                pal.accent,
                "Размер диска",
                Some(RichText::new(fsize(total)).font(bold(15.0)).color(pal.text)),
            );
            size_slider(ui, pal, "Оперативная память — до", &mut self.ram_gb, max_r, pal.ram, true, Some(free_ram));
            ui.add_space(2.0);
            size_slider(ui, pal, "Видеопамять — до", &mut self.vram_gb, max_v, pal.vram, gpu.is_some(), free_vram);
            if total > 0 {
                ui.add_space(4.0);
                let both = ram_b > 0 && vram_b > 0;
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Сначала занимать").color(if both { pal.weak } else { pal.faint }));
                    let mut f = self.settings.fill_first;
                    if w::segmented(ui, pal, &mut f, &[(FillFirst::Ram, "оперативную память"), (FillFirst::Vram, "видеопамять")], both)
                        && f != self.settings.fill_first
                    {
                        self.settings.fill_first = f;
                        self.save();
                    }
                });
                ui.label(
                    RichText::new(
                        "Память занимается по мере записи и возвращается после удаления файлов — пустой диск почти ничего не занимает. \
                         Когда первая память заполнена до предела, данные идут во вторую.",
                    )
                    .font(font(12.5))
                    .color(pal.faint),
                );
                if ram_b > free_ram && !self.demo {
                    ui.label(
                        RichText::new(format!(
                            "Сейчас свободно {} оперативной памяти. Если диск заполнится сильнее, Windows начнёт выгружать программы в файл подкачки.",
                            fsize(free_ram)
                        ))
                        .font(font(12.5))
                        .color(pal.warn),
                    );
                }
                if let Some(fv) = free_vram.filter(|&fv| vram_b > fv) {
                    ui.label(
                        RichText::new(format!(
                            "Сейчас свободно {} видеопамяти. Пока её занимают другие программы, данные будут временно ложиться в оперативную память.",
                            fsize(fv)
                        ))
                        .font(font(12.5))
                        .color(pal.warn),
                    );
                }
            }
        });

        // Параметры
        w::card(ui, pal, |ui| {
            w::card_title(ui, pal, Icon::Sliders, pal.weak, "Параметры", None);
            egui::Grid::new("opts").num_columns(2).spacing(vec2(16.0, 10.0)).show(ui, |ui| {
                ui.label(RichText::new("Буква диска").color(pal.weak));
                let mut letter = self.settings.letter;
                egui::ComboBox::from_id_salt("letter").width(90.0).selected_text(format!("{letter}:")).show_ui(ui, |ui| {
                    for &c in &self.letters {
                        ui.selectable_value(&mut letter, c, format!("{c}:"));
                    }
                });
                if letter != self.settings.letter {
                    self.settings.letter = letter;
                    self.letters_touched = true;
                }
                ui.end_row();

                ui.label(RichText::new("Метка тома").color(pal.weak));
                let limit = self.settings.fs.label_limit();
                ui.add(egui::TextEdit::singleline(&mut self.settings.label).char_limit(limit).desired_width(200.0).hint_text("VramDisk"));
                ui.end_row();

                ui.label(RichText::new("Файловая система").color(pal.weak));
                let mut fs = self.settings.fs;
                if w::segmented(ui, pal, &mut fs, &[(FsKind::Ntfs, "NTFS"), (FsKind::ExFat, "exFAT")], true) {
                    self.settings.fs = fs;
                    self.settings.label = clean_label(&self.settings.label, fs);
                    if fs != FsKind::Ntfs {
                        self.settings.compress = false;
                    }
                }
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.separator();
            let ntfs = self.settings.fs == FsKind::Ntfs;
            w::toggle_row(
                ui,
                pal,
                "Сжатие NTFS",
                "Больше места для текстов и кода, но медленнее и нагружает процессор",
                &mut self.settings.compress,
                ntfs,
            );
            w::toggle_row(ui, pal, "Открыть в Проводнике", "Сразу после создания диска", &mut self.settings.open_explorer, true);
            let mut auto = self.autostart_on;
            let r = w::toggle_row(
                ui,
                pal,
                "Создавать при входе в Windows",
                "Диск с этими настройками будет создаваться автоматически",
                &mut auto,
                self.startup_applied,
            );
            if r.changed() {
                match platform::set_autostart(auto) {
                    Ok(()) => {
                        self.autostart_on = auto;
                        self.settings.autostart = auto;
                        // Для автозапуска нужны сохранённые размеры.
                        self.settings.ram_mb = ram_b / MIB;
                        self.settings.vram_mb = vram_b / MIB;
                        self.settings.fresh = false;
                        self.save();
                        log(format!("автозапуск {}", if auto { "включён" } else { "выключен" }));
                    }
                    Err(e) => self.modal = Some(Modal::Error { title: "Не удалось изменить автозапуск".into(), text: e }),
                }
            }
        });
    }

    /// Закреплённая внизу кнопка «Создать диск» с предупреждением.
    fn idle_actions(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let (ram_b, vram_b) = self.planned();
        let total = ram_b + vram_b;
        ui.horizontal_top(|ui| {
            w::icon(ui, Icon::Info, 16.0, pal.faint);
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Диск живёт только в памяти: его содержимое пропадёт при выключении или перезагрузке компьютера и при сбое драйвера видеокарты.",
                    )
                    .font(font(12.5))
                    .color(pal.weak),
                )
                .wrap(),
            );
        });
        ui.add_space(8.0);
        let letter = self.settings.letter;
        let blocked = if !self.startup_applied {
            Some("Проверка системы…".to_string())
        } else if let Some(p) = &self.imdisk_problem {
            Some(if p.starts_with("ImDisk не установлен") {
                "Сначала установите ImDisk".to_string()
            } else {
                "Сначала исправьте ImDisk — см. выше".to_string()
            })
        } else if total == 0 {
            Some("Выберите размер диска".to_string())
        } else if total < plan::MIN_SIZE {
            Some(format!("Минимальный размер — {}", fsize(plan::MIN_SIZE)))
        } else if !self.letters.contains(&letter) {
            Some(format!("Буква {letter}: занята — выберите другую"))
        } else {
            None
        };
        let text = blocked.clone().unwrap_or_else(|| format!("Создать диск {letter}: · {}", fsize(total)));
        if w::button(ui, pal, ButtonKind::Primary, &text, blocked.is_none().then_some(Icon::Bolt), ui.available_width(), blocked.is_none())
            .clicked()
        {
            self.start_create(true);
        }
    }

    // ---------- создание ----------

    fn creating_view(&mut self, ui: &mut egui::Ui, pal: &Palette, c: &CreatingView) {
        w::card(ui, pal, |ui| {
            w::card_title(
                ui,
                pal,
                Icon::Disk,
                pal.accent,
                &format!("Создание диска {}:", c.letter),
                Some(RichText::new(fsize(c.total)).font(bold(15.0)).color(pal.text)),
            );
            ui.add_space(4.0);
            for s in &c.steps {
                w::step_row(ui, pal, s.mark, &s.title, s.detail.as_deref());
            }
            if c.can_cancel {
                ui.add_space(6.0);
                if w::button(ui, pal, ButtonKind::Secondary, "Отмена", None, 140.0, true).clicked() {
                    self.link.cancel();
                }
            }
        });
        ui.label(
            RichText::new("Память пока не занимается: она будет браться по мере записи на диск. Видеопамять перед подключением проверяется.")
                .font(font(12.5))
                .color(pal.faint),
        );
    }

    // ---------- подключённый диск ----------

    fn mounted_view(&mut self, ui: &mut egui::Ui, pal: &Palette, m: &MountedView) {
        let s = m.stats;
        self.storage_banner(ui, pal);
        if s.device_lost {
            w::banner(ui, pal.danger, pal.danger_bg, Icon::Warning, |ui| {
                ui.label(RichText::new("Видеокарта была сброшена").font(bold(14.5)).color(pal.text));
                ui.label(
                    RichText::new("Драйвер видеокарты перезапустился, данные в видеопамяти потеряны. Скопируйте с диска то, что ещё читается, и отключите его.")
                        .font(font(13.0))
                        .color(pal.weak),
                );
            });
        } else if !m.running {
            w::banner(ui, pal.danger, pal.danger_bg, Icon::Warning, |ui| {
                ui.label(RichText::new("Диск перестал отвечать").font(bold(14.5)).color(pal.text));
                ui.label(RichText::new("Сервер диска остановился. Отключите диск.").font(font(13.0)).color(pal.weak));
            });
        }

        // Главная карточка
        w::card(ui, pal, |ui| {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(46.0, 46.0), egui::Sense::hover());
                ui.painter().rect_filled(r, egui::CornerRadius::same(12), pal.accent.gamma_multiply(0.12));
                w::paint_icon(ui.painter(), r.shrink(10.0), Icon::Disk, pal.accent);
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    let label = if m.label.is_empty() { "Локальный диск".to_string() } else { m.label.clone() };
                    ui.label(RichText::new(format!("{}:  {}", m.letter, label)).font(bold(21.0)).color(pal.text));
                    ui.label(
                        RichText::new(format!(
                            "{} · {} · работает {}",
                            m.fs.label(),
                            fsize(m.size),
                            ago(Duration::from_millis(m.uptime_ms))
                        ))
                            .font(font(13.0))
                            .color(pal.weak),
                    );
                });
            });
            ui.add_space(6.0);
            if let Some((total, free)) = self.space {
                let used = total.saturating_sub(free);
                w::bar(ui, pal, 10.0, &[(used as f64 / total.max(1) as f64, pal.accent)]);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("занято {}", fsize(used))).font(font(13.0)).color(pal.weak));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("свободно {} из {}", fsize(free), fsize(total))).font(font(13.0)).color(pal.weak),
                        );
                    });
                });
            }
        });

        if s.over_ram > 0 {
            w::banner(ui, pal.warn, pal.warn_bg, Icon::Info, |ui| {
                ui.label(RichText::new("Видеопамять занята другими программами").font(bold(14.0)).color(pal.text));
                ui.label(
                    RichText::new(format!(
                        "{} данных временно лежат в оперативной памяти сверх заданного предела. Когда видеопамять освободится, они переедут обратно.",
                        fsize(s.over_ram)
                    ))
                    .font(font(13.0))
                    .color(pal.weak),
                );
            });
        }
        if s.over_vram > 0 {
            w::banner(ui, pal.warn, pal.warn_bg, Icon::Info, |ui| {
                ui.label(
                    RichText::new(format!(
                        "Оперативной памяти не хватило: {} данных временно лежат в видеопамяти сверх заданного предела.",
                        fsize(s.over_vram)
                    ))
                    .font(font(13.0))
                    .color(pal.text),
                );
            });
        }
        if s.nomem > 0 {
            w::banner(ui, pal.danger, pal.danger_bg, Icon::Warning, |ui| {
                ui.label(RichText::new("Не хватило памяти для записи").font(bold(14.0)).color(pal.text));
                ui.label(
                    RichText::new(format!(
                        "Запись на диск не прошла {} раз: ни оперативная память, ни видеопамять не дали места. Удалите ненужные файлы с диска или закройте программы.",
                        s.nomem
                    ))
                    .font(font(13.0))
                    .color(pal.weak),
                );
            });
        }

        // Где лежат данные
        w::card(ui, pal, |ui| {
            w::card_title(
                ui,
                pal,
                Icon::Ram,
                pal.weak,
                "Где лежат данные",
                Some(RichText::new(format!("сначала {}", m.first.label())).font(font(12.5)).color(pal.faint)),
            );
            if m.ram > 0 || s.held_ram > 0 {
                part_row(ui, pal, "Оперативная память", None, s.used_ram, s.held_ram, m.ram, pal.ram);
            }
            if m.vram > 0 {
                let sub = match (&m.gpu_name, &m.gpu_api) {
                    (Some(n), Some(a)) => Some(format!("{n} · {a}")),
                    (Some(n), None) => Some(n.clone()),
                    _ => None,
                };
                part_row(ui, pal, "Видеопамять", sub.as_deref(), s.used_vram, s.held_vram, m.vram, pal.vram);
            }
            let mut line = format!("Память занимается по мере записи: сейчас {} из {}", fsize(s.held_ram + s.held_vram), fsize(m.size));
            if s.freed() > 0 {
                line.push_str(&format!(" · возвращено после удаления файлов {}", fsize(s.freed())));
            }
            ui.label(RichText::new(line).font(font(12.5)).color(pal.faint));
            // Столько же показывает Диспетчер задач (частный рабочий набор).
            // Пусто, если Windows не ответила.
            let total = self.link.server_mem().map(|m| m.private_ws.unwrap_or(m.private)).filter(|&t| t > 0);
            if let Some(total) = total {
                let mut t = format!("Фоновая часть VramDisk занимает {} оперативной памяти", fsize(total));
                if s.held_ram == 0 {
                    t.push_str(": драйвер видеокарты и служебные данные VramDisk, данных диска в ней нет");
                } else if s.held_ram < total {
                    t.push_str(&format!(
                        ": данные диска — {}, остальное — драйвер видеокарты и служебные данные VramDisk",
                        fsize(s.held_ram)
                    ));
                } else {
                    // Не все страницы данных сейчас в рабочем наборе — не
                    // гадаем почему, просто показываем обе цифры.
                    t.push_str(&format!("; данные диска в оперативной памяти — {}", fsize(s.held_ram)));
                }
                t.push('.');
                ui.label(RichText::new(t).font(font(12.5)).color(pal.faint));
            }
        });

        // Активность
        w::card(ui, pal, |ui| {
            w::card_title(
                ui,
                pal,
                Icon::Activity,
                pal.accent,
                "Активность",
                Some(RichText::new("за минуту").font(font(12.5)).color(pal.faint)),
            );
            ui.columns(4, |cols| {
                w::stat(&mut cols[0], pal, "Чтение", &speed(self.rate.0), Some(pal.read));
                w::stat(&mut cols[1], pal, "Запись", &speed(self.rate.1), Some(pal.write));
                w::stat(&mut cols[2], pal, "Прочитано", &fsize(s.read_ram + s.read_vram), None);
                w::stat(&mut cols[3], pal, "Записано", &fsize(s.write_ram + s.write_vram), None);
            });
            let data: Vec<(f32, f32)> = self.samples.iter().copied().collect();
            w::sparkline(ui, pal, &data, 70.0);
            if s.errors > 0 {
                ui.label(RichText::new(format!("Ошибок ввода-вывода: {} — подробности в журнале", s.errors)).font(font(12.5)).color(pal.danger));
            }
        });

        if let Some(t) = &m.test {
            ui.horizontal(|ui| {
                w::icon(ui, Icon::Check, 16.0, pal.ok);
                ui.label(
                    RichText::new(format!("Видеопамять проверена: {}", test_line(t))).font(font(12.5)).color(pal.weak),
                );
            });
        }
        for n in &m.notes {
            w::banner(ui, pal.warn, pal.warn_bg, Icon::Info, |ui| {
                ui.label(RichText::new(n).font(font(13.0)).color(pal.text));
            });
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let half = (ui.available_width() - 8.0) / 2.0;
            if w::button(ui, pal, ButtonKind::Secondary, "Открыть в Проводнике", Some(Icon::Folder), half, m.running).clicked() {
                platform::open_drive(m.letter);
            }
            if w::button(ui, pal, ButtonKind::Danger, "Отключить диск", Some(Icon::Power), half, true).clicked() {
                self.modal = Some(Modal::ConfirmRemove);
            }
        });
        let hint = if self.link.background() {
            "Окно можно закрыть: диск продолжит работать в фоне, а VramDisk можно открыть снова в любой момент. Отключайте диск \
             этой кнопкой, а не через Диспетчер задач. При выключении Windows диск отключится сам."
        } else {
            "Диск работает, пока открыта программа. Окно можно свернуть; при выключении Windows диск отключится сам."
        };
        ui.label(RichText::new(hint).font(font(12.5)).color(pal.faint));
    }

    fn removing_view(&self, ui: &mut egui::Ui, pal: &Palette, letter: char, force: bool) {
        w::card(ui, pal, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(20.0).color(pal.accent));
                ui.label(
                    RichText::new(format!("Отключение диска {letter}:{}…", if force { " (принудительно)" } else { "" }))
                        .font(bold(15.0))
                        .color(pal.text),
                );
            });
            ui.label(RichText::new("Файловая система сбрасывает данные, затем память освобождается.").font(font(13.0)).color(pal.weak));
        });
    }

    // ---------- окна ----------

    fn modals(&mut self, ctx: &egui::Context, pal: &Palette, view: &View) {
        let Some(modal) = self.modal.take() else { return };
        let letter = match view {
            View::Mounted(m) => m.letter,
            _ => self.settings.letter,
        };
        let mut keep = true;
        let frame = egui::Frame::new()
            .fill(pal.card)
            .stroke(egui::Stroke::new(1.0, pal.border))
            .corner_radius(egui::CornerRadius::same(14))
            .inner_margin(Margin::same(22))
            .shadow(ctx.global_style().visuals.window_shadow);
        let resp = egui::Modal::new(Id::unique("vramdisk-modal")).frame(frame).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 90.0).clamp(300.0, 420.0));
            match &modal {
                Modal::ConfirmRemove => {
                    modal_title(ui, pal, Icon::Power, pal.danger, &format!("Отключить диск {letter}:?"));
                    modal_text(ui, pal, "Все файлы на диске будут удалены безвозвратно. Скопируйте нужное заранее.");
                    match modal_buttons(ui, pal, &[("Отмена", ButtonKind::Secondary), ("Отключить", ButtonKind::Danger)]) {
                        Some(0) => keep = false,
                        Some(_) => {
                            keep = false;
                            self.link.remove(false);
                        }
                        None => {}
                    }
                }
                Modal::Busy(t) => {
                    modal_title(ui, pal, Icon::Warning, pal.warn, &format!("Диск {letter}: занят"));
                    modal_text(
                        ui,
                        pal,
                        "Диск используется — например, открыт в Проводнике или в нём работает программа. Закройте окна и программы и попробуйте ещё раз — или отключите принудительно: несохранённые изменения в открытых файлах пропадут.",
                    );
                    if !t.trim().is_empty() {
                        ui.label(RichText::new(t.trim()).font(font(12.0)).color(pal.faint));
                    }
                    match modal_buttons(
                        ui,
                        pal,
                        &[("Отмена", ButtonKind::Secondary), ("Ещё раз", ButtonKind::Secondary), ("Принудительно", ButtonKind::Danger)],
                    ) {
                        Some(0) => keep = false,
                        Some(1) => {
                            keep = false;
                            self.link.remove(false);
                        }
                        Some(_) => {
                            keep = false;
                            self.link.remove(true);
                        }
                        None => {}
                    }
                }
                Modal::ConfirmClose => {
                    modal_title(ui, pal, Icon::Warning, pal.warn, &format!("Диск {letter}: ещё подключён"));
                    modal_text(
                        ui,
                        pal,
                        "Диск работает, только пока открыта программа. Если выйти, диск будет отключён, а всё его содержимое пропадёт. Окно можно просто свернуть.",
                    );
                    match modal_buttons(
                        ui,
                        pal,
                        &[("Отмена", ButtonKind::Secondary), ("Свернуть", ButtonKind::Secondary), ("Отключить и выйти", ButtonKind::Danger)],
                    ) {
                        Some(0) => keep = false,
                        Some(1) => {
                            keep = false;
                            ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
                        }
                        Some(_) => {
                            keep = false;
                            self.exit_after_remove = true;
                            self.link.remove(true);
                        }
                        None => {}
                    }
                }
                Modal::Error { title, text } => {
                    modal_title(ui, pal, Icon::Warning, pal.danger, title);
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| modal_text(ui, pal, text));
                    match modal_buttons(ui, pal, &[("Журнал", ButtonKind::Secondary), ("Понятно", ButtonKind::Primary)]) {
                        Some(0) => self.modal = Some(Modal::Log),
                        Some(_) => keep = false,
                        None => {}
                    }
                    if self.modal.is_some() {
                        keep = false;
                    }
                }
                Modal::Log => {
                    ui.set_width((ctx.content_rect().width() - 90.0).clamp(300.0, 620.0));
                    modal_title(ui, pal, Icon::Info, pal.accent, "Журнал");
                    // Файл журнала — вместе со строками фоновой части; раз в секунду.
                    if self.log_lines.as_ref().is_none_or(|(t, _)| t.elapsed() > Duration::from_secs(1)) {
                        self.log_lines = Some((Instant::now(), logger::tail()));
                        ctx.request_repaint_after(Duration::from_secs(1));
                    }
                    let lines = self.log_lines.as_ref().map(|(_, l)| l.clone()).unwrap_or_default();
                    egui::Frame::new().fill(pal.track).corner_radius(egui::CornerRadius::same(8)).inner_margin(Margin::same(10)).show(
                        ui,
                        |ui| {
                            egui::ScrollArea::vertical().max_height(320.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 3.0;
                                for l in &lines {
                                    ui.add(egui::Label::new(RichText::new(l).font(egui::FontId::monospace(11.5)).color(pal.weak)).wrap());
                                }
                            });
                        },
                    );
                    match modal_buttons(ui, pal, &[("Открыть файл", ButtonKind::Secondary), ("Закрыть", ButtonKind::Primary)]) {
                        Some(0) => {
                            if let Some(p) = logger::path() {
                                platform::open_path(&p);
                            }
                        }
                        Some(_) => keep = false,
                        None => {}
                    }
                }
            }
        });
        if resp.should_close() && !matches!(modal, Modal::ConfirmClose | Modal::Busy(_)) {
            keep = false;
        }
        if keep && self.modal.is_none() {
            self.modal = Some(modal);
        } else {
            // Ещё один кадр без окна: иначе egui до следующего события
            // считает его открытым и съедает первый щелчок.
            ctx.request_repaint();
        }
    }
}

const IMDISK_URL: &str = "https://sourceforge.net/projects/imdisk-toolkit/";

#[allow(clippy::too_many_arguments)]
fn size_slider(
    ui: &mut egui::Ui,
    pal: &Palette,
    title: &str,
    value: &mut f64,
    max: f64,
    color: Color32,
    enabled: bool,
    free: Option<u64>,
) {
    ui.horizontal(|ui| {
        w::legend(ui, pal, color, title);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let t = if enabled { format!("{} ГБ", gb_label(*value)) } else { "—".into() };
            ui.label(RichText::new(t).font(bold(15.0)).color(if enabled { pal.text } else { pal.faint }));
        });
    });
    w::slider(ui, pal, value, max.max(0.0), STEP_GB, color, enabled);
    let hint = if !enabled {
        "нет видеокарты".to_string()
    } else if max <= 0.0 {
        "памяти не хватает".to_string()
    } else {
        let mut h = format!("можно до {} ГБ", gb_label(max));
        if let Some(f) = free {
            h.push_str(&format!(" · сейчас свободно {}", fsize(f)));
        }
        h
    };
    ui.label(RichText::new(hint).font(font(12.0)).color(pal.faint));
}

fn gb_label(v: f64) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    let s = if s.contains('.') { s } else { format!("{v:.1}") };
    s.replace('.', ",")
}

/// Строка хранилища: данные, занятая память (видеопамять берётся кусками —
/// занято может быть чуть больше данных) и предел.
#[allow(clippy::too_many_arguments)]
fn part_row(ui: &mut egui::Ui, pal: &Palette, title: &str, sub: Option<&str>, used: u64, held: u64, limit: u64, color: Color32) {
    let held = held.max(used);
    ui.horizontal(|ui| {
        w::legend(ui, pal, color, title);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(format!("занято {} из {}", fsize(held), fsize(limit))).font(font(13.0)).color(pal.weak));
        });
    });
    // Вторая строка: видеокарта и сколько из занятого — сами данные
    // (видеопамять берётся кусками, поэтому занято может быть чуть больше).
    let mut note: Vec<String> = sub.map(|s| vec![s.to_string()]).unwrap_or_default();
    if fsize(held) != fsize(used) {
        note.push(format!("данные {}", fsize(used)));
    }
    if !note.is_empty() {
        ui.add(egui::Label::new(RichText::new(note.join(" · ")).font(font(12.5)).color(pal.faint)).truncate());
    }
    let total = limit.max(held).max(1) as f64;
    w::bar(
        ui,
        pal,
        8.0,
        &[(used as f64 / total, color), (held.saturating_sub(used) as f64 / total, color.gamma_multiply(0.35))],
    );
}

fn modal_title(ui: &mut egui::Ui, pal: &Palette, ic: Icon, color: Color32, title: &str) {
    ui.horizontal(|ui| {
        w::icon(ui, ic, 22.0, color);
        ui.label(RichText::new(title).font(bold(17.0)).color(pal.text));
    });
    ui.add_space(2.0);
}

fn modal_text(ui: &mut egui::Ui, pal: &Palette, text: &str) {
    ui.label(RichText::new(text).font(font(14.0)).color(pal.weak));
}

/// Кнопки справа налево; возвращает индекс нажатой.
fn modal_buttons(ui: &mut egui::Ui, pal: &Palette, buttons: &[(&str, ButtonKind)]) -> Option<usize> {
    ui.add_space(10.0);
    let mut clicked = None;
    // Строка ровно по высоте кнопок: иначе после прокручиваемого текста
    // egui центрирует их в лишнем свободном месте.
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 42.0), Layout::right_to_left(Align::Center), |ui| {
        for (i, (text, kind)) in buttons.iter().enumerate().rev() {
            let wdt = ui.painter().layout_no_wrap(text.to_string(), bold(14.5), pal.text).size().x + 36.0;
            if w::button(ui, pal, *kind, text, None, wdt.max(96.0), true).clicked() {
                clicked = Some(i);
            }
        }
    });
    clicked
}

impl eframe::App for VramApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let pal = theme::palette(&ctx);
        if !self.hooked {
            self.hooked = platform::hook_main_window();
        }
        let view = self.view();
        self.poll(&ctx, &view);
        self.handle_close(&ctx, &view);
        let view = self.view();

        egui::Panel::top("header")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(pal.bg).inner_margin(Margin { left: 20, right: 20, top: 16, bottom: 10 }))
            .show(ui, |ui| self.header(ui, &pal, &view));
        egui::Panel::bottom("footer")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(pal.bg).inner_margin(Margin { left: 20, right: 14, top: 2, bottom: 6 }))
            .show(ui, |ui| self.footer(ui, &pal));
        if matches!(view, View::Idle) {
            egui::Panel::bottom("actions")
                .show_separator_line(false)
                .frame(egui::Frame::new().fill(pal.bg).inner_margin(Margin { left: 20, right: 20, top: 10, bottom: 8 }))
                .show(ui, |ui| self.idle_actions(ui, &pal));
        }
        // Справа у прокрутки свой отступ: полоса прокрутки не ложится на карточки.
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(pal.bg).inner_margin(Margin { left: 20, right: 6, top: 2, bottom: 0 }))
            .show(ui, |ui| {
                let out = egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    egui::Frame::new().inner_margin(Margin { left: 0, right: 14, top: 0, bottom: 8 }).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 10.0;
                        match &view {
                            View::Idle => self.idle_view(ui, &pal),
                            View::Creating(c) => self.creating_view(ui, &pal, c),
                            View::Mounted(m) => self.mounted_view(ui, &pal, m),
                            View::Removing { letter, force } => self.removing_view(ui, &pal, *letter, *force),
                        }
                    });
                });
                // Ниже есть ещё содержимое — мягкое затухание у края.
                let r = out.inner_rect;
                if out.content_size.y - out.state.clamped_offset().y - r.height() > 1.0 {
                    let fade = egui::Rect::from_min_max(egui::pos2(r.left(), r.bottom() - 24.0), egui::pos2(r.right() - 14.0, r.bottom()));
                    w::fade_down(ui.painter(), fade, pal.bg);
                }
            });
        self.modals(&ctx, &pal, &view);
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn labels() {
        assert_eq!(super::gb_label(4.0), "4,0");
        assert_eq!(super::gb_label(4.25), "4,25");
        assert_eq!(super::gb_label(4.5), "4,5");
        assert_eq!(super::gb_to_bytes(0.25), 256 << 20);
    }
}
