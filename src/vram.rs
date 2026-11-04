//! Видеопамять через Vulkan.
//!
//! Видеопамять берётся у драйвера кусками по 64 МиБ — по мере того как на
//! диск пишут данные, — и возвращается, когда кусок опустел. Куски не
//! отображаются в память процесса: данные идут через промежуточный буфер в
//! оперативной памяти и копируются командами передачи (на многих видеокартах —
//! отдельным движком DMA, не мешая играм). Два промежуточных буфера по
//! очереди: запись подтверждается сразу после постановки копирования в
//! очередь, а барьер в начале каждой команды гарантирует, что следующее
//! чтение увидит записанное.

use crate::disk::DiskError;
use crate::gpu::{Api, GpuInfo, GpuKind, OpenParams};
use crate::pool::{ChunkAlloc, Kind, NoRoom, Slot, Store, BLOCK};
use ash::vk;
use std::ffi::CStr;
use std::time::{Duration, Instant};

const FENCE_TIMEOUT_NS: u64 = 10_000_000_000;
/// После отказа в видеопамяти не спрашиваем драйвер снова какое-то время.
const REFUSAL_BACKOFF: Duration = Duration::from_secs(1);

fn vkerr(what: &str, e: vk::Result) -> String {
    format!("{what}: {e:?}")
}

fn load_entry() -> Result<ash::Entry, String> {
    unsafe { ash::Entry::load() }
        .map_err(|e| format!("Vulkan не найден ({e}). Обновите драйвер видеокарты с сайта её производителя."))
}

fn create_instance(entry: &ash::Entry) -> Result<ash::Instance, String> {
    let ver = unsafe { entry.try_enumerate_instance_version() }.ok().flatten().unwrap_or(vk::API_VERSION_1_0);
    if ver < vk::API_VERSION_1_1 {
        return Err("нужен Vulkan 1.1 или новее — обновите драйвер видеокарты".into());
    }
    let app = vk::ApplicationInfo::default()
        .application_name(c"VramDisk")
        .application_version(2)
        .engine_name(c"VramDisk")
        .api_version(vk::API_VERSION_1_1);
    let ci = vk::InstanceCreateInfo::default().application_info(&app);
    unsafe { entry.create_instance(&ci, None) }.map_err(|e| vkerr("не удалось запустить Vulkan", e))
}

fn has_ext(instance: &ash::Instance, pd: vk::PhysicalDevice, name: &CStr) -> bool {
    unsafe { instance.enumerate_device_extension_properties(pd) }
        .map(|v| v.iter().any(|e| e.extension_name_as_c_str().ok() == Some(name)))
        .unwrap_or(false)
}

/// Основная куча видеопамяти: самая большая с флагом DEVICE_LOCAL.
fn main_heap(mp: &vk::PhysicalDeviceMemoryProperties) -> Option<(u32, u64)> {
    mp.memory_heaps_as_slice()
        .iter()
        .enumerate()
        .filter(|(_, h)| h.flags.contains(vk::MemoryHeapFlags::DEVICE_LOCAL))
        .max_by_key(|(_, h)| h.size)
        .map(|(i, h)| (i as u32, h.size))
}

fn budget_of(instance: &ash::Instance, pd: vk::PhysicalDevice, heap: u32) -> (Option<u64>, Option<u64>) {
    let mut b = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
    let mut p2 = vk::PhysicalDeviceMemoryProperties2::default().push_next(&mut b);
    unsafe { instance.get_physical_device_memory_properties2(pd, &mut p2) };
    let _ = p2;
    let (bud, used) = (b.heap_budget[heap as usize], b.heap_usage[heap as usize]);
    if bud == 0 {
        (None, None)
    } else {
        (Some(bud), Some(used))
    }
}

/// Семейство очередей для копирования: отдельное (DMA), если есть.
fn transfer_family(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Option<(u32, bool)> {
    let fams = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    let dedicated = fams.iter().position(|f| {
        f.queue_count > 0
            && f.queue_flags.contains(vk::QueueFlags::TRANSFER)
            && !f.queue_flags.intersects(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
    });
    if let Some(i) = dedicated {
        return Some((i as u32, true));
    }
    // Любая очередь графики или вычислений тоже умеет копировать.
    fams.iter()
        .position(|f| {
            f.queue_count > 0
                && f.queue_flags.intersects(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER)
        })
        .map(|i| (i as u32, false))
}

fn describe(instance: &ash::Instance, pd: vk::PhysicalDevice, index: usize) -> Option<GpuInfo> {
    let props = unsafe { instance.get_physical_device_properties(pd) };
    if props.api_version < vk::API_VERSION_1_1 {
        return None;
    }
    let mut idp = vk::PhysicalDeviceIDProperties::default();
    let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut idp);
    unsafe { instance.get_physical_device_properties2(pd, &mut p2) };
    let _ = p2;
    let uuid = idp.device_uuid.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let luid = (idp.device_luid_valid == vk::TRUE).then(|| u64::from_le_bytes(idp.device_luid));
    let mp = unsafe { instance.get_physical_device_memory_properties(pd) };
    let (heap, total) = main_heap(&mp)?;
    let (budget, usage) =
        if has_ext(instance, pd, ash::ext::memory_budget::NAME) { budget_of(instance, pd, heap) } else { (None, None) };
    let kind = match props.device_type {
        vk::PhysicalDeviceType::DISCRETE_GPU => GpuKind::Discrete,
        vk::PhysicalDeviceType::INTEGRATED_GPU => GpuKind::Integrated,
        vk::PhysicalDeviceType::VIRTUAL_GPU => GpuKind::Virtual,
        vk::PhysicalDeviceType::CPU => GpuKind::Cpu,
        _ => GpuKind::Other,
    };
    let name =
        props.device_name_as_c_str().map(|c| c.to_string_lossy().into_owned()).unwrap_or_else(|_| "Видеокарта".into());
    let v = props.api_version;
    let (_, tq) = transfer_family(instance, pd)?;
    Some(GpuInfo {
        index,
        name,
        uuid,
        kind,
        vram_total: total,
        budget,
        usage,
        api: format!("Vulkan {}.{}.{}", vk::api_version_major(v), vk::api_version_minor(v), vk::api_version_patch(v)),
        backend: Api::Vulkan,
        transfer_queue: tq,
        luid,
        d3d_fallback: false,
    })
}

/// Опрос видеокарт через Vulkan: держит экземпляр, чтобы быстро обновлять
/// свободную видеопамять.
pub struct VkProbe {
    _entry: ash::Entry,
    instance: ash::Instance,
    devices: Vec<vk::PhysicalDevice>,
}

impl VkProbe {
    pub fn new() -> Result<Self, String> {
        let entry = load_entry()?;
        let instance = create_instance(&entry)?;
        let devices = match unsafe { instance.enumerate_physical_devices() } {
            Ok(d) => d,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(vkerr("видеокарты не найдены", e));
            }
        };
        Ok(Self { _entry: entry, instance, devices })
    }

    pub fn list(&self) -> Vec<GpuInfo> {
        self.devices.iter().enumerate().filter_map(|(i, &pd)| describe(&self.instance, pd, i)).collect()
    }
}

impl Drop for VkProbe {
    fn drop(&mut self) {
        unsafe { self.instance.destroy_instance(None) };
    }
}

struct Chunk {
    buf: vk::Buffer,
    mem: vk::DeviceMemory,
}

struct Staging {
    buf: vk::Buffer,
    mem: vk::DeviceMemory,
    ptr: *mut u8,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    busy: bool,
}

pub struct VkStore {
    _entry: ash::Entry,
    instance: ash::Instance,
    pd: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    staging: Vec<Staging>,
    next: usize,
    staging_size: u64,
    mp: vk::PhysicalDeviceMemoryProperties,
    heap: u32,
    has_budget: bool,
    has_prio: bool,
    ca: ChunkAlloc,
    chunks: Vec<Option<Chunk>>,
    chunk_bytes: u64,
    limit: u64,
    reserve: u64,
    lost: bool,
    refused: Option<(Instant, NoRoom)>,
    pub info: GpuInfo,
}

// Все объекты Vulkan используются из одного потока за раз.
unsafe impl Send for VkStore {}

fn find_type(
    mp: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: vk::MemoryPropertyFlags,
    avoid: vk::MemoryPropertyFlags,
    heap: Option<u32>,
) -> Option<u32> {
    mp.memory_types_as_slice().iter().enumerate().find_map(|(i, t)| {
        let ok = bits & (1 << i) != 0
            && t.property_flags.contains(want)
            && !t.property_flags.intersects(avoid)
            && heap.is_none_or(|h| t.heap_index == h);
        ok.then_some(i as u32)
    })
}

impl VkStore {
    /// Открыть видеокарту `target` (по UUID). Видеопамять под данные пока не
    /// выделяется — только промежуточные буферы.
    pub fn open(p: &OpenParams, target: &GpuInfo) -> Result<Self, String> {
        let entry = load_entry()?;
        let instance = create_instance(&entry)?;
        // Пока объекта нет, убираем за собой сами; дальше — его Drop.
        let (pd, info, qf, has_budget, has_prio) = match Self::choose(&instance, &target.uuid) {
            Ok(v) => v,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(e);
            }
        };
        let mut exts = Vec::new();
        if has_budget {
            exts.push(ash::ext::memory_budget::NAME.as_ptr());
        }
        if has_prio {
            exts.push(ash::ext::memory_priority::NAME.as_ptr());
        }
        let prios = [1.0f32];
        let qci = [vk::DeviceQueueCreateInfo::default().queue_family_index(qf).queue_priorities(&prios)];
        let mut prio_feat = vk::PhysicalDeviceMemoryPriorityFeaturesEXT::default().memory_priority(true);
        let mut dci = vk::DeviceCreateInfo::default().queue_create_infos(&qci).enabled_extension_names(&exts);
        if has_prio {
            dci = dci.push_next(&mut prio_feat);
        }
        let device = match unsafe { instance.create_device(pd, &dci, None) } {
            Ok(d) => d,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(vkerr("не удалось открыть видеокарту", e));
            }
        };
        let pool_ci =
            vk::CommandPoolCreateInfo::default().queue_family_index(qf).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let pool = match unsafe { device.create_command_pool(&pool_ci, None) } {
            Ok(pl) => pl,
            Err(e) => {
                unsafe {
                    device.destroy_device(None);
                    instance.destroy_instance(None);
                }
                return Err(vkerr("command pool", e));
            }
        };
        let queue = unsafe { device.get_device_queue(qf, 0) };
        let mp = unsafe { instance.get_physical_device_memory_properties(pd) };
        let heap = main_heap(&mp).map(|h| h.0).unwrap_or(0);
        let chunk_bytes = (p.chunk / BLOCK).max(64) / 64 * 64 * BLOCK;
        let ca = ChunkAlloc::new(p.capacity.div_ceil(BLOCK), chunk_bytes / BLOCK, p.limit.div_ceil(BLOCK));
        // Предел — целое число кусков, иначе последний кусок считался бы «сверх предела».
        let limit = ca.soft() as u64 * chunk_bytes;
        let n = ca.chunks();
        let mut me = Self {
            _entry: entry,
            instance,
            pd,
            device,
            queue,
            pool,
            staging: Vec::new(),
            next: 0,
            staging_size: p.staging,
            mp,
            heap,
            has_budget,
            has_prio,
            ca,
            chunks: (0..n).map(|_| None).collect(),
            chunk_bytes,
            limit,
            reserve: p.reserve,
            lost: false,
            refused: None,
            info,
        };
        me.make_staging()?;
        Ok(me)
    }

    /// Устройство по UUID, его очередь и расширения; ничего не создаёт.
    fn choose(instance: &ash::Instance, uuid: &str) -> Result<(vk::PhysicalDevice, GpuInfo, u32, bool, bool), String> {
        let pds = unsafe { instance.enumerate_physical_devices() }.map_err(|e| vkerr("видеокарты не найдены", e))?;
        let (pd, info) = pds
            .iter()
            .enumerate()
            .filter_map(|(i, &pd)| describe(instance, pd, i).map(|g| (pd, g)))
            .find(|(_, g)| g.uuid == uuid)
            .ok_or("видеокарта не найдена (её отключили или сменился драйвер)")?;
        let (qf, _) = transfer_family(instance, pd).ok_or("у видеокарты нет очереди передачи данных")?;
        let has_budget = has_ext(instance, pd, ash::ext::memory_budget::NAME);
        let mut has_prio = has_ext(instance, pd, ash::ext::memory_priority::NAME);
        if has_prio {
            let mut f = vk::PhysicalDeviceMemoryPriorityFeaturesEXT::default();
            let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut f);
            unsafe { instance.get_physical_device_features2(pd, &mut f2) };
            let _ = f2;
            has_prio = f.memory_priority == vk::TRUE;
        }
        Ok((pd, info, qf, has_budget, has_prio))
    }

    fn make_staging(&mut self) -> Result<(), String> {
        let cmds = unsafe {
            self.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(2),
            )
        }
        .map_err(|e| vkerr("command buffers", e))?;
        for cmd in cmds {
            let s = self.make_slot(cmd)?;
            self.staging.push(s);
        }
        Ok(())
    }

    fn make_slot(&self, cmd: vk::CommandBuffer) -> Result<Staging, String> {
        let d = &self.device;
        let buf = unsafe {
            d.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(self.staging_size)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }
        .map_err(|e| vkerr("staging buffer", e))?;
        let req = unsafe { d.get_buffer_memory_requirements(buf) };
        use vk::MemoryPropertyFlags as M;
        let mp = &self.mp;
        let ty = find_type(mp, req.memory_type_bits, M::HOST_VISIBLE | M::HOST_COHERENT | M::HOST_CACHED, M::DEVICE_LOCAL, None)
            .or_else(|| find_type(mp, req.memory_type_bits, M::HOST_VISIBLE | M::HOST_COHERENT, M::DEVICE_LOCAL, None))
            .or_else(|| find_type(mp, req.memory_type_bits, M::HOST_VISIBLE | M::HOST_COHERENT, M::empty(), None));
        let Some(ty) = ty else {
            unsafe { d.destroy_buffer(buf, None) };
            return Err("нет подходящей памяти для обмена с видеокартой".into());
        };
        let mem = match unsafe {
            d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None)
        } {
            Ok(m) => m,
            Err(e) => {
                unsafe { d.destroy_buffer(buf, None) };
                return Err(vkerr("staging memory", e));
            }
        };
        let cleanup = || unsafe {
            d.destroy_buffer(buf, None);
            d.free_memory(mem, None);
        };
        if let Err(e) = unsafe { d.bind_buffer_memory(buf, mem, 0) } {
            cleanup();
            return Err(vkerr("bind staging", e));
        }
        let ptr = match unsafe { d.map_memory(mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) } {
            Ok(p) => p as *mut u8,
            Err(e) => {
                cleanup();
                return Err(vkerr("map staging", e));
            }
        };
        let fence =
            match unsafe { d.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None) } {
                Ok(f) => f,
                Err(e) => {
                    unsafe { d.unmap_memory(mem) };
                    cleanup();
                    return Err(vkerr("fence", e));
                }
            };
        Ok(Staging { buf, mem, ptr, cmd, fence, busy: false })
    }

    fn make_chunk(&self) -> Result<Chunk, vk::Result> {
        let d = &self.device;
        let buf = unsafe {
            d.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(self.chunk_bytes)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }?;
        let req = unsafe { d.get_buffer_memory_requirements(buf) };
        use vk::MemoryPropertyFlags as M;
        let mp = &self.mp;
        let ty = find_type(mp, req.memory_type_bits, M::DEVICE_LOCAL, M::HOST_VISIBLE, Some(self.heap))
            .or_else(|| find_type(mp, req.memory_type_bits, M::DEVICE_LOCAL, M::empty(), Some(self.heap)))
            .or_else(|| find_type(mp, req.memory_type_bits, M::DEVICE_LOCAL, M::empty(), None));
        let Some(ty) = ty else {
            unsafe { d.destroy_buffer(buf, None) };
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        };
        // Низкий приоритет: если игре не хватит видеопамяти, система сначала
        // вытеснит данные диска, а не игры.
        let mut pr = vk::MemoryPriorityAllocateInfoEXT::default().priority(0.2);
        let mut ai = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty);
        if self.has_prio {
            ai = ai.push_next(&mut pr);
        }
        let mem = match unsafe { d.allocate_memory(&ai, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { d.destroy_buffer(buf, None) };
                return Err(e);
            }
        };
        if let Err(e) = unsafe { d.bind_buffer_memory(buf, mem, 0) } {
            unsafe {
                d.destroy_buffer(buf, None);
                d.free_memory(mem, None);
            }
            return Err(e);
        }
        Ok(Chunk { buf, mem })
    }

    /// Текущий бюджет и занятость видеопамяти (если драйвер сообщает).
    pub fn budget(&self) -> (Option<u64>, Option<u64>) {
        if self.has_budget {
            budget_of(&self.instance, self.pd, self.heap)
        } else {
            (None, None)
        }
    }

    fn err(&mut self, what: &str, e: vk::Result) -> DiskError {
        match e {
            vk::Result::ERROR_DEVICE_LOST => {
                self.lost = true;
                DiskError::Lost(
                    "видеокарта была сброшена драйвером — данные в видеопамяти потеряны. Отключите диск и создайте заново"
                        .into(),
                )
            }
            vk::Result::TIMEOUT => DiskError::Device(format!("{what}: видеокарта не ответила за 10 с")),
            e => DiskError::Device(format!("{what}: {e:?}")),
        }
    }

    fn check(&self) -> Result<(), DiskError> {
        if self.lost {
            Err(DiskError::Lost("видеокарта была сброшена драйвером — данные в видеопамяти потеряны".into()))
        } else {
            Ok(())
        }
    }

    /// Берёт следующий промежуточный буфер: ждёт его прошлую операцию и
    /// начинает команду.
    fn acquire(&mut self) -> Result<usize, DiskError> {
        let i = self.next;
        self.next = (self.next + 1) % self.staging.len();
        let (fence, cmd) = (self.staging[i].fence, self.staging[i].cmd);
        if self.staging[i].busy {
            if let Err(e) = unsafe { self.device.wait_for_fences(&[fence], true, FENCE_TIMEOUT_NS) } {
                return Err(self.err("ожидание видеокарты", e));
            }
            self.staging[i].busy = false;
        }
        unsafe {
            self.device.reset_fences(&[fence]).map_err(|e| self.err("reset fence", e))?;
            self.device
                .reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .map_err(|e| self.err("reset cmd", e))?;
            self.device
                .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))
                .map_err(|e| self.err("begin cmd", e))?;
        }
        // Всё, что раньше было отправлено в очередь, завершается до этой команды.
        self.barrier(cmd, vk::PipelineStageFlags::TRANSFER, vk::AccessFlags::TRANSFER_READ | vk::AccessFlags::TRANSFER_WRITE);
        Ok(i)
    }

    fn barrier(&self, cmd: vk::CommandBuffer, dst_stage: vk::PipelineStageFlags, dst_access: vk::AccessFlags) {
        let mb = [vk::MemoryBarrier::default().src_access_mask(vk::AccessFlags::TRANSFER_WRITE).dst_access_mask(dst_access)];
        unsafe {
            self.device.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TRANSFER, dst_stage, vk::DependencyFlags::empty(), &mb, &[], &[])
        };
    }

    fn submit(&mut self, i: usize, wait: bool) -> Result<(), DiskError> {
        let (cmd, fence) = (self.staging[i].cmd, self.staging[i].fence);
        unsafe {
            self.device.end_command_buffer(cmd).map_err(|e| self.err("end cmd", e))?;
            let cmds = [cmd];
            let si = [vk::SubmitInfo::default().command_buffers(&cmds)];
            self.device.queue_submit(self.queue, &si, fence).map_err(|e| self.err("отправка команды", e))?;
        }
        self.staging[i].busy = true;
        if wait {
            if let Err(e) = unsafe { self.device.wait_for_fences(&[fence], true, FENCE_TIMEOUT_NS) } {
                return Err(self.err("ожидание видеокарты", e));
            }
            self.staging[i].busy = false;
        }
        Ok(())
    }

    /// Отрезок адресов по кускам: (кусок, смещение в куске, длина).
    fn split(&self, addr: u64, len: u64) -> Result<Vec<(vk::Buffer, u64, u64)>, DiskError> {
        let mut out = Vec::new();
        let (mut p, end) = (addr, addr + len);
        while p < end {
            let c = (p / self.chunk_bytes) as usize;
            let local = p % self.chunk_bytes;
            let l = (self.chunk_bytes - local).min(end - p);
            let buf = self
                .chunks
                .get(c)
                .and_then(|c| c.as_ref())
                .ok_or_else(|| DiskError::Device(format!("обращение к невыделенному куску видеопамяти {c}")))?
                .buf;
            out.push((buf, local, l));
            p += l;
        }
        Ok(out)
    }

    fn destroy_chunk(&mut self, c: usize) {
        if let Some(ch) = self.chunks[c].take() {
            unsafe {
                self.device.destroy_buffer(ch.buf, None);
                self.device.free_memory(ch.mem, None);
            }
        }
    }

    /// Хватит ли видеопамяти ещё на один кусок с учётом запаса для других программ.
    fn budget_allows(&self) -> Result<(), NoRoom> {
        if let (Some(b), Some(u)) = self.budget() {
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
}

impl Store for VkStore {
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
            self.make_chunk().map_err(|e| match e {
                vk::Result::ERROR_OUT_OF_DEVICE_MEMORY | vk::Result::ERROR_OUT_OF_HOST_MEMORY => {
                    NoRoom::Memory("видеокарта отказала в памяти — видеопамять занята".into())
                }
                vk::Result::ERROR_DEVICE_LOST => {
                    self.lost = true;
                    NoRoom::Memory("видеокарта сброшена драйвером".into())
                }
                e => NoRoom::Memory(format!("ошибка выделения видеопамяти: {e:?}")),
            })
        });
        match r {
            Ok(ch) => {
                self.refused = None;
                self.chunks[c] = Some(ch);
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
            for (buf, local, len) in self.split(a, l)? {
                parts.push((buf, vk::BufferCopy { src_offset: local, dst_offset: o, size: len }));
                o += len;
            }
        }
        let i = self.acquire()?;
        let (cmd, sbuf, ptr) = (self.staging[i].cmd, self.staging[i].buf, self.staging[i].ptr);
        for (buf, region) in &parts {
            unsafe { self.device.cmd_copy_buffer(cmd, *buf, sbuf, std::slice::from_ref(region)) };
        }
        // Данные должны стать видны процессору после ожидания.
        self.barrier(cmd, vk::PipelineStageFlags::HOST, vk::AccessFlags::HOST_READ);
        self.submit(i, true)?;
        for &(_, l, o) in runs {
            let src = unsafe { std::slice::from_raw_parts(ptr.add(o), l as usize) };
            out[o..o + l as usize].copy_from_slice(src);
        }
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
            for (buf, local, len) in self.split(a, l)? {
                parts.push((buf, vk::BufferCopy { src_offset: o, dst_offset: local, size: len }));
                o += len;
            }
        }
        let i = self.acquire()?;
        let (cmd, sbuf, ptr) = (self.staging[i].cmd, self.staging[i].buf, self.staging[i].ptr);
        for &(_, l, o) in runs {
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr().add(o), ptr.add(o), l as usize) };
        }
        if !fill_parts.is_empty() {
            for &(buf, local, len) in &fill_parts {
                unsafe { self.device.cmd_fill_buffer(cmd, buf, local, len, 0) };
            }
            self.barrier(cmd, vk::PipelineStageFlags::TRANSFER, vk::AccessFlags::TRANSFER_WRITE);
        }
        for (buf, region) in &parts {
            unsafe { self.device.cmd_copy_buffer(cmd, sbuf, *buf, std::slice::from_ref(region)) };
        }
        self.submit(i, false)
    }

    fn copy(&mut self, moves: &[(u64, u64)]) -> Result<(), DiskError> {
        self.check()?;
        let mut parts = Vec::new();
        for &(s, d) in moves {
            let src = self.split(s * BLOCK, BLOCK)?;
            let dst = self.split(d * BLOCK, BLOCK)?;
            // Слот целиком лежит в одном куске.
            parts.push((src[0].0, dst[0].0, vk::BufferCopy { src_offset: src[0].1, dst_offset: dst[0].1, size: BLOCK }));
        }
        let i = self.acquire()?;
        let cmd = self.staging[i].cmd;
        for (s, d, region) in &parts {
            unsafe { self.device.cmd_copy_buffer(cmd, *s, *d, std::slice::from_ref(region)) };
        }
        self.submit(i, true)
    }

    fn flush(&mut self) -> Result<(), DiskError> {
        for i in 0..self.staging.len() {
            if self.staging[i].busy {
                let f = self.staging[i].fence;
                if let Err(e) = unsafe { self.device.wait_for_fences(&[f], true, FENCE_TIMEOUT_NS) } {
                    return Err(self.err("ожидание видеокарты", e));
                }
                self.staging[i].busy = false;
            }
        }
        Ok(())
    }

    fn reap(&mut self, now: Instant, force: bool) -> u64 {
        let dead = self.ca.reap(now, force);
        if dead.is_empty() {
            return 0;
        }
        // Куски могут быть заняты ещё не выполненными командами.
        if self.flush().is_err() {
            let _ = unsafe { self.device.device_wait_idle() };
        }
        for &c in &dead {
            self.destroy_chunk(c);
        }
        dead.len() as u64 * self.chunk_bytes
    }

    fn plan_compaction(&mut self, max: usize) -> Vec<(u64, u64)> {
        if self.lost {
            return Vec::new();
        }
        self.ca.plan_compaction(max)
    }
}

impl Drop for VkStore {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            for c in 0..self.chunks.len() {
                self.destroy_chunk(c);
            }
            for s in self.staging.drain(..) {
                self.device.destroy_fence(s.fence, None);
                self.device.destroy_buffer(s.buf, None);
                self.device.unmap_memory(s.mem);
                self.device.free_memory(s.mem, None);
            }
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
