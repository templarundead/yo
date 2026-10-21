//! Поиск системных шрифтов и чтение их имён из таблицы `name`.
//!
//! `egui` умеет рисовать только те шрифты, которые ему передали байтами.
//! Поэтому «выбрать семейство и начертание» — это фактически «найти файл
//! `.ttf`/`.otf` с нужным именем». Модуль обходит стандартные каталоги
//! шрифтов ОС, читает таблицу `name` каждого файла и складывает найденное
//! в [`FontCatalog`].
//!
//! Разбор `name` написан вручную (около сотни строк), чтобы не тянуть
//! в проект ещё одну зависимость: формат простой — заголовок SFNT, один
//! указатель на таблицу и записи с UTF-16BE-строками.
//!
//! Обход выполняется только по требованию (когда открыт диалог шрифта):
//! старт приложения не должен зависеть от количества установленных шрифтов.

use std::path::{Path, PathBuf};

use yol::settings::FontStyle;

/// Больше файлов не читаем — защита от гигантских коллекций шрифтов.
pub const MAX_FONTS: usize = 400;

/// Файлы больше этого размера пропускаем (обычно это .ttc с эмодзи).
const MAX_FONT_BYTES: u64 = 32 * 1024 * 1024;

/// Глубина обхода каталогов шрифтов.
const MAX_DEPTH: usize = 3;

/// Один найденный файл шрифта.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFace {
    /// Семейство из таблицы `name` (nameID 16 или 1).
    pub family: String,
    /// Начертание из nameID 17/2.
    pub style: FontStyle,
    pub path: PathBuf,
}

/// Каталог найденных шрифтов.
#[derive(Default, Clone, Debug)]
pub struct FontCatalog {
    pub faces: Vec<FontFace>,
}

impl FontCatalog {
    /// Пройти по системным каталогам и собрать шрифты.
    pub fn scan() -> Self {
        let mut faces = Vec::new();
        for dir in system_font_dirs() {
            walk(&dir, 0, &mut faces);
            if faces.len() >= MAX_FONTS {
                break;
            }
        }
        // Один и тот же файл может лежать в двух каталогах — убираем дубли
        // по пути, а одинаковые семейство+начертание оставляем первыми.
        let mut seen_paths = std::collections::HashSet::new();
        let mut unique = Vec::with_capacity(faces.len());
        for face in faces {
            if !seen_paths.insert(face.path.clone()) {
                continue;
            }
            if unique
                .iter()
                .any(|f: &FontFace| f.family == face.family && f.style == face.style)
            {
                continue;
            }
            unique.push(face);
        }
        unique.sort_by(|a, b| {
            a.family
                .to_lowercase()
                .cmp(&b.family.to_lowercase())
                .then(a.style.key().cmp(b.style.key()))
        });
        unique.truncate(MAX_FONTS);
        Self { faces: unique }
    }

    /// Семейства в алфавитном порядке (без повторов).
    pub fn families(&self) -> Vec<String> {
        let mut names: Vec<String> = self.faces.iter().map(|f| f.family.clone()).collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        names
    }

    /// Точное совпадение семейства и начертания.
    pub fn find(&self, family: &str, style: FontStyle) -> Option<&FontFace> {
        self.faces
            .iter()
            .find(|f| f.family.eq_ignore_ascii_case(family) && f.style == style)
    }

    /// Ближайшее доступное начертание: сначала без курсива, потом без
    /// жирности, в конце — обычное.
    pub fn best(&self, family: &str, style: FontStyle) -> Option<&FontFace> {
        let candidates = [
            style,
            style.without_italic(),
            style.without_bold(),
            FontStyle::Regular,
        ];
        candidates
            .into_iter()
            .find_map(|s| self.find(family, s))
            // Совсем без начертания — просто первый файл семейства.
            .or_else(|| {
                self.faces
                    .iter()
                    .find(|f| f.family.eq_ignore_ascii_case(family))
            })
    }
}

/// Каталоги, где ОС обычно держит шрифты.
pub fn system_font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(windows) {
        if let Some(win) = std::env::var_os("WINDIR") {
            dirs.push(PathBuf::from(win).join("Fonts"));
        } else {
            dirs.push(PathBuf::from(r"C:\Windows\Fonts"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join(r"Microsoft\Windows\Fonts"));
        }
    } else if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.push(PathBuf::from("/Library/Fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Fonts"));
        }
    } else {
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            dirs.push(home.join(".local/share/fonts"));
            dirs.push(home.join(".fonts"));
        }
    }
    dirs.retain(|d| d.is_dir());
    dirs
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<FontFace>) {
    if depth > MAX_DEPTH || out.len() >= MAX_FONTS {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        if out.len() >= MAX_FONTS {
            return;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            subdirs.push(path);
            continue;
        }
        if !is_font_file(&path) {
            continue;
        }
        if entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_FONT_BYTES {
            continue;
        }
        if let Some(face) = read_face_file(&path) {
            out.push(face);
        }
    }
    for sub in subdirs {
        walk(&sub, depth + 1, out);
    }
}

fn is_font_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("ttf") | Some("otf")
    )
}

/// Прочитать файл и вытащить из него имя семейства и начертание.
pub fn read_face_file(path: &Path) -> Option<FontFace> {
    let data = std::fs::read(path).ok()?;
    let names = read_face_names(&data)?;
    Some(FontFace {
        family: names.family,
        style: names.style,
        path: path.to_path_buf(),
    })
}

/// Имена из таблицы `name`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaceNames {
    pub family: String,
    pub style: FontStyle,
}

/// Разобрать таблицу `name` шрифта (первое лицо коллекции).
///
/// Поддерживаются платформы Unicode (0) и Windows (3) — этого достаточно
/// для любых `.ttf`/`.otf`, встречающихся в системных каталогах.
pub fn read_face_names(data: &[u8]) -> Option<FaceNames> {
    if data.len() < 12 {
        return None;
    }
    let num_tables = be_u16(data, 4)? as usize;
    let mut name_offset = None;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        let tag = data.get(rec..rec + 4)?;
        if tag == b"name" {
            name_offset = Some(be_u32(data, rec + 8)? as usize);
            break;
        }
    }
    let table = name_offset?;
    let count = be_u16(data, table + 2)? as usize;
    let storage = table
        .checked_add(be_u16(data, table + 4)? as usize)
        .filter(|&s| s <= data.len())?;

    let mut family: Option<String> = None;
    let mut typographic_family: Option<String> = None;
    let mut subfamily: Option<String> = None;
    let mut typographic_subfamily: Option<String> = None;

    for i in 0..count {
        let rec = table + 6 + i * 12;
        let Some(platform) = be_u16(data, rec) else {
            break;
        };
        let encoding = be_u16(data, rec + 2)?;
        let language = be_u16(data, rec + 4).unwrap_or(0);
        let name_id = be_u16(data, rec + 6)?;
        let len = be_u16(data, rec + 8)? as usize;
        let off = be_u16(data, rec + 10)? as usize;
        // Английский Windows — приоритет; иначе любой Unicode/Windows.
        let english = platform == 3 && language & 0x3FF == 0x009;
        let unicode = platform == 0 || (platform == 3 && (encoding == 1 || encoding == 10));
        if !unicode {
            continue;
        }
        let start = storage.checked_add(off)?;
        let end = start.checked_add(len)?;
        let raw = data.get(start..end)?;
        // Windows-записи Mac-платформы (1) не трогаем: это MacRoman.
        let Some(text) = decode_utf16be(raw) else {
            continue;
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        match name_id {
            16 if english || typographic_family.is_none() => typographic_family = Some(text),
            1 if english || family.is_none() => family = Some(text),
            17 if english || typographic_subfamily.is_none() => typographic_subfamily = Some(text),
            2 if english || subfamily.is_none() => subfamily = Some(text),
            _ => {}
        }
    }

    let family = typographic_family.or(family)?;
    let sub = typographic_subfamily.or(subfamily).unwrap_or_default();
    let sub_lc = sub.to_ascii_lowercase();
    let bold = sub_lc.contains("bold") || sub_lc.contains("black") || sub_lc.contains("heavy");
    let italic = sub_lc.contains("italic") || sub_lc.contains("oblique");
    let style = match (bold, italic) {
        (true, true) => FontStyle::BoldItalic,
        (true, false) => FontStyle::Bold,
        (false, true) => FontStyle::Italic,
        (false, false) => FontStyle::Regular,
    };
    Some(FaceNames { family, style })
}

fn decode_utf16be(raw: &[u8]) -> Option<String> {
    if raw.len() % 2 != 0 {
        return None;
    }
    let mut units = Vec::with_capacity(raw.len() / 2);
    for pair in raw.chunks(2) {
        units.push(u16::from_be_bytes([pair[0], pair[1]]));
    }
    String::from_utf16(&units).ok()
}

fn be_u16(data: &[u8], at: usize) -> Option<u16> {
    let b = data.get(at..at + 2)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    let b = data.get(at..at + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Собрать минимальный SFNT с одной таблицей `name`.
    fn synthetic_font(family: &str, subfamily: &str) -> Vec<u8> {
        let encode =
            |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect() };
        let names = [(1u16, encode(family)), (2u16, encode(subfamily))];
        let count = names.len() as u16;
        // Заголовок таблицы name: format(0), count, stringOffset(6 + 12*count).
        let string_offset = 6 + 12 * count;
        let mut records = Vec::new();
        let mut strings = Vec::new();
        for (id, bytes) in &names {
            records.extend_from_slice(&3u16.to_be_bytes()); // platform: Windows
            records.extend_from_slice(&1u16.to_be_bytes()); // encoding: Unicode BMP
            records.extend_from_slice(&0x0409u16.to_be_bytes()); // language: en-US
            records.extend_from_slice(&id.to_be_bytes());
            records.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            records.extend_from_slice(&(strings.len() as u16).to_be_bytes());
            strings.extend_from_slice(bytes);
        }
        let mut table = Vec::new();
        table.extend_from_slice(&0u16.to_be_bytes()); // format
        table.extend_from_slice(&count.to_be_bytes());
        table.extend_from_slice(&string_offset.to_be_bytes());
        table.extend_from_slice(&records);
        table.extend_from_slice(&strings);

        let table_at = 12 + 16; // один table record
        let mut font = Vec::new();
        font.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // sfnt version
        font.extend_from_slice(&1u16.to_be_bytes()); // numTables
        font.extend_from_slice(&[0; 6]); // searchRange/entrySelector/rangeShift
        font.extend_from_slice(b"name");
        font.extend_from_slice(&0u32.to_be_bytes()); // checksum
        font.extend_from_slice(&(table_at as u32).to_be_bytes());
        font.extend_from_slice(&(table.len() as u32).to_be_bytes());
        font.extend_from_slice(&table);
        font
    }

    #[test]
    fn reads_family_and_style_from_the_name_table() {
        let font = synthetic_font("DejaVu Sans", "Bold Italic");
        let names = read_face_names(&font).expect("имена должны разобраться");
        assert_eq!(names.family, "DejaVu Sans");
        assert_eq!(names.style, FontStyle::BoldItalic);
    }

    #[test]
    fn regular_style_is_the_default() {
        let names = read_face_names(&synthetic_font("Liberation Serif", "Regular")).unwrap();
        assert_eq!(names.family, "Liberation Serif");
        assert_eq!(names.style, FontStyle::Regular);
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        assert!(read_face_names(&[]).is_none());
        assert!(read_face_names(&[0, 1, 0, 0]).is_none());
        assert!(read_face_names(&[0xFF; 64]).is_none());
        // Заголовок есть, таблицы name нет.
        let mut font = vec![0u8; 12];
        font[4..6].copy_from_slice(&1u16.to_be_bytes());
        font.extend_from_slice(b"head");
        font.extend_from_slice(&[0; 12]);
        assert!(read_face_names(&font).is_none());
    }

    #[test]
    fn catalog_finds_closest_style() {
        let catalog = FontCatalog {
            faces: vec![
                FontFace {
                    family: "Test".into(),
                    style: FontStyle::Regular,
                    path: "regular.ttf".into(),
                },
                FontFace {
                    family: "Test".into(),
                    style: FontStyle::Bold,
                    path: "bold.ttf".into(),
                },
            ],
        };
        assert_eq!(
            catalog.find("Test", FontStyle::Bold).unwrap().path,
            PathBuf::from("bold.ttf")
        );
        // Жирного курсива нет — берём жирный.
        assert_eq!(
            catalog.best("Test", FontStyle::BoldItalic).unwrap().path,
            PathBuf::from("bold.ttf")
        );
        assert_eq!(catalog.families(), vec!["Test".to_string()]);
    }

    #[test]
    fn font_directories_exist_on_this_platform() {
        for dir in system_font_dirs() {
            assert!(dir.is_dir(), "каталог шрифтов должен существовать: {dir:?}");
        }
    }
}
