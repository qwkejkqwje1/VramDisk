//! Видеокарты: список для интерфейса и подключение видеопамяти к диску.
//!
//! Основной путь — Vulkan: он есть у видеокарт AMD, NVIDIA и Intel последних
//! лет. Для видеокарт без Vulkan в Windows есть запасной путь — Direct3D 11;
//! он же используется, если Vulkan на этой видеокарте не запустился.

use crate::logger::log;
use crate::pool::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuKind {
    Discrete,
    Integrated,
    Virtual,
    Cpu,
    Other,
}

impl GpuKind {
    pub fn label(self) -> &'static str {
        match self {
            GpuKind::Discrete => "дискретная",
            GpuKind::Integrated => "встроенная",
            GpuKind::Virtual => "виртуальная",
            GpuKind::Cpu => "программная (процессор)",
            GpuKind::Other => "другая",
        }
    }
}

/// Через что работаем с видеокартой.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Vulkan,
    D3d11,
}

#[derive(Debug, Clone)]
pub struct GpuInfo {
    pub index: usize,
    pub name: String,
    /// Постоянный идентификатор (для настроек).
    pub uuid: String,
    pub kind: GpuKind,
    /// Размер видеопамяти.
    pub vram_total: u64,
    /// Сколько видеопамяти система готова отдать процессу.
    pub budget: Option<u64>,
    /// Сколько из неё уже занято этим процессом.
    pub usage: Option<u64>,
    /// Подпись для интерфейса: «Vulkan 1.3.287», «Direct3D 11».
    pub api: String,
    pub backend: Api,
    /// Есть отдельная очередь копирования (DMA).
    pub transfer_queue: bool,
    /// LUID адаптера Windows: по нему Vulkan-устройство сопоставляется с DXGI.
    pub luid: Option<u64>,
    /// Если Vulkan не запустится, можно через Direct3D 11.
    pub d3d_fallback: bool,
}

impl GpuInfo {
    /// Свободно сейчас: бюджет минус занятое, без бюджета — весь объём.
    pub fn available(&self) -> u64 {
        match (self.budget, self.usage) {
            (Some(b), Some(u)) => b.saturating_sub(u).min(self.vram_total),
            _ => self.vram_total,
        }
    }

    /// Сколько видеопамяти процесс может занять в лучшем случае.
    pub fn capacity(&self) -> u64 {
        self.budget.unwrap_or(self.vram_total).min(self.vram_total)
    }

    /// Видеопамять — это та же оперативная память (встроенная графика и т. п.).
    pub fn shares_ram(&self) -> bool {
        !matches!(self.kind, GpuKind::Discrete | GpuKind::Virtual)
    }
}

/// Лучшая видеокарта: сначала дискретная, затем по объёму памяти.
pub fn pick_best(list: &[GpuInfo]) -> Option<&GpuInfo> {
    list.iter().max_by_key(|g| (g.kind == GpuKind::Discrete, g.kind != GpuKind::Cpu, g.backend == Api::Vulkan, g.vram_total))
}

/// Склеивает списки Vulkan и DXGI: один адаптер — одна строка, Vulkan
/// предпочтительнее, видеокарты без Vulkan идут через Direct3D 11.
pub fn merge(vk: Vec<GpuInfo>, dx: Vec<GpuInfo>) -> Vec<GpuInfo> {
    let mut out = vk;
    for d in dx {
        match out.iter_mut().find(|v| v.backend == Api::Vulkan && v.luid.is_some() && v.luid == d.luid) {
            Some(v) => {
                v.d3d_fallback = true;
                if v.budget.is_none() {
                    v.budget = d.budget;
                    v.usage = d.usage;
                }
            }
            None => out.push(d),
        }
    }
    for (i, g) in out.iter_mut().enumerate() {
        g.index = i;
    }
    out
}

/// Опрос видеокарт для интерфейса.
pub struct GpuProbe {
    vk: Option<crate::vram::VkProbe>,
    #[cfg(windows)]
    dx: Option<crate::win::d3d11::DxgiProbe>,
}

impl GpuProbe {
    pub fn new() -> Result<Self, String> {
        let vk = crate::vram::VkProbe::new();
        if let Err(e) = &vk {
            log(format!("Vulkan: {e}"));
        }
        #[cfg(windows)]
        {
            let dx = crate::win::d3d11::DxgiProbe::new();
            if let Err(e) = &dx {
                log(format!("DXGI: {e}"));
            }
            match (vk, dx) {
                (Err(a), Err(_)) => Err(a),
                (vk, dx) => Ok(Self { vk: vk.ok(), dx: dx.ok() }),
            }
        }
        #[cfg(not(windows))]
        {
            Ok(Self { vk: Some(vk?) })
        }
    }

    pub fn list(&self) -> Vec<GpuInfo> {
        let vk = self.vk.as_ref().map(|p| p.list()).unwrap_or_default();
        #[cfg(windows)]
        let dx = self.dx.as_ref().map(|p| p.list()).unwrap_or_default();
        #[cfg(not(windows))]
        let dx = Vec::new();
        merge(vk, dx)
    }
}

pub struct OpenParams {
    pub gpu_uuid: Option<String>,
    /// Сколько адресов завести (байт): весь диск — на случай аварийного
    /// выхода за предел, когда оперативной памяти не хватило.
    pub capacity: u64,
    /// Предел пользователя, байт.
    pub limit: u64,
    pub chunk: u64,
    pub staging: u64,
    /// Сколько видеопамяти оставлять свободной для игр и рабочего стола.
    pub reserve: u64,
}

pub struct Opened {
    pub store: Box<dyn Store>,
    pub info: GpuInfo,
    /// Пояснение для пользователя (например, о запасном пути).
    pub note: Option<String>,
}

/// Открыть видеокарту (выбранную или лучшую) и подготовить хранилище.
pub fn open(p: &OpenParams) -> Result<Opened, String> {
    let list = GpuProbe::new()?.list();
    let target = p
        .gpu_uuid
        .as_ref()
        .and_then(|u| list.iter().find(|g| &g.uuid == u))
        .or_else(|| pick_best(&list))
        .cloned()
        .ok_or("подходящая видеокарта не найдена")?;
    match target.backend {
        Api::Vulkan => match crate::vram::VkStore::open(p, &target) {
            Ok(s) => Ok(Opened { info: s.info.clone(), store: Box::new(s), note: None }),
            Err(e) => {
                #[cfg(windows)]
                if target.d3d_fallback {
                    log(format!("Vulkan не запустился ({e}) — пробую Direct3D 11"));
                    let s = crate::win::d3d11::D3dStore::open(p, &target).map_err(|e2| format!("{e}; Direct3D 11: {e2}"))?;
                    return Ok(Opened {
                        info: s.info.clone(),
                        store: Box::new(s),
                        note: Some(format!("Vulkan на этой видеокарте не запустился ({e}), видеопамять подключена через Direct3D 11.")),
                    });
                }
                Err(e)
            }
        },
        Api::D3d11 => {
            #[cfg(windows)]
            {
                let s = crate::win::d3d11::D3dStore::open(p, &target)?;
                Ok(Opened { info: s.info.clone(), store: Box::new(s), note: None })
            }
            #[cfg(not(windows))]
            {
                Err("Direct3D 11 есть только в Windows".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(name: &str, backend: Api, luid: Option<u64>, kind: GpuKind, mem: u64) -> GpuInfo {
        GpuInfo {
            index: 0,
            name: name.into(),
            uuid: name.into(),
            kind,
            vram_total: mem,
            budget: None,
            usage: None,
            api: String::new(),
            backend,
            transfer_queue: false,
            luid,
            d3d_fallback: false,
        }
    }

    #[test]
    fn merge_by_luid() {
        let vk = vec![g("rx", Api::Vulkan, Some(7), GpuKind::Discrete, 8 << 30)];
        let mut dx_rx = g("rx-dx", Api::D3d11, Some(7), GpuKind::Discrete, 8 << 30);
        dx_rx.budget = Some(7 << 30);
        dx_rx.usage = Some(1 << 30);
        let dx = vec![dx_rx, g("hd4600", Api::D3d11, Some(9), GpuKind::Integrated, 1 << 30)];
        let m = merge(vk, dx);
        assert_eq!(m.len(), 2);
        assert!(m[0].d3d_fallback);
        assert_eq!(m[0].budget, Some(7 << 30), "бюджет взят из DXGI");
        assert_eq!(m[1].backend, Api::D3d11);
        assert_eq!(m[1].index, 1);
        assert_eq!(pick_best(&m).unwrap().name, "rx");
        // без Vulkan лучшая — единственная из DXGI
        let only = merge(Vec::new(), vec![g("old", Api::D3d11, Some(1), GpuKind::Discrete, 1 << 30)]);
        assert_eq!(pick_best(&only).unwrap().name, "old");
    }
}
