//! Размеры и буква диска — общее для окна и для автозапуска без окна.

use super::engine::CreateRequest;
use vramdisk::disk::Kind;
use vramdisk::gpu::GpuInfo;
use vramdisk::settings::{clean_label, FillFirst, FsKind, Settings};
use vramdisk::util::{size as fsize, GIB, MIB};

/// Шаг ползунков размера, ГБ.
pub const STEP_GB: f64 = 0.25;
/// Меньше диск не создаётся.
pub const MIN_SIZE: u64 = 256 * MIB;
/// Буквы, которые диск берёт, если выбранная занята.
const PREFERRED: [char; 7] = ['V', 'W', 'X', 'Y', 'Z', 'R', 'T'];

pub fn gb_to_bytes(gb: f64) -> u64 {
    ((gb * 1024.0).round().max(0.0) as u64) * MIB
}

pub fn floor_step(gb: f64) -> f64 {
    ((gb / STEP_GB).floor() * STEP_GB).max(0.0)
}

pub fn bytes_gb(b: u64) -> f64 {
    b as f64 / GIB as f64
}

/// Наибольший предел оперативной памяти, ГБ: вся память без запаса системе.
pub fn max_ram_gb(total: u64, reserve_mb: u64) -> f64 {
    floor_step(bytes_gb(total.saturating_sub(reserve_mb * MIB)))
}

/// Наибольший предел видеопамяти, ГБ: вся видеопамять без запаса для игр.
pub fn max_vram_gb(g: Option<&GpuInfo>, reserve_mb: u64) -> f64 {
    g.map(|g| floor_step(bytes_gb(g.vram_total.saturating_sub(reserve_mb * MIB)))).unwrap_or(0.0)
}

/// Выбранная буква, если свободна; иначе первая свободная из привычных
/// (V, W, X…), иначе последняя свободная.
pub fn pick_letter(want: char, free: &[char]) -> Option<char> {
    if free.contains(&want) {
        return Some(want);
    }
    PREFERRED.iter().copied().find(|c| free.contains(c)).or_else(|| free.last().copied())
}

/// Запрос на создание диска с готовыми размерами частей.
pub fn request(s: &Settings, ram: u64, vram: u64, letter: char, gpu: Option<&GpuInfo>, demo: bool) -> CreateRequest {
    let first = match s.fill_first {
        _ if vram == 0 => Kind::Ram,
        FillFirst::Ram => Kind::Ram,
        FillFirst::Vram => Kind::Vram,
    };
    CreateRequest {
        size: ram + vram,
        ram,
        vram,
        first,
        letter,
        label: clean_label(&s.label, s.fs),
        fs: s.fs,
        compress: s.compress && s.fs == FsKind::Ntfs,
        gpu_uuid: gpu.map(|g| g.uuid.clone()),
        vram_reserve: s.vram_reserve_mb * MIB,
        demo,
    }
}

/// Диск при входе в Windows — по сохранённым настройкам. Если памяти
/// сейчас меньше (или видеокарта недоступна), диск будет меньше, а
/// настройки останутся прежними. Возвращает и строки для журнала.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn autostart_request(
    s: &Settings,
    mem_total: u64,
    gpu: Option<&GpuInfo>,
    free: &[char],
) -> Result<(CreateRequest, Vec<String>), String> {
    let mut notes = Vec::new();
    let ram = gb_to_bytes(bytes_gb(s.ram_mb * MIB).min(max_ram_gb(mem_total, s.ram_reserve_mb)));
    let vram = match gpu {
        Some(g) => gb_to_bytes(bytes_gb(s.vram_mb * MIB).min(max_vram_gb(Some(g), s.vram_reserve_mb))),
        None => {
            if s.vram_mb > 0 {
                notes.push("видеокарта недоступна — диск будет только из оперативной памяти".to_string());
            }
            0
        }
    };
    if ram + vram < MIN_SIZE {
        return Err(format!(
            "Не хватает памяти: можно занять {}, а диску нужно хотя бы {}.",
            fsize(ram + vram),
            fsize(MIN_SIZE)
        ));
    }
    if ram / MIB < s.ram_mb || (gpu.is_some() && vram / MIB < s.vram_mb) {
        notes.push(format!(
            "памяти меньше, чем в настройках — диск {} вместо {}",
            fsize(ram + vram),
            fsize((s.ram_mb + s.vram_mb) * MIB)
        ));
    }
    let letter = pick_letter(s.letter, free).ok_or_else(|| "Нет свободной буквы диска.".to_string())?;
    if letter != s.letter {
        notes.push(format!("буква {}: занята — диск будет {letter}:", s.letter));
    }
    Ok((request(s, ram, vram, letter, gpu, false), notes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vramdisk::gpu::{Api, GpuKind};

    fn gpu(total: u64) -> GpuInfo {
        GpuInfo {
            index: 0,
            name: "GPU".into(),
            uuid: "u1".into(),
            kind: GpuKind::Discrete,
            vram_total: total,
            budget: None,
            usage: None,
            api: "Vulkan".into(),
            backend: Api::Vulkan,
            transfer_queue: false,
            luid: None,
            d3d_fallback: false,
        }
    }

    #[test]
    fn letters() {
        assert_eq!(pick_letter('Q', &['D', 'Q', 'V']), Some('Q'));
        assert_eq!(pick_letter('Q', &['D', 'W', 'V']), Some('V'));
        assert_eq!(pick_letter('Q', &['D', 'G']), Some('G'));
        assert_eq!(pick_letter('Q', &[]), None);
    }

    #[test]
    fn autostart_sizes() {
        let s = Settings {
            ram_mb: 2048,
            vram_mb: 4096,
            ram_reserve_mb: 2048,
            vram_reserve_mb: 1024,
            fill_first: FillFirst::Vram,
            letter: 'V',
            ..Default::default()
        };
        // Всё помещается.
        let (r, notes) = autostart_request(&s, 16 * GIB, Some(&gpu(8 * GIB)), &['V', 'W']).unwrap();
        assert_eq!((r.ram, r.vram, r.size), (2 * GIB, 4 * GIB, 6 * GIB));
        assert_eq!((r.letter, r.first, r.gpu_uuid.as_deref()), ('V', Kind::Vram, Some("u1")));
        assert!(notes.is_empty());
        // Видеокарта меньше: видеопамять урезана запасом.
        let (r, notes) = autostart_request(&s, 16 * GIB, Some(&gpu(4 * GIB)), &['W']).unwrap();
        assert_eq!((r.ram, r.vram, r.letter), (2 * GIB, 3 * GIB, 'W'));
        assert_eq!(notes.len(), 2);
        // Нет видеокарты — только ОЗУ, первой занимается она.
        let (r, _) = autostart_request(&s, 16 * GIB, None, &['V']).unwrap();
        assert_eq!((r.vram, r.first), (0, Kind::Ram));
        // Памяти нет совсем.
        assert!(autostart_request(&s, 2 * GIB, None, &['V']).is_err());
        assert!(autostart_request(&s, 16 * GIB, None, &[]).is_err());
    }
}
