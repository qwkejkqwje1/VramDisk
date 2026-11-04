//! Форматирование размеров и скоростей по-русски: «10,5 ГБ», «850 МБ/с».

pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;

fn comma(s: String) -> String {
    s.replace('.', ",")
}

/// «10,5 ГБ», «512 МБ», «0 Б».
pub fn size(bytes: u64) -> String {
    let b = bytes as f64;
    if bytes >= GIB {
        let g = b / GIB as f64;
        if g >= 100.0 {
            format!("{g:.0} ГБ")
        } else {
            comma(format!("{g:.1} ГБ"))
        }
    } else if bytes >= MIB {
        format!("{:.0} МБ", b / MIB as f64)
    } else if bytes >= 1024 {
        format!("{:.0} КБ", b / 1024.0)
    } else {
        format!("{bytes} Б")
    }
}

/// Размер в ГБ с одним знаком, без единиц: «6,0».
pub fn gb1(bytes: u64) -> String {
    comma(format!("{:.1}", bytes as f64 / GIB as f64))
}

/// «1,8 ГБ/с», «350 МБ/с», «0 МБ/с».
pub fn speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= GIB as f64 {
        comma(format!("{:.1} ГБ/с", bytes_per_sec / GIB as f64))
    } else {
        format!("{:.0} МБ/с", bytes_per_sec / MIB as f64)
    }
}

/// Округляет вниз до кратного `step`.
pub fn round_down(v: u64, step: u64) -> u64 {
    v / step * step
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formats() {
        assert_eq!(size(10 * GIB + GIB / 2), "10,5 ГБ");
        assert_eq!(size(512 * MIB), "512 МБ");
        assert_eq!(speed(1.8 * GIB as f64), "1,8 ГБ/с");
        assert_eq!(speed(350.0 * MIB as f64), "350 МБ/с");
        assert_eq!(gb1(6 * GIB), "6,0");
    }
}
