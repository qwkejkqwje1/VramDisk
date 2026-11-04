//! Обмен окна с фоновой частью: запрос — ответ в JSON по именованному каналу.

use super::engine::{Event, View};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// Состояние и события с номерами больше `after`; `None` — без событий,
    /// только номер последнего (окно подключается к уже работающему диску).
    Status { after: Option<u64> },
    Cancel,
    Remove { force: bool },
}

/// Сколько оперативной памяти занимает фоновая часть.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProcMem {
    /// Частный рабочий набор — это число показывает Диспетчер задач.
    pub private_ws: Option<u64>,
    /// Выделено процессу всего (вместе с выгруженным в файл подкачки).
    pub private: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    pub version: String,
    pub view: View,
    pub last_seq: u64,
    pub events: Vec<(u64, Event)>,
    pub mem: Option<ProcMem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Reply {
    Status(Box<Status>),
    Ok,
    Error(String),
}

pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

pub fn decode<T: DeserializeOwned>(b: &[u8]) -> Result<T, String> {
    serde_json::from_slice(b).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::engine::{initial_steps, CreateRequest, CreatingView, MountedView};
    use super::*;
    use vramdisk::disk::Kind;
    use vramdisk::pool::SelfTest;
    use vramdisk::settings::FsKind;
    use vramdisk::stats::Snapshot;

    fn req() -> CreateRequest {
        CreateRequest {
            size: 6 << 30,
            ram: 2 << 30,
            vram: 4 << 30,
            first: Kind::Vram,
            letter: 'V',
            label: "Диск \"для\" игр".into(),
            fs: FsKind::ExFat,
            compress: false,
            gpu_uuid: Some("abc".into()),
            vram_reserve: 1 << 30,
            demo: false,
        }
    }

    #[test]
    fn roundtrip() {
        let r = req();
        assert_eq!(decode::<CreateRequest>(&encode(&r)).unwrap(), r);
        for q in [Request::Status { after: None }, Request::Status { after: Some(7) }, Request::Cancel, Request::Remove { force: true }] {
            assert_eq!(decode::<Request>(&encode(&q)).unwrap(), q);
        }
        let creating = View::Creating(CreatingView { letter: 'V', total: r.size, steps: initial_steps(&r), can_cancel: true });
        let mounted = View::Mounted(MountedView {
            letter: 'V',
            label: "VramDisk".into(),
            fs: FsKind::Ntfs,
            size: 6 << 30,
            ram: 2 << 30,
            vram: 4 << 30,
            first: Kind::Ram,
            gpu_name: Some("AMD Radeon RX 5700 XT".into()),
            gpu_api: Some("Vulkan 1.3".into()),
            test: Some(SelfTest { write_bps: 2.5e9, read_bps: 2.1e9, checked_bytes: 64 << 20, ms: 650 }),
            notes: vec!["заметка".into()],
            device: Some(3),
            uptime_ms: 123_456,
            stats: Snapshot { used_ram: 5, held_vram: 64 << 20, device_lost: true, ..Default::default() },
            running: true,
        });
        for view in [View::Idle, creating, mounted, View::Removing { letter: 'V', force: false }] {
            let st = Reply::Status(Box::new(Status {
                pid: 42,
                version: "2.1.0".into(),
                view,
                last_seq: 3,
                events: vec![
                    (1, Event::Created { letter: 'V', device: Some(3), notes: vec![] }),
                    (2, Event::Busy { text: "занят".into() }),
                    (3, Event::Failed { title: "t".into(), text: "x".into() }),
                ],
                mem: Some(ProcMem { private_ws: Some(40 << 20), private: 60 << 20 }),
            }));
            assert_eq!(decode::<Reply>(&encode(&st)).unwrap(), st);
        }
        assert!(decode::<Request>(b"null").is_err());
    }
}
