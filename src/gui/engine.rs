//! Создание и отключение диска в фоновом потоке; интерфейс только читает
//! состояние (снимок `View`) и забирает события.
//!
//! Движок работает в фоновой части VramDisk (`--serve`), а окно получает
//! снимки по именованному каналу. В демо-режиме движок живёт прямо в окне.

use super::platform::{self, Unmount};
use super::widgets::StepMark;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use vramdisk::disk::{HybridDisk, Kind};
use vramdisk::gpu::{self, OpenParams};
use vramdisk::logger::log;
use vramdisk::pool::{self, SelfTest, SimStore, Store, CHUNK};
use vramdisk::ram::RamStore;
use vramdisk::settings::FsKind;
use vramdisk::stats::{Snapshot, Stats};
use vramdisk::util::size as fsize;

/// Как сообщить интерфейсу, что состояние изменилось.
pub type Notify = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateRequest {
    /// Размер диска: сумма пределов.
    pub size: u64,
    /// Предел оперативной памяти.
    pub ram: u64,
    /// Предел видеопамяти.
    pub vram: u64,
    /// Что занимать первым.
    pub first: Kind,
    pub letter: char,
    pub label: String,
    pub fs: FsKind,
    pub compress: bool,
    pub gpu_uuid: Option<String>,
    pub vram_reserve: u64,
    pub demo: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub title: String,
    pub mark: StepMark,
    pub detail: Option<String>,
}

struct Creating {
    pub letter: char,
    pub total: u64,
    pub steps: Vec<Step>,
    pub cancel: Arc<AtomicBool>,
    pub can_cancel: bool,
}

struct Mounted {
    letter: char,
    label: String,
    fs: FsKind,
    size: u64,
    ram: u64,
    vram: u64,
    first: Kind,
    gpu_name: Option<String>,
    /// Через что подключена видеопамять: «Vulkan 1.3.287», «Direct3D 11».
    gpu_api: Option<String>,
    test: Option<SelfTest>,
    notes: Vec<String>,
    stats: Arc<Stats>,
    device: Option<u32>,
    since: Instant,
    server: platform::Server,
}

// Состояние одно на всё приложение и почти не перемещается: упаковывать
// большой вариант в Box незачем.
#[allow(clippy::large_enum_variant)]
enum Phase {
    Idle,
    Creating(Creating),
    Mounted(Mounted),
    Removing { letter: char, force: bool },
}

/// Снимок состояния для интерфейса (и для передачи окну по каналу).
/// Делается несколько раз в секунду и сразу читается — упаковывать
/// большой вариант незачем.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum View {
    Idle,
    Creating(CreatingView),
    Mounted(MountedView),
    Removing { letter: char, force: bool },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreatingView {
    pub letter: char,
    pub total: u64,
    pub steps: Vec<Step>,
    pub can_cancel: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MountedView {
    pub letter: char,
    pub label: String,
    pub fs: FsKind,
    pub size: u64,
    pub ram: u64,
    pub vram: u64,
    pub first: Kind,
    pub gpu_name: Option<String>,
    pub gpu_api: Option<String>,
    pub test: Option<SelfTest>,
    pub notes: Vec<String>,
    pub device: Option<u32>,
    /// Сколько диск уже работает, мс.
    pub uptime_ms: u64,
    pub stats: Snapshot,
    /// Сервер ImDisk отвечает.
    pub running: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    Created { letter: char, device: Option<u32>, notes: Vec<String> },
    Failed { title: String, text: String },
    Busy { text: String },
    Removed,
    /// Фоновая часть пропала, пока держала диск (только в окне).
    Lost { letter: char },
}

struct Inner {
    phase: Phase,
    events: VecDeque<Event>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    notify: Notify,
}

const STEP_IMDISK: usize = 0;
const STEP_RAM: usize = 1;
const STEP_VRAM: usize = 2;
const STEP_TEST: usize = 3;
const STEP_MOUNT: usize = 4;

enum Fail {
    Cancelled,
    At(usize, String),
}

/// Шаги создания диска — до того, как они начались.
pub fn initial_steps(req: &CreateRequest) -> Vec<Step> {
    let t = |s: String| Step { title: s, mark: StepMark::Pending, detail: None };
    let mut steps = vec![
        t("Проверка ImDisk".into()),
        t(format!("Оперативная память · до {}", fsize(req.ram))),
        t(format!("Видеопамять · до {}", fsize(req.vram))),
        t("Проверка видеопамяти".into()),
        t(format!("Подключение и форматирование {}:", req.letter)),
    ];
    if req.ram == 0 {
        steps[STEP_RAM].title = if req.vram > 0 {
            "Оперативная память · только запас на случай, если видеопамять занята".into()
        } else {
            "Оперативная память не используется".into()
        };
    }
    if req.vram == 0 {
        steps[STEP_VRAM].mark = StepMark::Skipped;
        steps[STEP_VRAM].title = "Видеопамять не используется".into();
        steps[STEP_TEST].mark = StepMark::Skipped;
    }
    steps
}

/// Как выглядит создание диска, пока оно не началось.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn creating_view(req: &CreateRequest) -> View {
    View::Creating(CreatingView { letter: req.letter, total: req.size, steps: initial_steps(req), can_cancel: true })
}

impl Engine {
    pub fn new(notify: Notify) -> Self {
        Self { inner: Arc::new(Mutex::new(Inner { phase: Phase::Idle, events: VecDeque::new() })), notify }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn view(&self) -> View {
        match &self.lock().phase {
            Phase::Idle => View::Idle,
            Phase::Creating(c) => View::Creating(CreatingView {
                letter: c.letter,
                total: c.total,
                steps: c.steps.clone(),
                can_cancel: c.can_cancel,
            }),
            Phase::Mounted(m) => View::Mounted(MountedView {
                letter: m.letter,
                label: m.label.clone(),
                fs: m.fs,
                size: m.size,
                ram: m.ram,
                vram: m.vram,
                first: m.first,
                gpu_name: m.gpu_name.clone(),
                gpu_api: m.gpu_api.clone(),
                test: m.test,
                notes: m.notes.clone(),
                device: m.device,
                uptime_ms: m.since.elapsed().as_millis() as u64,
                stats: m.stats.snapshot(),
                running: m.server.running(),
            }),
            Phase::Removing { letter, force } => View::Removing { letter: *letter, force: *force },
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn is_idle(&self) -> bool {
        matches!(self.lock().phase, Phase::Idle)
    }

    pub fn events(&self) -> Vec<Event> {
        self.lock().events.drain(..).collect()
    }

    /// Диск сняли другой программой (например, ImDisk): сервер получил
    /// CLOSE, а буквы больше нет — забыть о диске.
    pub fn check_external(&self) {
        let gone = match &self.lock().phase {
            Phase::Mounted(m) => {
                let stopped = m.stats.server_stopped.load(SeqCst);
                stopped.then_some(m.letter)
            }
            _ => None,
        };
        if gone.is_some_and(|l| !platform::letter_used(l)) {
            self.forget();
        }
    }

    fn step(&self, i: usize, mark: StepMark, detail: Option<String>) {
        if let Phase::Creating(c) = &mut self.lock().phase {
            if let Some(s) = c.steps.get_mut(i) {
                s.mark = mark;
                if detail.is_some() {
                    s.detail = detail;
                }
            }
        }
        (self.notify)();
    }

    pub fn create(&self, req: CreateRequest) {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut g = self.lock();
            if !matches!(g.phase, Phase::Idle) {
                return;
            }
            g.phase = Phase::Creating(Creating {
                letter: req.letter,
                total: req.size,
                steps: initial_steps(&req),
                cancel: cancel.clone(),
                can_cancel: true,
            });
        }
        log(format!(
            "создание диска {}: {} (ОЗУ до {}, видеопамять до {}, сначала {}), {}{}",
            req.letter,
            fsize(req.size),
            fsize(req.ram),
            fsize(req.vram),
            req.first.label(),
            req.fs.label(),
            if req.compress { ", сжатие" } else { "" }
        ));
        let me = self.clone();
        std::thread::Builder::new()
            .name("create-disk".into())
            .spawn(move || {
                let r = me.run_create(&req, &cancel);
                let mut armed = None;
                {
                    let mut g = me.lock();
                    match r {
                        Ok(m) => {
                            let (letter, device, notes) = (m.letter, m.device, m.notes.clone());
                            g.phase = Phase::Mounted(m);
                            g.events.push_back(Event::Created { letter, device, notes });
                            armed = Some(letter);
                        }
                        Err(Fail::Cancelled) => {
                            log("создание диска отменено");
                            g.phase = Phase::Idle;
                        }
                        Err(Fail::At(i, text)) => {
                            log(format!("создание диска не удалось на шаге {i}: {text}"));
                            g.phase = Phase::Idle;
                            g.events.push_back(Event::Failed { title: "Не удалось создать диск".into(), text });
                        }
                    }
                }
                if armed.is_some() {
                    platform::arm_cleanup(armed);
                }
                (me.notify)();
            })
            .expect("поток создания диска");
    }

    pub fn cancel(&self) {
        if let Phase::Creating(c) = &self.lock().phase {
            if c.can_cancel {
                c.cancel.store(true, SeqCst);
            }
        }
    }

    fn run_create(&self, req: &CreateRequest, cancel: &AtomicBool) -> Result<Mounted, Fail> {
        let cancelled = || cancel.load(SeqCst);
        // 1. ImDisk
        self.step(STEP_IMDISK, StepMark::Running, None);
        if let Some(p) = platform::imdisk_problem() {
            self.step(STEP_IMDISK, StepMark::Failed, None);
            return Err(Fail::At(STEP_IMDISK, p));
        }
        self.step(STEP_IMDISK, StepMark::Done, Some(if req.demo { "демо-режим".into() } else { "готов".into() }));
        if cancelled() {
            return Err(Fail::Cancelled);
        }

        let stats = Arc::new(Stats::default());
        let mut disk = HybridDisk::new(req.size, stats.clone());

        // 2. Оперативная память: заводятся только адреса, память занимается
        // по мере записи. Нужна и диску «только из видеопамяти» — как запас,
        // если видеопамять займёт игра.
        self.step(STEP_RAM, StepMark::Running, None);
        let ram: Box<dyn Store> = if req.demo {
            let mut s = SimStore::new(Kind::Ram, req.size, req.ram, 4 << 20);
            s.phantom = true;
            s.per_block = true;
            Box::new(s)
        } else {
            match RamStore::new(req.size, req.ram) {
                Ok(r) => Box::new(r),
                Err(e) => {
                    self.step(STEP_RAM, StepMark::Failed, None);
                    return Err(Fail::At(STEP_RAM, e));
                }
            }
        };
        disk.add(ram);
        self.step(STEP_RAM, StepMark::Done, Some("по мере записи".into()));
        if cancelled() {
            return Err(Fail::Cancelled);
        }

        // 3–4. Видеопамять и её проверка
        let mut gpu_name = None;
        let mut gpu_api = None;
        let mut test = None;
        let mut notes = Vec::new();
        if req.vram > 0 {
            self.step(STEP_VRAM, StepMark::Running, None);
            let t0 = Instant::now();
            let mut store: Box<dyn Store> = if req.demo {
                std::thread::sleep(Duration::from_millis(400));
                let mut s = SimStore::new(Kind::Vram, req.size, req.vram, CHUNK);
                s.phantom = true;
                s.alloc_delay = Duration::from_millis(2);
                gpu_name = Some("AMD Radeon RX 5700 XT".to_string());
                gpu_api = Some("Vulkan 1.3.287".to_string());
                Box::new(s)
            } else {
                let params = OpenParams {
                    gpu_uuid: req.gpu_uuid.clone(),
                    capacity: req.size,
                    limit: req.vram,
                    chunk: CHUNK,
                    staging: platform_staging(),
                    reserve: req.vram_reserve,
                };
                match gpu::open(&params) {
                    Ok(o) => {
                        log(format!(
                            "видеопамять: «{}» через {} за {} мс{}",
                            o.info.name,
                            o.info.api,
                            t0.elapsed().as_millis(),
                            if o.info.transfer_queue { ", отдельная очередь DMA" } else { "" }
                        ));
                        if let Some(n) = o.note {
                            log(&n);
                            notes.push(n);
                        }
                        if o.info.shares_ram() {
                            notes.push(format!(
                                "«{}» — {} видеокарта: её видеопамять — это та же оперативная память.",
                                o.info.name,
                                o.info.kind.label()
                            ));
                        }
                        gpu_name = Some(o.info.name.clone());
                        gpu_api = Some(o.info.api.clone());
                        o.store
                    }
                    Err(e) => {
                        self.step(STEP_VRAM, StepMark::Failed, None);
                        return Err(Fail::At(STEP_VRAM, format!("Видеопамять: {e}")));
                    }
                }
            };
            self.step(STEP_VRAM, StepMark::Done, Some(gpu_api.clone().unwrap_or_default()));
            if cancelled() {
                return Err(Fail::Cancelled);
            }
            self.step(STEP_TEST, StepMark::Running, None);
            let r = if req.demo {
                std::thread::sleep(Duration::from_millis(600));
                Ok(SelfTest { write_bps: 2.3 * 1073741824.0, read_bps: 2.0 * 1073741824.0, checked_bytes: CHUNK, ms: 650 })
            } else {
                pool::self_test(store.as_mut(), CHUNK)
            };
            match r {
                Ok(t) => {
                    log(format!(
                        "проверка видеопамяти: {} проверено, запись {}, чтение {}, {} мс",
                        fsize(t.checked_bytes),
                        vramdisk::util::speed(t.write_bps),
                        vramdisk::util::speed(t.read_bps),
                        t.ms
                    ));
                    self.step(STEP_TEST, StepMark::Done, Some(test_line(&t)));
                    test = Some(t);
                }
                Err(e) => {
                    self.step(STEP_TEST, StepMark::Failed, None);
                    return Err(Fail::At(STEP_TEST, e));
                }
            }
            disk.add(store);
        }
        disk.set_first(req.first);
        if cancelled() {
            return Err(Fail::Cancelled);
        }

        // 5. Подключение
        if let Phase::Creating(c) = &mut self.lock().phase {
            c.can_cancel = false;
        }
        self.step(STEP_MOUNT, StepMark::Running, None);
        let size = disk.size();
        let s = stats.snapshot();
        let (ram, vram) = (s.max_ram, s.max_vram);
        match platform::mount(disk, req) {
            Ok(ok) => {
                self.step(STEP_MOUNT, StepMark::Done, Some(req.fs.label().into()));
                notes.extend(ok.notes);
                Ok(Mounted {
                    letter: req.letter,
                    label: req.label.clone(),
                    fs: req.fs,
                    size,
                    ram,
                    vram,
                    first: req.first,
                    gpu_name,
                    gpu_api,
                    test,
                    notes,
                    stats,
                    device: ok.device,
                    since: Instant::now(),
                    server: ok.server,
                })
            }
            Err(e) => {
                self.step(STEP_MOUNT, StepMark::Failed, None);
                Err(Fail::At(STEP_MOUNT, e))
            }
        }
    }

    /// Отключить диск. `force` — даже если на нём открыты файлы.
    pub fn remove(&self, force: bool) {
        let m = {
            let mut g = self.lock();
            let letter = match &g.phase {
                Phase::Mounted(m) => m.letter,
                _ => return,
            };
            match std::mem::replace(&mut g.phase, Phase::Removing { letter, force }) {
                Phase::Mounted(m) => m,
                _ => unreachable!(),
            }
        };
        log(format!("отключение диска {}:{}", m.letter, if force { " (принудительно)" } else { "" }));
        (self.notify)();
        let me = self.clone();
        std::thread::Builder::new()
            .name("remove-disk".into())
            .spawn(move || {
                let mut m = m;
                let r = platform::unmount(m.letter, &mut m.server, force);
                let mut removed = None;
                {
                    let mut g = me.lock();
                    match r {
                        Ok(()) => {
                            removed = Some(m.letter);
                            drop(m);
                            g.phase = Phase::Idle;
                            g.events.push_back(Event::Removed);
                        }
                        Err(Unmount::Busy(t)) => {
                            g.phase = Phase::Mounted(m);
                            g.events.push_back(Event::Busy { text: t });
                        }
                        Err(Unmount::Failed(t)) => {
                            g.phase = Phase::Mounted(m);
                            g.events.push_back(Event::Failed { title: "Не удалось отключить диск".into(), text: t });
                        }
                    }
                }
                if let Some(letter) = removed {
                    platform::arm_cleanup(None);
                    log(format!("диск {letter}: отключён"));
                }
                (me.notify)();
            })
            .expect("поток отключения диска");
    }

    /// Диск исчез без нас (снят другой программой): забыть о нём.
    fn forget(&self) {
        let mut g = self.lock();
        if let Phase::Mounted(m) = &g.phase {
            log(format!("диск {}: снят извне", m.letter));
            g.phase = Phase::Idle;
            g.events.push_back(Event::Removed);
            drop(g);
            platform::arm_cleanup(None);
            (self.notify)();
        }
    }
}

fn platform_staging() -> u64 {
    #[cfg(windows)]
    {
        platform::SHM_BUFFER as u64
    }
    #[cfg(not(windows))]
    {
        8 << 20
    }
}

pub fn test_line(t: &SelfTest) -> String {
    format!("запись {} · чтение {}", vramdisk::util::speed(t.write_bps), vramdisk::util::speed(t.read_bps))
}
