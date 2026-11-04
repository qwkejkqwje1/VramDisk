//! Фоновая часть VramDisk: процесс без окна, который держит диск.
//!
//! Видеопамять и сервер ImDisk принадлежат процессу, поэтому диск не может
//! жить совсем без программы. Но окну (интерфейс, OpenGL) жить вместе с
//! диском незачем: окно запускает `vramdisk.exe --serve <запрос>`, получает
//! от него состояние по именованному каналу и может быть закрыто — диск
//! работает дальше. Открытое снова окно подключается к той же фоновой части.
//! При входе в Windows (`--autostart`) фоновая часть создаёт диск сама,
//! без окна. Фоновая часть завершается сама после отключения диска.

use super::engine::{CreateRequest, Engine, Event, View};
use super::pipe::{self, PipeServer};
use super::platform;
use super::plan;
use super::proto::{self, ProcMem, Reply, Request, Status};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vramdisk::gpu::{self, GpuProbe};
use vramdisk::logger::log;
use vramdisk::settings::{MountRecord, Settings};
use vramdisk::win::{imdisk, sys};

/// Коды выхода фоновой части (их видит окно, если она не запустилась).
pub const EXIT_BAD_REQUEST: i32 = 2;
pub const EXIT_ALREADY_RUNNING: i32 = 3;
pub const EXIT_PIPE: i32 = 4;

/// Сколько хранить события для окна.
const KEEP_EVENTS: usize = 64;
/// Сколько ждать окно после отключения диска (чтобы оно узнало итог).
const LINGER: Duration = Duration::from_secs(3);
/// Сколько беда с диском должна продержаться, чтобы о ней сказать
/// (сервер на миг останавливается и при отключении диска извне).
const TROUBLE_AFTER: Duration = Duration::from_secs(3);

#[derive(Default)]
struct Shared {
    seq: u64,
    events: VecDeque<(u64, Event)>,
    /// Когда окно последний раз спрашивало состояние.
    asked: Option<Instant>,
    /// До какого события окно уже всё получило.
    delivered: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Запущена окном: о неудаче расскажет окно.
    Window,
    /// Автозапуск без окна: о неудаче — сообщением.
    Autostart,
}

/// `vramdisk.exe --serve <запрос JSON>`.
pub fn serve(json: &str) -> i32 {
    match serde_json::from_str::<CreateRequest>(json) {
        Ok(req) => run(req, Origin::Window),
        Err(e) => {
            log(format!("фоновая часть: неверный запрос ({e})"));
            EXIT_BAD_REQUEST
        }
    }
}

/// Почему автозапуск не создаёт диск сам.
enum Skip {
    /// Нужно внимание пользователя — открыть окно.
    Window(String),
    /// Уже сказано сообщением.
    Told,
}

/// `vramdisk.exe --autostart`: создать диск по настройкам, без окна.
/// Если нужно внимание пользователя (ImDisk не готов, остался зависший
/// диск), открывается окно — как при обычном запуске.
pub fn autostart() -> i32 {
    match autostart_request() {
        Ok(req) => run(req, Origin::Autostart),
        Err(Skip::Window(why)) => {
            log(format!("автозапуск: {why} — открываю окно"));
            if let Err(e) = sys::spawn_self(&[]) {
                log(format!("окно не запустилось: {e}"));
                sys::error_box("VramDisk", &format!("Диск при входе в Windows не создан: {why}."));
            }
            0
        }
        Err(Skip::Told) => 0,
    }
}

fn autostart_request() -> Result<CreateRequest, Skip> {
    let s = Settings::load();
    if let Err(p) = imdisk::check() {
        return Err(Skip::Window(format!("ImDisk не готов ({p})")));
    }
    if let Some(r) = MountRecord::load().or(s.mounted.clone()) {
        if !sys::process_is_vramdisk(r.pid) && platform::stale_exists(r.letter, r.device, r.pid) {
            return Err(Skip::Window(format!("остался зависший диск {}: от прошлого запуска", r.letter)));
        }
    }
    let gpus = if s.vram_mb > 0 {
        match GpuProbe::new() {
            Ok(p) => p.list(),
            Err(e) => {
                log(format!("видеокарты: {e}"));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let g = s.gpu_uuid.as_ref().and_then(|u| gpus.iter().find(|g| &g.uuid == u)).or_else(|| gpu::pick_best(&gpus));
    match plan::autostart_request(&s, sys::memory().0, g, &sys::free_letters()) {
        Ok((req, notes)) => {
            for n in notes {
                log(format!("автозапуск: {n}"));
            }
            Ok(req)
        }
        Err(e) => {
            log(format!("автозапуск: {e}"));
            sys::error_box("VramDisk", &format!("Диск при входе в Windows не создан.\n\n{e}"));
            Err(Skip::Told)
        }
    }
}

fn run(req: CreateRequest, origin: Origin) -> i32 {
    let Some(guard) = sys::server_instance(Duration::from_secs(8)) else {
        log("фоновая часть уже работает — вторая не нужна");
        return EXIT_ALREADY_RUNNING;
    };
    sys::no_power_throttling();
    let engine = Engine::new(Arc::new(|| {}));
    let shared = Arc::new(Mutex::new(Shared::default()));
    let pid = std::process::id();
    let pipe = {
        let (engine, shared) = (engine.clone(), shared.clone());
        PipeServer::start(&pipe::default_name(), move |msg| {
            let reply = match proto::decode::<Request>(msg) {
                Ok(Request::Status { after }) => {
                    let view = engine.view();
                    let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
                    let events = match after {
                        Some(a) => g.events.iter().filter(|(s, _)| *s > a).cloned().collect(),
                        None => Vec::new(),
                    };
                    g.asked = Some(Instant::now());
                    g.delivered = g.seq;
                    let mem = sys::process_memory().map(|(ws, private)| ProcMem { private_ws: ws, private });
                    Reply::Status(Box::new(Status {
                        pid,
                        version: vramdisk::VERSION.to_string(),
                        view,
                        last_seq: g.seq,
                        events,
                        mem,
                    }))
                }
                Ok(Request::Cancel) => {
                    engine.cancel();
                    Reply::Ok
                }
                Ok(Request::Remove { force }) => {
                    engine.remove(force);
                    Reply::Ok
                }
                Err(e) => Reply::Error(e),
            };
            proto::encode(&reply)
        })
    };
    let pipe = match pipe {
        Ok(p) => p,
        Err(e) => {
            log(format!("фоновая часть: {e}"));
            if origin == Origin::Autostart {
                sys::error_box("VramDisk", &format!("Диск при входе в Windows не создан: не удалось открыть канал связи.\n\n{e}"));
            }
            return EXIT_PIPE;
        }
    };
    log(format!("фоновая часть работает (процесс {pid})"));

    engine.create(req);
    let mut failure = None;
    let mut idle_since: Option<Instant> = None;
    let mut trouble_since: Option<Instant> = None;
    let mut told = false;
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let events = engine.events();
        let idle = engine.is_idle();
        let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
        for e in events {
            match &e {
                Event::Created { letter, device, notes } => {
                    for n in notes {
                        log(n);
                    }
                    let rec = MountRecord { letter: *letter, device: *device, pid };
                    if let Err(err) = rec.save() {
                        log(err);
                    }
                }
                Event::Removed => MountRecord::clear(),
                // Не создался (а не «не отключился»: тогда диск остаётся).
                Event::Failed { title, text } if idle => failure = Some((title.clone(), text.clone())),
                _ => {}
            }
            g.seq += 1;
            let seq = g.seq;
            g.events.push_back((seq, e));
            while g.events.len() > KEEP_EVENTS {
                g.events.pop_front();
            }
        }
        let (asked, all_delivered) = (g.asked, g.delivered >= g.seq);
        drop(g);
        engine.check_external();
        if !told {
            match trouble(&engine.view()) {
                Some(text) if trouble_since.get_or_insert_with(Instant::now).elapsed() >= TROUBLE_AFTER => {
                    told = true;
                    // Открытое окно показывает это само.
                    if asked.is_none_or(|a| a.elapsed() > Duration::from_secs(2)) {
                        log(format!("окно закрыто — сообщаю: {text}"));
                        tell(text);
                    }
                }
                Some(_) => {}
                None => trouble_since = None,
            }
        }
        if !engine.is_idle() {
            idle_since = None;
            continue;
        }
        // Диск отключён или не создался: дождаться, пока окно узнает итог
        // (или недолго — если окна нет), и выйти.
        let since = *idle_since.get_or_insert_with(Instant::now);
        let seen = all_delivered && asked.is_some_and(|a| a > since);
        if seen && since.elapsed() >= Duration::from_millis(300) || since.elapsed() >= LINGER {
            break;
        }
    }
    let asked = shared.lock().unwrap_or_else(|e| e.into_inner()).asked;
    pipe.stop();
    drop(guard);
    log("фоновая часть завершается: диска нет");
    if origin == Origin::Autostart {
        // Окно, если его открыли, само показало ошибку.
        let window_saw = asked.is_some_and(|a| a.elapsed() < Duration::from_secs(5));
        if let Some((title, text)) = failure.filter(|_| !window_saw) {
            let q = format!("{title} при входе в Windows.\n\n{text}\n\nОткрыть VramDisk?");
            if sys::ask_box("VramDisk", &q) {
                if let Err(e) = sys::spawn_self(&[]) {
                    log(format!("окно не запустилось: {e}"));
                }
            }
        }
    }
    0
}

/// Беда с подключённым диском, о которой надо знать, даже если окно закрыто.
fn trouble(view: &View) -> Option<String> {
    match view {
        View::Mounted(m) if m.stats.device_lost => Some(format!(
            "Диск {}: видеокарта была сброшена — драйвер перезапустился, данные в видеопамяти потеряны. \
             Скопируйте с диска то, что ещё читается, и отключите его.",
            m.letter
        )),
        View::Mounted(m) if !m.running => Some(format!(
            "Диск {}: перестал отвечать — сервер диска остановился. Отключите диск в окне VramDisk.",
            m.letter
        )),
        _ => None,
    }
}

/// Сообщение без окна — в своём потоке, чтобы диск обслуживался дальше.
fn tell(text: String) {
    let _ = std::thread::Builder::new().name("trouble-box".into()).spawn(move || {
        if sys::ask_box("VramDisk", &format!("{text}\n\nОткрыть VramDisk?")) {
            if let Err(e) = sys::spawn_self(&[]) {
                log(format!("окно не запустилось: {e}"));
            }
        }
    });
}
