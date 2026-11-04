//! Запасной путь к видеопамяти — Direct3D 11.
//!
//! Нужен для видеокарт без Vulkan (старые модели, виртуальные машины) и на
//! случай, если Vulkan на видеокарте не запустился. Устройство то же, что и
//! в `vram.rs`: видеопамять берётся кусками по мере записи, данные идут через
//! два промежуточных буфера (D3D11_USAGE_STAGING) и копируются видеокартой.
//! Порядок команд Direct3D 11 соблюдает сам: чтение после записи всегда
//! видит записанное.

use crate::disk::DiskError;
use crate::gpu::{Api, GpuInfo, GpuKind, OpenParams};
use crate::pool::{ChunkAlloc, Kind, NoRoom, Slot, Store, BLOCK};
use std::time::{Duration, Instant};
use windows::core::{Interface, BOOL};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;

const ZERO_BYTES: u64 = BLOCK;
const WAIT: Duration = Duration::from_secs(10);
const REFUSAL_BACKOFF: Duration = Duration::from_secs(1);

const LEVELS: [D3D_FEATURE_LEVEL; 4] =
    [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];

fn hr(what: &str, e: &windows::core::Error) -> String {
    let m = e.message();
    let m = m.trim();
    if m.is_empty() {
        format!("{what}: 0x{:08X}", e.code().0 as u32)
    } else {
        format!("{what}: {m} (0x{:08X})", e.code().0 as u32)
    }
}

fn removed(e: &windows::core::Error) -> bool {
    [DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_DRIVER_INTERNAL_ERROR]
        .contains(&e.code())
}

fn level_name(fl: D3D_FEATURE_LEVEL) -> &'static str {
    match fl {
        D3D_FEATURE_LEVEL_11_1 => "11_1",
        D3D_FEATURE_LEVEL_11_0 => "11_0",
        D3D_FEATURE_LEVEL_10_1 => "10_1",
        _ => "10_0",
    }
}

/// Устройство Direct3D 11 на адаптере.
fn create_device(a: &IDXGIAdapter1) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext, D3D_FEATURE_LEVEL)> {
    let mut dev = None;
    let mut ctx = None;
    let mut fl = D3D_FEATURE_LEVEL_10_0;
    let mut r = Err(windows::core::Error::empty());
    // Windows 7 без обновления не знает уровня 11_1 и отвечает E_INVALIDARG.
    for levels in [&LEVELS[..], &LEVELS[1..]] {
        r = unsafe {
            D3D11CreateDevice(
                a,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(levels),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                Some(&mut fl),
                Some(&mut ctx),
            )
        };
        if r.is_ok() {
            break;
        }
    }
    r?;
    match (dev, ctx) {
        (Some(d), Some(c)) => Ok((d, c, fl)),
        _ => Err(windows::core::Error::empty()),
    }
}

fn wide_name(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n]).trim().to_string()
}

fn luid_of(d: &DXGI_ADAPTER_DESC1) -> u64 {
    ((d.AdapterLuid.HighPart as u32 as u64) << 32) | d.AdapterLuid.LowPart as u64
}

/// Бюджет и занятость видеопамяти этим процессом (Windows 10 и новее).
fn budget_of(a: &IDXGIAdapter1) -> (Option<u64>, Option<u64>) {
    let Ok(a3) = a.cast::<IDXGIAdapter3>() else { return (None, None) };
    let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    match unsafe { a3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info) } {
        Ok(()) if info.Budget > 0 => (Some(info.Budget), Some(info.CurrentUsage)),
        _ => (None, None),
    }
}

/// Описание адаптера. Создаёт на нём устройство: так заодно проверяется, что
/// Direct3D 11 на нём работает, и узнаётся, общая ли у него память с
/// процессором (встроенная графика).
fn describe(a: &IDXGIAdapter1, index: usize) -> Option<GpuInfo> {
    let d = unsafe { a.GetDesc1() }.ok()?;
    // Программные адаптеры и «Базовый видеоадаптер (Майкрософт)».
    if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 || d.VendorId == 0x1414 {
        return None;
    }
    let (dev, _ctx, fl) = create_device(a).ok()?;
    let mut o2 = D3D11_FEATURE_DATA_D3D11_OPTIONS2::default();
    let uma = match unsafe {
        dev.CheckFeatureSupport(
            D3D11_FEATURE_D3D11_OPTIONS2,
            &mut o2 as *mut _ as *mut _,
            std::mem::size_of::<D3D11_FEATURE_DATA_D3D11_OPTIONS2>() as u32,
        )
    } {
        Ok(()) => o2.UnifiedMemoryArchitecture.as_bool(),
        // Старая система: встроенная графика почти без своей памяти.
        Err(_) => (d.DedicatedVideoMemory as u64) < (512 << 20),
    };
    let virt = matches!(d.VendorId, 0x15AD | 0x80EE | 0x1AB8 | 0x1AF4);
    let kind = if virt {
        GpuKind::Virtual
    } else if uma {
        GpuKind::Integrated
    } else {
        GpuKind::Discrete
    };
    let vram_total = if uma {
        d.DedicatedVideoMemory as u64 + d.SharedSystemMemory as u64
    } else {
        d.DedicatedVideoMemory as u64
    };
    if vram_total == 0 {
        return None;
    }
    let (budget, usage) = budget_of(a);
    Some(GpuInfo {
        index,
        name: wide_name(&d.Description),
        uuid: format!("dx-{:04x}{:04x}{:08x}{:02x}", d.VendorId, d.DeviceId, d.SubSysId, d.Revision),
        kind,
        vram_total,
        budget,
        usage,
        api: if fl.0 >= D3D_FEATURE_LEVEL_11_0.0 {
            "Direct3D 11".into()
        } else {
            format!("Direct3D 11 (уровень {})", level_name(fl))
        },
        backend: Api::D3d11,
        transfer_queue: false,
        luid: Some(luid_of(&d)),
        d3d_fallback: false,
    })
}

/// Опрос адаптеров DXGI: сведения собираются один раз, бюджет — при каждом
/// обновлении списка.
pub struct DxgiProbe {
    adapters: Vec<(IDXGIAdapter1, GpuInfo)>,
}

// Адаптеры только читаются, из одного потока за раз.
unsafe impl Send for DxgiProbe {}

impl DxgiProbe {
    pub fn new() -> Result<Self, String> {
        let f: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| hr("DXGI недоступен", &e))?;
        let mut adapters: Vec<(IDXGIAdapter1, GpuInfo)> = Vec::new();
        for i in 0..16 {
            let Ok(a) = (unsafe { f.EnumAdapters1(i) }) else { break };
            if let Some(mut g) = describe(&a, adapters.len()) {
                // Две одинаковые видеокарты различаются номером.
                let same = adapters.iter().filter(|(_, x)| x.uuid.starts_with(&g.uuid)).count();
                if same > 0 {
                    g.uuid = format!("{}-{}", g.uuid, same + 1);
                }
                adapters.push((a, g));
            }
        }
        Ok(Self { adapters })
    }

    pub fn list(&self) -> Vec<GpuInfo> {
        self.adapters
            .iter()
            .map(|(a, g)| {
                let mut g = g.clone();
                let (b, u) = budget_of(a);
                if b.is_some() {
                    g.budget = b;
                    g.usage = u;
                }
                g
            })
            .collect()
    }

    fn take(self, luid: Option<u64>, uuid: &str) -> Option<(IDXGIAdapter1, GpuInfo)> {
        self.adapters.into_iter().find(|(_, g)| (luid.is_some() && g.luid == luid) || g.uuid == uuid)
    }
}

pub struct D3dStore {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    adapter: IDXGIAdapter1,
    staging: Vec<ID3D11Buffer>,
    next: usize,
    staging_size: u64,
    zero: ID3D11Buffer,
    query: ID3D11Query,
    bind: u32,
    ca: ChunkAlloc,
    chunks: Vec<Option<ID3D11Buffer>>,
    chunk_bytes: u64,
    limit: u64,
    reserve: u64,
    has_budget: bool,
    lost: bool,
    refused: Option<(Instant, NoRoom)>,
    pub info: GpuInfo,
}

// Объекты Direct3D используются из одного потока за раз.
unsafe impl Send for D3dStore {}

fn buffer_desc(bytes: u64, usage: D3D11_USAGE, bind: u32, cpu: u32) -> D3D11_BUFFER_DESC {
    D3D11_BUFFER_DESC {
        ByteWidth: bytes as u32,
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
        StructureByteStride: 0,
    }
}

fn make_buffer(dev: &ID3D11Device, desc: &D3D11_BUFFER_DESC, init: Option<&[u8]>) -> windows::core::Result<ID3D11Buffer> {
    let data = init.map(|b| D3D11_SUBRESOURCE_DATA { pSysMem: b.as_ptr() as *const _, SysMemPitch: 0, SysMemSlicePitch: 0 });
    let mut out = None;
    unsafe { dev.CreateBuffer(desc, data.as_ref().map(|d| d as *const _), Some(&mut out)) }?;
    out.ok_or_else(windows::core::Error::empty)
}

fn bx(from: u64, len: u64) -> D3D11_BOX {
    D3D11_BOX { left: from as u32, top: 0, front: 0, right: (from + len) as u32, bottom: 1, back: 1 }
}

impl D3dStore {
    /// Открыть видеокарту `target` через Direct3D 11 (для Vulkan-видеокарты —
    /// по её LUID). Видеопамять под данные пока не выделяется.
    pub fn open(p: &OpenParams, target: &GpuInfo) -> Result<Self, String> {
        let probe = DxgiProbe::new()?;
        let (adapter, mut info) =
            probe.take(target.luid, &target.uuid).ok_or("видеокарта не найдена среди адаптеров Direct3D")?;
        let (device, ctx, _) = create_device(&adapter).map_err(|e| hr("не удалось открыть видеокарту через Direct3D 11", &e))?;
        let chunk_bytes = (p.chunk / BLOCK).max(64) / 64 * 64 * BLOCK;
        let staging_size = p.staging.max(BLOCK) / BLOCK * BLOCK;
        let mut staging = Vec::new();
        for _ in 0..2 {
            let d = buffer_desc(
                staging_size,
                D3D11_USAGE_STAGING,
                0,
                (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32,
            );
            staging.push(make_buffer(&device, &d, None).map_err(|e| hr("промежуточный буфер", &e))?);
        }
        // Буфер нулей: им заполняются новые блоки, в которые пишут частично.
        let zeros = vec![0u8; ZERO_BYTES as usize];
        let mut bind = 0u32;
        let zero = match make_buffer(&device, &buffer_desc(ZERO_BYTES, D3D11_USAGE_DEFAULT, 0, 0), Some(&zeros)) {
            Ok(b) => b,
            Err(_) => {
                // Некоторые драйверы не принимают буфер без назначения.
                bind = D3D11_BIND_SHADER_RESOURCE.0 as u32;
                make_buffer(&device, &buffer_desc(ZERO_BYTES, D3D11_USAGE_DEFAULT, bind, 0), Some(&zeros))
                    .map_err(|e| hr("буфер видеопамяти", &e))?
            }
        };
        let mut query = None;
        unsafe { device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut query)) }
            .map_err(|e| hr("запрос синхронизации", &e))?;
        let query = query.ok_or("запрос синхронизации не создан")?;
        let ca = ChunkAlloc::new(p.capacity.div_ceil(BLOCK), chunk_bytes / BLOCK, p.limit.div_ceil(BLOCK));
        let limit = ca.soft() as u64 * chunk_bytes;
        let n = ca.chunks();
        let has_budget = budget_of(&adapter).0.is_some();
        if let (Some(b), Some(u)) = budget_of(&adapter) {
            info.budget = Some(b);
            info.usage = Some(u);
        }
        Ok(Self {
            device,
            ctx,
            adapter,
            staging,
            next: 0,
            staging_size,
            zero,
            query,
            bind,
            ca,
            chunks: (0..n).map(|_| None).collect(),
            chunk_bytes,
            limit,
            reserve: p.reserve,
            has_budget,
            lost: false,
            refused: None,
            info,
        })
    }

    fn err(&mut self, what: &str, e: windows::core::Error) -> DiskError {
        if removed(&e) {
            self.lost = true;
            let why = unsafe { self.device.GetDeviceRemovedReason() }.err().map(|r| hr("причина", &r)).unwrap_or_default();
            crate::logger::log(format!("Direct3D 11: видеокарта сброшена ({what}). {why}"));
            DiskError::Lost(
                "видеокарта была сброшена драйвером — данные в видеопамяти потеряны. Отключите диск и создайте заново".into(),
            )
        } else {
            DiskError::Device(hr(what, &e))
        }
    }

    fn check(&self) -> Result<(), DiskError> {
        if self.lost {
            Err(DiskError::Lost("видеокарта была сброшена драйвером — данные в видеопамяти потеряны".into()))
        } else {
            Ok(())
        }
    }

    /// Разбить отрезок адресов хранилища по кускам: (кусок, смещение, длина).
    fn split(&self, addr: u64, len: u64) -> Result<Vec<(usize, u64, u64)>, DiskError> {
        let mut out = Vec::new();
        let (mut p, end) = (addr, addr + len);
        while p < end {
            let c = (p / self.chunk_bytes) as usize;
            let local = p % self.chunk_bytes;
            let l = (self.chunk_bytes - local).min(end - p);
            if self.chunks.get(c).and_then(|x| x.as_ref()).is_none() {
                return Err(DiskError::Device(format!("обращение к невыделенному куску видеопамяти {c}")));
            }
            out.push((c, local, l));
            p += l;
        }
        Ok(out)
    }

    fn chunk(&self, c: usize) -> &ID3D11Buffer {
        self.chunks[c].as_ref().expect("кусок выделен")
    }

    fn make_chunk(&mut self) -> windows::core::Result<ID3D11Buffer> {
        let b = make_buffer(&self.device, &buffer_desc(self.chunk_bytes, D3D11_USAGE_DEFAULT, self.bind, 0), None)?;
        // Если видеопамяти не хватит игре, система сначала вытеснит наши куски.
        if let Ok(r) = b.cast::<IDXGIResource>() {
            let _ = unsafe { r.SetEvictionPriority(DXGI_RESOURCE_PRIORITY_LOW) };
        }
        Ok(b)
    }

    fn budget_allows(&self) -> Result<(), NoRoom> {
        if !self.has_budget {
            return Ok(());
        }
        if let (Some(b), Some(u)) = budget_of(&self.adapter) {
            if u.saturating_add(self.chunk_bytes).saturating_add(self.reserve) > b {
                return Err(NoRoom::Budget(format!(
                    "видеопамять занята другими программами: доступно {}, занято диском {}, запас {}",
                    crate::util::size(b),
                    crate::util::size(u),
                    crate::util::size(self.reserve)
                )));
            }
        }
        Ok(())
    }

    /// Дождаться, пока видеокарта выполнит все команды.
    fn wait_idle(&mut self) -> Result<(), DiskError> {
        unsafe {
            self.ctx.End(&self.query);
            self.ctx.Flush();
        }
        let t = Instant::now();
        loop {
            let mut done = BOOL(0);
            let r = unsafe {
                self.ctx.GetData(&self.query, Some(&mut done as *mut _ as *mut _), std::mem::size_of::<BOOL>() as u32, 0)
            };
            if let Err(e) = r {
                return Err(self.err("ожидание видеокарты", e));
            }
            if done.as_bool() {
                return Ok(());
            }
            if t.elapsed() > WAIT {
                return Err(DiskError::Device("видеокарта не ответила за 10 с".into()));
            }
            std::thread::yield_now();
        }
    }

    fn map(&mut self, i: usize, mt: D3D11_MAP) -> Result<*mut u8, DiskError> {
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        if let Err(e) = unsafe { self.ctx.Map(&self.staging[i], 0, mt, 0, Some(&mut m)) } {
            return Err(self.err("доступ к промежуточному буферу", e));
        }
        if m.pData.is_null() {
            unsafe { self.ctx.Unmap(&self.staging[i], 0) };
            return Err(DiskError::Device("промежуточный буфер недоступен".into()));
        }
        Ok(m.pData as *mut u8)
    }
}

impl Store for D3dStore {
    fn kind(&self) -> Kind {
        Kind::Vram
    }

    fn limit(&self) -> u64 {
        self.limit
    }

    fn alloc(&mut self, over: bool) -> Result<Slot, NoRoom> {
        if self.lost {
            return Err(NoRoom::Memory("видеокарта сброшена драйвером".into()));
        }
        if let Some(s) = self.ca.take_live() {
            return Ok(Slot { index: s, zeroed: false });
        }
        let c = self.ca.next_dead(over).ok_or(NoRoom::Limit)?;
        let now = Instant::now();
        if let Some((t, why)) = &self.refused {
            if now.saturating_duration_since(*t) < REFUSAL_BACKOFF {
                return Err(why.clone());
            }
        }
        let r = self.budget_allows().and_then(|_| {
            self.make_chunk().map_err(|e| {
                if removed(&e) {
                    self.lost = true;
                    NoRoom::Memory("видеокарта сброшена драйвером".into())
                } else {
                    NoRoom::Memory(hr("видеокарта отказала в памяти", &e))
                }
            })
        });
        match r {
            Ok(b) => {
                self.refused = None;
                self.chunks[c] = Some(b);
                self.ca.set_live(c);
                Ok(Slot { index: self.ca.take_in(c), zeroed: false })
            }
            Err(e) => {
                self.refused = Some((now, e.clone()));
                Err(e)
            }
        }
    }

    fn free(&mut self, slot: u64) {
        self.ca.release(slot);
    }

    fn used(&self) -> u64 {
        self.ca.used()
    }

    fn held(&self) -> u64 {
        self.ca.live_count() as u64 * self.chunk_bytes
    }

    fn max_io(&self) -> usize {
        self.staging_size as usize
    }

    fn read(&mut self, runs: &[(u64, u64, usize)], out: &mut [u8]) -> Result<(), DiskError> {
        self.check()?;
        if out.len() as u64 > self.staging_size {
            return Err(DiskError::Device("слишком большой запрос чтения".into()));
        }
        let mut parts = Vec::new();
        for &(a, l, o) in runs {
            let mut o = o as u64;
            for (c, local, len) in self.split(a, l)? {
                parts.push((c, local, len, o));
                o += len;
            }
        }
        let i = self.next;
        self.next = (self.next + 1) % self.staging.len();
        for &(c, local, len, o) in &parts {
            unsafe {
                self.ctx.CopySubresourceRegion(&self.staging[i], 0, o as u32, 0, 0, self.chunk(c), 0, Some(&bx(local, len)))
            };
        }
        // Map ждёт, пока копирование в промежуточный буфер закончится.
        let ptr = self.map(i, D3D11_MAP_READ)?;
        for &(_, l, o) in runs {
            let src = unsafe { std::slice::from_raw_parts(ptr.add(o), l as usize) };
            out[o..o + l as usize].copy_from_slice(src);
        }
        unsafe { self.ctx.Unmap(&self.staging[i], 0) };
        Ok(())
    }

    fn write(&mut self, fills: &[(u64, u64)], runs: &[(u64, u64, usize)], data: &[u8]) -> Result<(), DiskError> {
        self.check()?;
        if data.len() as u64 > self.staging_size {
            return Err(DiskError::Device("слишком большой запрос записи".into()));
        }
        let mut fill_parts = Vec::new();
        for &(a, l) in fills {
            fill_parts.extend(self.split(a, l)?);
        }
        let mut parts = Vec::new();
        for &(a, l, o) in runs {
            let mut o = o as u64;
            for (c, local, len) in self.split(a, l)? {
                parts.push((c, local, len, o));
                o += len;
            }
        }
        let i = self.next;
        self.next = (self.next + 1) % self.staging.len();
        if !runs.is_empty() {
            // Если видеокарта ещё копирует из этого буфера, Map дождётся.
            let ptr = self.map(i, D3D11_MAP_WRITE)?;
            for &(_, l, o) in runs {
                unsafe { std::ptr::copy_nonoverlapping(data.as_ptr().add(o), ptr.add(o), l as usize) };
            }
            unsafe { self.ctx.Unmap(&self.staging[i], 0) };
        }
        for &(c, local, len) in &fill_parts {
            let mut done = 0;
            while done < len {
                let l = (len - done).min(ZERO_BYTES);
                unsafe {
                    self.ctx.CopySubresourceRegion(self.chunk(c), 0, (local + done) as u32, 0, 0, &self.zero, 0, Some(&bx(0, l)))
                };
                done += l;
            }
        }
        for &(c, local, len, o) in &parts {
            unsafe {
                self.ctx.CopySubresourceRegion(self.chunk(c), 0, local as u32, 0, 0, &self.staging[i], 0, Some(&bx(o, len)))
            };
        }
        unsafe { self.ctx.Flush() };
        Ok(())
    }

    fn copy(&mut self, moves: &[(u64, u64)]) -> Result<(), DiskError> {
        self.check()?;
        let mut buf = Vec::new();
        for &(s, d) in moves {
            let src = self.split(s * BLOCK, BLOCK)?[0];
            let dst = self.split(d * BLOCK, BLOCK)?[0];
            if src.0 == dst.0 {
                // Внутри одного буфера Direct3D 11 копировать не умеет.
                buf.resize(BLOCK as usize, 0);
                self.read(&[(s * BLOCK, BLOCK, 0)], &mut buf)?;
                self.write(&[], &[(d * BLOCK, BLOCK, 0)], &buf)?;
                continue;
            }
            unsafe {
                self.ctx.CopySubresourceRegion(self.chunk(dst.0), 0, dst.1 as u32, 0, 0, self.chunk(src.0), 0, Some(&bx(src.1, BLOCK)))
            };
        }
        self.wait_idle()
    }

    fn flush(&mut self) -> Result<(), DiskError> {
        self.check()?;
        self.wait_idle()
    }

    fn reap(&mut self, now: Instant, force: bool) -> u64 {
        let dead = self.ca.reap(now, force);
        // Direct3D сам дождётся команд, которые ещё обращаются к кускам.
        for &c in &dead {
            self.chunks[c] = None;
        }
        unsafe { self.ctx.Flush() };
        dead.len() as u64 * self.chunk_bytes
    }

    fn plan_compaction(&mut self, max: usize) -> Vec<(u64, u64)> {
        if self.lost {
            return Vec::new();
        }
        self.ca.plan_compaction(max)
    }
}

impl Drop for D3dStore {
    fn drop(&mut self) {
        unsafe {
            self.ctx.ClearState();
            self.ctx.Flush();
        }
        self.chunks.clear();
    }
}
