//! Окно ↔ фоновая часть. Окно запускает фоновую часть (`--serve`), опрашивает
//! её по каналу и передаёт ей команды. Опрос идёт в своём потоке: окно
//! никогда не ждёт канал. Если фоновая часть уже работает (диск создан
//! раньше, окно открыли снова), окно просто подключается к ней.

use super::engine::{creating_view, CreateRequest, Event, Notify, View};
use super::pipe;
use super::proto::{self, ProcMem, Reply, Request, Status};
use super::server::EXIT_ALREADY_RUNNING;
use std::collections::VecDeque;
use std::process::Child;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use vramdisk::logger::log;
use vramdisk::win::sys;

/// Как часто спрашивать состояние, когда фоновая часть есть.
const POLL: Duration = Duration::from_millis(250);
/// Как часто проверять, не появилась ли фоновая часть.
const PROBE: Duration = Duration::from_secs(1);
/// Сколько ждать ответа только что запущенной фоновой части (она может
/// ждать, пока завершится предыдущая).
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Молчит дольше — показываем, что диск не отвечает.
const STALL: Duration = Duration::from_secs(3);

struct Launch {
    child: Child,
    at: Instant,
}

struct Attached {
    pid: u32,
    /// До какого события всё получено.
    seq: u64,
    last_ok: Instant,
    mem: Option<ProcMem>,
}

struct St {
    view: View,
    events: VecDeque<Event>,
    launch: Option<Launch>,
    attached: Option<Attached>,
    commands: VecDeque<Request>,
    stalled: bool,
    bad_reply: bool,
}

#[derive(Clone)]
pub struct Remote {
    st: Arc<Mutex<St>>,
    notify: Notify,
    name: Arc<String>,
}

impl Remote {
    pub fn new(notify: Notify) -> Self {
        let st = St {
            view: View::Idle,
            events: VecDeque::new(),
            launch: None,
            attached: None,
            commands: VecDeque::new(),
            stalled: false,
            bad_reply: false,
        };
        let me = Self { st: Arc::new(Mutex::new(st)), notify, name: Arc::new(pipe::default_name()) };
        // Первый опрос — сразу: окно открывается уже с работающим диском.
        me.poll_once(500);
        let m2 = me.clone();
        let _ = std::thread::Builder::new().name("remote-poll".into()).spawn(move || loop {
            let next = m2.poll_once(1000);
            std::thread::sleep(next);
        });
        me
    }

    fn lock(&self) -> MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn view(&self) -> View {
        let g = self.lock();
        let mut v = g.view.clone();
        if let (true, View::Mounted(m)) = (g.stalled, &mut v) {
            m.running = false;
        }
        v
    }

    pub fn events(&self) -> Vec<Event> {
        self.lock().events.drain(..).collect()
    }

    /// Сколько памяти занимает фоновая часть.
    pub fn mem(&self) -> Option<ProcMem> {
        self.lock().attached.as_ref().and_then(|a| a.mem)
    }

    pub fn create(&self, req: CreateRequest) {
        let mut g = self.lock();
        if g.launch.is_some() || !matches!(g.view, View::Idle) {
            return;
        }
        let json = serde_json::to_string(&req).unwrap_or_default();
        match sys::spawn_self(&["--serve", &json]) {
            Ok(child) => {
                log(format!("запущена фоновая часть (процесс {}) для диска {}:", child.id(), req.letter));
                g.launch = Some(Launch { child, at: Instant::now() });
                // Прежняя фоновая часть могла ещё не завершиться — ответы
                // принимаем только от новой.
                g.attached = None;
                g.stalled = false;
                g.commands.clear();
                g.view = creating_view(&req);
            }
            Err(e) => {
                log(format!("фоновая часть не запустилась: {e}"));
                g.events.push_back(Event::Failed {
                    title: "Не удалось создать диск".into(),
                    text: format!("Не удалось запустить фоновую часть VramDisk: {e}"),
                });
            }
        }
        drop(g);
        (self.notify)();
    }

    pub fn cancel(&self) {
        let mut g = self.lock();
        if matches!(g.view, View::Creating(_)) {
            g.commands.push_back(Request::Cancel);
        }
    }

    pub fn remove(&self, force: bool) {
        let mut g = self.lock();
        if let View::Mounted(m) = &g.view {
            let letter = m.letter;
            g.view = View::Removing { letter, force };
            g.commands.push_back(Request::Remove { force });
        }
        drop(g);
        (self.notify)();
    }

    /// Один обмен с фоновой частью; возвращает, когда обменяться снова.
    fn poll_once(&self, timeout_ms: u32) -> Duration {
        let req = {
            let mut g = self.lock();
            let cmd = if g.attached.is_some() { g.commands.pop_front() } else { None };
            cmd.unwrap_or_else(|| Request::Status {
                // Свою, только что запущенную, — с самого начала.
                after: if g.launch.is_some() { Some(0) } else { g.attached.as_ref().map(|a| a.seq) },
            })
        };
        let command = !matches!(req, Request::Status { .. });
        let r = pipe::call(&self.name, &proto::encode(&req), timeout_ms);
        let mut g = self.lock();
        let changed = match r.map(|b| proto::decode::<Reply>(&b)) {
            Ok(Ok(Reply::Status(s))) => apply(&mut g, *s),
            Ok(Ok(Reply::Ok)) => false,
            Ok(Ok(Reply::Error(e))) => {
                log(format!("фоновая часть не приняла запрос: {e}"));
                false
            }
            Ok(Err(e)) => {
                if !g.bad_reply {
                    g.bad_reply = true;
                    log(format!("фоновая часть ответила непонятно (другая версия VramDisk?): {e}"));
                }
                false
            }
            Err(_) => unreachable_server(&mut g),
        };
        let active = g.attached.is_some() || g.launch.is_some();
        drop(g);
        if changed {
            (self.notify)();
        }
        if command {
            Duration::ZERO
        } else if active {
            POLL
        } else {
            PROBE
        }
    }
}

/// Ответ о состоянии. `true` — что-то изменилось.
fn apply(g: &mut St, s: Status) -> bool {
    let ours = match &g.launch {
        Some(l) if l.child.id() != s.pid => return false,
        Some(_) => {
            g.launch = None;
            true
        }
        None => false,
    };
    let fresh = g.attached.as_ref().is_none_or(|a| a.pid != s.pid);
    if fresh {
        log(format!("окно подключено к фоновой части (процесс {}, версия {})", s.pid, s.version));
        // К чужой (уже работавшей) — без старых событий.
        let seq = if ours { 0 } else { s.last_seq };
        g.attached = Some(Attached { pid: s.pid, seq, last_ok: Instant::now(), mem: None });
    }
    let a = g.attached.as_mut().expect("подключено");
    let new: Vec<Event> = s.events.into_iter().filter(|(n, _)| *n > a.seq).map(|(_, e)| e).collect();
    a.seq = a.seq.max(s.last_seq);
    a.last_ok = Instant::now();
    a.mem = s.mem;
    let changed = fresh || g.stalled || !new.is_empty() || g.view != s.view;
    g.stalled = false;
    g.events.extend(new);
    g.view = s.view;
    changed
}

/// Канал не ответил. `true` — что-то изменилось.
fn unreachable_server(g: &mut St) -> bool {
    if let Some(l) = &mut g.launch {
        let failed = match l.child.try_wait() {
            Ok(Some(st)) if st.code() == Some(EXIT_ALREADY_RUNNING) => {
                log("фоновая часть уже работает — подключаюсь к ней");
                None
            }
            Ok(Some(st)) => Some(format!(
                "Фоновая часть VramDisk завершилась, не начав работу (код {}). Подробности — в журнале.",
                st.code().map_or("?".to_string(), |c| c.to_string())
            )),
            Ok(None) if l.at.elapsed() > LAUNCH_TIMEOUT => {
                let _ = l.child.kill();
                Some("Фоновая часть VramDisk не ответила за 20 секунд и остановлена. Подробности — в журнале.".to_string())
            }
            _ => return false,
        };
        g.launch = None;
        g.commands.clear();
        g.view = View::Idle;
        if let Some(text) = failed {
            log(&text);
            g.events.push_back(Event::Failed { title: "Не удалось создать диск".into(), text });
        }
        return true;
    }
    let Some(a) = &g.attached else { return false };
    if sys::process_is_vramdisk(a.pid) {
        // Жива, но молчит.
        if a.last_ok.elapsed() > STALL && !g.stalled {
            log("фоновая часть не отвечает");
            g.stalled = true;
            return true;
        }
        return false;
    }
    let pid = a.pid;
    g.attached = None;
    g.stalled = false;
    g.commands.clear();
    match std::mem::replace(&mut g.view, View::Idle) {
        // Обычное завершение после отключения диска.
        View::Idle => {}
        View::Creating(_) => {
            log(format!("фоновая часть (процесс {pid}) завершилась во время создания диска"));
            g.events.push_back(Event::Failed {
                title: "Не удалось создать диск".into(),
                text: "Фоновая часть VramDisk неожиданно завершилась во время создания диска. Подробности — в журнале.".into(),
            });
        }
        View::Removing { letter, .. } if !sys::letter_used(letter) => g.events.push_back(Event::Removed),
        View::Mounted(super::engine::MountedView { letter, .. }) | View::Removing { letter, .. } => {
            log(format!("фоновая часть (процесс {pid}) пропала — диск {letter}: остался без обслуживания"));
            g.events.push_back(Event::Lost { letter });
        }
    }
    true
}
