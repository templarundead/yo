//! # `yo_core` — ядро ёфикации
//!
//! Порт логики классической программы **YOL** («Ё лайт», Delphi 7) на Rust.
//!
//! Здесь находится всё, что не связано с графикой:
//!
//! * [`Dictionary`] — база слов на «Ё» (`yox.dat`), бинарный поиск по «е-форме»;
//! * [`RegexDict`]  — словарь регулярных выражений (`yo_regexpr.dat`) с тем же
//!   преобразованием шаблонов, что делали `TRegExprDic.AddWord` /
//!   `MakeValidRegExpr`;
//! * [`Engine`]     — конечный автомат обхода текста (аналог `TMainForm.Yoficate`
//!   + `CheckWord` + `CheckWordEx` + `SelectWord`);
//! * [`Stats`]      — статистика в формате строки состояния оригинала.
//!
//! Обработка всегда идёт через [`Sink`], поэтому один и тот же движок
//! обслуживает и GUI (события через `mpsc`, GUI никогда не блокируется),
//! и консольный потоковый режим для файлов в сотни мегабайт.
//!
//! ## Встроенные ресурсы
//!
//! ```text
//! const DICTIONARY_BYTES: &[u8] = include_bytes!("../assets/yox.dat");
//! const REGEXPR_BYTES:    &[u8] = include_bytes!("../assets/yo_regexpr.dat");
//! ```
//!
//! Никакого обращения к диску во время работы — всё уже внутри исполняемого
//! файла. Обе базы хранятся в CP1251 (как и в оригинале) и один раз
//! перекодируются в UTF-8 при первом обращении.

#![allow(dead_code)]

use std::cmp::Ordering;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering as AtomicOrdering};
use std::sync::OnceLock;
use std::time::Duration;

use regex::{Regex, RegexBuilder};

// ═══════════════════════════════════════════════════════════════════════════
//  1. Встроенные словари
// ═══════════════════════════════════════════════════════════════════════════

/// База слов на «Ё» (исходник — `binary/yox.dat`, кодировка CP1251).
pub const DICTIONARY_BYTES: &[u8] = include_bytes!("../assets/yox.dat");

/// Словарь регулярных выражений (исходник — `binary/yo_regexpr.dat`, CP1251).
pub const REGEXPR_BYTES: &[u8] = include_bytes!("../assets/yo_regexpr.dat");

/// Версия программы (дублирует `YoVersion` из `Commons.pas`).
pub const YO_VERSION: &str = "0.984 (light) — Rust/egui";

// ═══════════════════════════════════════════════════════════════════════════
//  2. Кодировка CP1251
// ═══════════════════════════════════════════════════════════════════════════

/// Старшая половина таблицы CP1251 (байты `0x80..=0xFF`).
const CP1251_HIGH: [char; 128] = [
    '\u{0402}', '\u{0403}', '\u{201A}', '\u{0453}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{20AC}', '\u{2030}', '\u{0409}', '\u{2039}', '\u{040A}', '\u{040C}', '\u{040B}', '\u{040F}',
    '\u{0452}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{FFFD}', '\u{2122}', '\u{0459}', '\u{203A}', '\u{045A}', '\u{045C}', '\u{045B}', '\u{045F}',
    '\u{00A0}', '\u{040E}', '\u{045E}', '\u{00A3}', '\u{00A4}', '\u{0490}', '\u{00A6}', '\u{00A7}',
    '\u{0401}', '\u{00A9}', '\u{0404}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{0407}',
    '\u{00B0}', '\u{00B1}', '\u{0406}', '\u{0456}', '\u{0491}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{0451}', '\u{2116}', '\u{0454}', '\u{00BB}', '\u{0458}', '\u{0405}', '\u{0455}', '\u{0457}',
    '\u{0410}', '\u{0411}', '\u{0412}', '\u{0413}', '\u{0414}', '\u{0415}', '\u{0416}', '\u{0417}',
    '\u{0418}', '\u{0419}', '\u{041A}', '\u{041B}', '\u{041C}', '\u{041D}', '\u{041E}', '\u{041F}',
    '\u{0420}', '\u{0421}', '\u{0422}', '\u{0423}', '\u{0424}', '\u{0425}', '\u{0426}', '\u{0427}',
    '\u{0428}', '\u{0429}', '\u{042A}', '\u{042B}', '\u{042C}', '\u{042D}', '\u{042E}', '\u{042F}',
    '\u{0430}', '\u{0431}', '\u{0432}', '\u{0433}', '\u{0434}', '\u{0435}', '\u{0436}', '\u{0437}',
    '\u{0438}', '\u{0439}', '\u{043A}', '\u{043B}', '\u{043C}', '\u{043D}', '\u{043E}', '\u{043F}',
    '\u{0440}', '\u{0441}', '\u{0442}', '\u{0443}', '\u{0444}', '\u{0445}', '\u{0446}', '\u{0447}',
    '\u{0448}', '\u{0449}', '\u{044A}', '\u{044B}', '\u{044C}', '\u{044D}', '\u{044E}', '\u{044F}',
];

/// Один байт CP1251 → `char`.
#[inline]
pub fn cp1251_char(b: u8) -> char {
    if b < 0x80 {
        b as char
    } else {
        CP1251_HIGH[(b - 0x80) as usize]
    }
}

/// Перекодировать срез CP1251 в UTF-8. Один проход, без лишних аллокаций:
/// ~2 мс на полную базу в 1.8 МБ.
pub fn decode_cp1251(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 8);
    for &b in bytes {
        out.push(cp1251_char(b));
    }
    out
}

/// Перекодировать строку UTF-8 в CP1251 (для сохранения файлов).
pub fn encode_cp1251(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let u = c as u32;
        let b = match c {
            '\u{0}'..='\u{7F}' => u as u8,
            '\u{410}'..='\u{42F}' => (u - 0x410 + 0xC0) as u8,
            '\u{430}'..='\u{44F}' => (u - 0x430 + 0xE0) as u8,
            '\u{401}' => 0xA8,
            '\u{451}' => 0xB8,
            _ => CP1251_HIGH
                .iter()
                .position(|&x| x == c)
                .map(|i| (i + 0x80) as u8)
                .unwrap_or(b'?'),
        };
        out.push(b);
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════════
//  3. Буквенные примитивы (аналоги функций из Main.pas)
// ═══════════════════════════════════════════════════════════════════════════

/// `IsLower`: строчная русская буква.
#[inline]
pub fn is_lower(c: char) -> bool {
    matches!(c, 'а'..='я' | 'ё')
}

/// `IsUpper`: прописная русская буква.
#[inline]
pub fn is_upper(c: char) -> bool {
    matches!(c, 'А'..='Я' | 'Ё')
}

/// `IsAlpha`: **русская** буква. Именно она, а не любой алфавитный символ,
/// ограничивает слово — важная деталь оригинала: латиница и цифры служат
/// разделителями.
#[inline]
pub fn is_alpha(c: char) -> bool {
    is_lower(c) || is_upper(c)
}

#[inline]
pub fn is_ye(c: char) -> bool {
    c == 'е' || c == 'Е'
}

#[inline]
pub fn is_yo(c: char) -> bool {
    c == 'ё' || c == 'Ё'
}

#[inline]
pub fn is_ye_yo(c: char) -> bool {
    is_ye(c) || is_yo(c)
}

/// `IsYoIn`: есть ли в слове «ё».
#[inline]
pub fn is_yo_in(s: &str) -> bool {
    s.chars().any(is_yo)
}

/// `IsYeYoIn`: есть ли в слове «е» или «ё».
#[inline]
pub fn is_ye_yo_in(s: &str) -> bool {
    s.chars().any(is_ye_yo)
}

/// `IsBlank` по `MySpaceChars = ' '#9#10#13#12#160`.
#[inline]
pub fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0C}' | '\u{A0}')
}

/// Нижний регистр «как `AnsiLowerCase`», но без аллокаций.
#[inline]
pub fn lower_char(c: char) -> char {
    match c {
        'A'..='Z' => (c as u8 + 32) as char,
        'А'..='Я' => char::from_u32(c as u32 + 0x20).unwrap_or(c),
        'Ё' => 'ё',
        _ if c.is_uppercase() => c.to_lowercase().next().unwrap_or(c),
        _ => c,
    }
}

/// Верхний регистр — пара к [`lower_char`].
#[inline]
pub fn upper_char(c: char) -> char {
    match c {
        'a'..='z' => (c as u8 - 32) as char,
        'а'..='я' => char::from_u32(c as u32 - 0x20).unwrap_or(c),
        'ё' => 'Ё',
        _ if c.is_lowercase() => c.to_uppercase().next().unwrap_or(c),
        _ => c,
    }
}

/// `LC()` без аллокации: очистить `out` и записать туда нижний регистр `s`.
#[inline]
pub fn lower_into(s: &str, out: &mut String) {
    out.clear();
    out.reserve(s.len());
    for c in s.chars() {
        out.push(lower_char(c));
    }
}

/// `ToYe()` без аллокации: «ё» → «е».
#[inline]
pub fn to_ye_into(s: &str, out: &mut String) {
    out.clear();
    out.reserve(s.len());
    for c in s.chars() {
        out.push(if c == 'ё' {
            'е'
        } else if c == 'Ё' {
            'Е'
        } else {
            c
        });
    }
}

/// `ToYe()` — удобная обёртка, выделяющая память.
#[inline]
pub fn to_ye(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    to_ye_into(s, &mut out);
    out
}

/// `CopyCase`: перевести `w` в регистр образца `w_case` — только те позиции,
/// где в образце стоит прописная буква (в точности как в оригинале).
pub fn copy_case(w: &str, w_case: &str) -> String {
    let mut out = String::with_capacity(w.len());
    let mut sample = w_case.chars();
    for c in w.chars() {
        let c = match sample.next() {
            Some(sc) if is_upper(sc) => upper_char(c),
            _ => c,
        };
        out.push(c);
    }
    out
}

/// `IsItEllipsis` — после слова идёт «...» или «…».
#[inline]
pub fn is_ellipsis_at(input: &str, pos: usize) -> bool {
    match input.get(pos..) {
        Some(rest) => rest.starts_with("...") || rest.starts_with('\u{2026}'),
        None => false,
    }
}

/// `IsItAbbreviation` — после точки (в `pos` — символ сразу за ней) идёт
/// строчная буква, значит слово могло быть сокращением.
#[inline]
pub fn is_abbreviation_at(input: &str, pos: usize) -> bool {
    for (_, c) in input[pos..].char_indices() {
        if is_lower(c) {
            return true;
        }
        if !is_blank(c) {
            return false;
        }
    }
    false
}

/// `FindBinary` (формат FictionBook): позиция тега `<binary`, без учёта
/// регистра. Поиск по байтам — без аллокации копии в верхнем регистре.
fn find_binary_tag(input: &str) -> usize {
    const PATTERN: &[u8] = b"<binary";
    let b = input.as_bytes();
    if b.len() < PATTERN.len() {
        return b.len();
    }
    'outer: for i in 0..=(b.len() - PATTERN.len()) {
        for (j, &p) in PATTERN.iter().enumerate() {
            let c = b[i + j];
            if c != p && c != p.to_ascii_uppercase() {
                continue 'outer;
            }
        }
        return i;
    }
    b.len()
}

// ═══════════════════════════════════════════════════════════════════════════
//  4. Опции (`TOptions` из OptionsPage.pas + политика для неоднозначных слов)
// ═══════════════════════════════════════════════════════════════════════════

/// Что делать со словом, которое требует подтверждения (в оригинале — кнопки
/// «Е (1)» / «Ё (2)» / «Всегда Е (3)» / «Всегда Ё (4)»).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AmbiguousPolicy {
    /// Ставить «Ё» (поведение по умолчанию для пакетной ёфикации).
    #[default]
    Yo,
    /// Оставить «Е».
    Ye,
    /// Спрашивать — интерактивный режим, панель запроса внизу окна.
    Ask,
}

impl AmbiguousPolicy {
    pub const ALL: [AmbiguousPolicy; 3] = [
        AmbiguousPolicy::Yo,
        AmbiguousPolicy::Ye,
        AmbiguousPolicy::Ask,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AmbiguousPolicy::Yo => "Ставить Ё",
            AmbiguousPolicy::Ye => "Оставить Е",
            AmbiguousPolicy::Ask => "Спрашивать",
        }
    }

    /// Номер варианта, подставляемый вместо ответа пользователя.
    #[inline]
    pub fn focus(self) -> u8 {
        match self {
            AmbiguousPolicy::Yo => 2,
            AmbiguousPolicy::Ye => 1,
            AmbiguousPolicy::Ask => 2,
        }
    }
}

/// Полный набор настроек ёфикации.
#[derive(Clone, Debug, PartialEq)]
pub struct YoOptions {
    // ── вкладка «Ёфикация» ────────────────────────────────────────────────
    /// Использование словаря регулярных выражений (`cbRegExprs`).
    pub reg_exprs: bool,
    /// Только безопасные замены (`cbNoVarOnly`).
    pub no_var_only: bool,
    /// Только интерактивные замены (`cbVarOnly`).
    pub var_only: bool,
    /// Проверять расстановку «ё» (`cbCheckYo`).
    pub check_yo: bool,
    /// Всегда подтверждать замену (`cbAlwaysAsk`).
    pub always_ask: bool,
    /// Подтверждать замену для сокращений (`cbToConfirmAbbr`).
    pub to_confirm_abbr: bool,
    /// Подтверждать замену для имён собственных (`cbToConfirmCap`).
    pub to_confirm_cap: bool,
    /// Подтверждать замену для оборванных слов (`cbToConfirmEllipsis`).
    pub to_confirm_ellipsis: bool,
    /// Предлагать последний вариант замены (`cbProposeLast`).
    pub propose_last: bool,
    /// Учитывать формат FictionBook (`cbFBFormat`).
    pub fb_format: bool,
    /// Быстрая прокрутка (`cbFastScroll`).
    pub fast_scroll: bool,
    /// Количество строк проматки (`eLinesBelow`).
    pub lines_below: usize,
    /// Что делать с неоднозначными словами в пакетном режиме.
    pub ambiguous: AmbiguousPolicy,

    // ── вкладка «Редактор» ────────────────────────────────────────────────
    /// Помечать исправленное (`cbMark`).
    pub mark: bool,
    /// Переносить текст по словам (`cbWordWrap`).
    pub word_wrap: bool,
    /// Автораспознавание Unicode при загрузке (`cbAutoUnicode`).
    pub auto_unicode: bool,
    /// Помнить последний файл (`cbLastFile`).
    pub last_file: bool,
}

impl Default for YoOptions {
    /// Значения повторяют `TMainForm.ReadIni` (defaults Delphi-версии).
    fn default() -> Self {
        Self {
            reg_exprs: true,
            no_var_only: false,
            var_only: false,
            check_yo: true,
            always_ask: false,
            to_confirm_abbr: true,
            to_confirm_cap: false,
            to_confirm_ellipsis: true,
            propose_last: true,
            fb_format: false,
            fast_scroll: false,
            lines_below: 2,
            ambiguous: AmbiguousPolicy::Yo,
            mark: true,
            word_wrap: false,
            auto_unicode: true,
            last_file: true,
        }
    }
}

impl YoOptions {
    /// Интерактивный режим (панель запроса активна).
    #[inline]
    pub fn interactive(&self) -> bool {
        self.always_ask || self.ambiguous == AmbiguousPolicy::Ask
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  5. Статистика (`TLetterStat` + строка состояния)
// ═══════════════════════════════════════════════════════════════════════════

/// Счётчики ёфикации. `Copy` — чтобы их можно было дёшево слать в GUI.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Проверено слов, содержащих е/ё (`t`).
    pub checked: u64,
    /// …из них изменено (`t1`).
    pub checked_changed: u64,
    /// …из них оставлено как есть (`t2`).
    pub checked_not_changed: u64,
    /// Спрошено, то есть неоднозначных (`A`).
    pub asked: u64,
    pub asked_changed: u64,
    pub asked_not_changed: u64,
    /// Сработало регулярных выражений (`RE`).
    pub re: u64,
    pub re_changed: u64,
    pub re_not_changed: u64,
    /// Подходило под запрос, но ни одно регулярное выражение не сработало.
    pub no_re: u64,
    /// Подозрительных «ё» (`E`).
    pub wrong_yo: u64,
    /// Было «е», стало «ё».
    pub e_to_yo: u64,
    /// Было «ё», стало «е».
    pub yo_to_e: u64,
    /// Обработано символов.
    pub chars: u64,
    /// Обработано слов.
    pub words: u64,
    /// Время работы (`T`).
    pub elapsed: Duration,
}

impl Stats {
    /// `REQ` — качество словаря регулярных выражений, в процентах.
    #[inline]
    pub fn re_quality(&self) -> f64 {
        let total = self.re + self.no_re;
        if total == 0 {
            0.0
        } else {
            self.re as f64 * 100.0 / total as f64
        }
    }

    /// `S` — замен в секунду.
    #[inline]
    pub fn speed(&self) -> f64 {
        let t = self.elapsed.as_secs_f64();
        if t <= 0.0 {
            0.0
        } else {
            self.checked_changed as f64 / t
        }
    }

    /// Точная копия формата строки состояния Delphi-версии:
    /// `t=t1+t2  A:a1+a2  RE:r1+r2  REQ:%  E:…  T:…  S:…`
    pub fn classic(&self) -> String {
        format!(
            "{}={}+{}  A:{}={}+{}  RE:{}={}+{}  REQ:{:.1}%  E:{}  T:{:.2}  S:{}",
            self.checked,
            self.checked_changed,
            self.checked_not_changed,
            self.asked,
            self.asked_changed,
            self.asked_not_changed,
            self.re,
            self.re_changed,
            self.re_not_changed,
            self.re_quality(),
            self.wrong_yo,
            self.elapsed.as_secs_f64(),
            self.speed() as u64
        )
    }

    /// Расширенная «человеческая» сводка для GUI.
    pub fn human(&self) -> String {
        format!(
            "символов {} · слов {} · заменено {} · ё →е {} · неоднозначных {} · РВ {} · подозрительных Ё {} · {:.2} с · {:.0} зам/с",
            self.chars,
            self.words,
            self.checked_changed,
            self.yo_to_e,
            self.asked,
            self.re,
            self.wrong_yo,
            self.elapsed.as_secs_f64(),
            self.speed()
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  6. Словарь (`TDic` из Main.pas)
// ═══════════════════════════════════════════════════════════════════════════

/// База слов на «Ё».
///
/// Хранит «ё-форму» (`yo`), «е-форму» (`ye`) и заранее посчитанный ключ
/// поиска `lc = LC(ye)` — поэтому в горячем цикле обращения к словарю не
/// делают ни одной аллокации и ни одного приведения регистра.
pub struct Dictionary {
    ye: Vec<String>,
    lc: Vec<String>,
    yo: Vec<String>,
    ask: Vec<bool>,
    /// `WordsAnswer` — последний выбор пользователя (1 = «Е», 2 = «Ё»).
    answer: Vec<AtomicU8>,
    /// `UserSel` — «всегда Е» / «всегда Ё».
    user_sel: Vec<AtomicU8>,
    version: String,
}

impl Default for Dictionary {
    fn default() -> Self {
        Self {
            ye: Vec::new(),
            lc: Vec::new(),
            yo: Vec::new(),
            ask: Vec::new(),
            answer: Vec::new(),
            user_sel: Vec::new(),
            version: String::new(),
        }
    }
}

impl Dictionary {
    /// Разбор базы в формате оригинала:
    /// * строки, начинающиеся с `#` — комментарии (первая строка с «Версия:»
    ///   даёт версию базы);
    /// * `слово`  — однозначная замена;
    /// * `слово?` — замена требует подтверждения (`WordsAsk`).
    pub fn parse(bytes: &[u8]) -> Self {
        let text = decode_cp1251(bytes);
        let capacity = text.len() / 14; // грубая оценка: ~14 байт на слово
        let mut d = Self {
            ye: Vec::with_capacity(capacity),
            lc: Vec::with_capacity(capacity),
            yo: Vec::with_capacity(capacity),
            ask: Vec::with_capacity(capacity),
            answer: Vec::with_capacity(capacity),
            user_sel: Vec::with_capacity(capacity),
            version: String::new(),
        };
        let mut ye_buf = String::with_capacity(32);
        let mut lc_buf = String::with_capacity(32);

        for raw in text.lines() {
            let line = raw.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                if d.version.is_empty() {
                    if let Some(v) = parse_version(line) {
                        d.version = v;
                    }
                }
                continue;
            }
            let (word, ask) = match line.strip_suffix('?') {
                Some(w) => (w, true),
                None => (line, false),
            };
            if word.is_empty() {
                continue;
            }
            to_ye_into(word, &mut ye_buf);
            lower_into(&ye_buf, &mut lc_buf);
            d.ye.push(ye_buf.clone());
            d.lc.push(lc_buf.clone());
            d.yo.push(word.to_owned());
            d.ask.push(ask);
            d.answer.push(AtomicU8::new(0));
            d.user_sel.push(AtomicU8::new(0));
        }
        d.ye.shrink_to_fit();
        d.lc.shrink_to_fit();
        d.yo.shrink_to_fit();

        // Оригинал полагается на то, что файл отсортирован по `Cmp()`.
        // Проверяем это один раз (O(n), ~2 мс) и платим за сортировку только
        // если подсунули стороннюю базу в другом порядке.
        if !d.is_sorted() {
            d.sort();
        }
        d
    }

    pub fn is_sorted(&self) -> bool {
        self.lc
            .windows(2)
            .enumerate()
            .all(|(i, w)| match w[0].as_str().cmp(w[1].as_str()) {
                Ordering::Less => true,
                Ordering::Equal => self.ye[i].as_str() <= self.ye[i + 1].as_str(),
                Ordering::Greater => false,
            })
    }

    /// Стабильная перестановка всех параллельных массивов по ключу `Cmp`.
    fn sort(&mut self) {
        let n = self.len();
        let mut order: Vec<u32> = (0..n as u32).collect();
        order.sort_by(|&a, &b| {
            let (a, b) = (a as usize, b as usize);
            self.lc[a]
                .as_str()
                .cmp(self.lc[b].as_str())
                .then_with(|| self.ye[a].as_str().cmp(self.ye[b].as_str()))
        });
        let idx = |i: usize| order[i] as usize;

        let reorder_strings = |src: &mut Vec<String>| {
            let old = std::mem::take(src);
            *src = (0..n).map(|i| old[idx(i)].clone()).collect();
        };
        let reorder_u8 = |src: &mut Vec<AtomicU8>| {
            let old = std::mem::take(src);
            *src = (0..n)
                .map(|i| AtomicU8::new(old[idx(i)].load(AtomicOrdering::Relaxed)))
                .collect();
        };
        reorder_strings(&mut self.ye);
        reorder_strings(&mut self.lc);
        reorder_strings(&mut self.yo);
        let old_ask = std::mem::take(&mut self.ask);
        self.ask = (0..n).map(|i| old_ask[idx(i)]).collect();
        reorder_u8(&mut self.answer);
        reorder_u8(&mut self.user_sel);
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.ye.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ye.is_empty()
    }

    #[inline]
    pub fn version(&self) -> &str {
        &self.version
    }

    #[inline]
    pub fn ye(&self, i: usize) -> &str {
        &self.ye[i]
    }

    #[inline]
    pub fn yo(&self, i: usize) -> &str {
        &self.yo[i]
    }

    #[inline]
    pub fn ask(&self, i: usize) -> bool {
        self.ask[i]
    }

    #[inline]
    pub fn answer(&self, i: usize) -> u8 {
        self.answer[i].load(AtomicOrdering::Relaxed)
    }

    #[inline]
    pub fn set_answer(&self, i: usize, v: u8) {
        self.answer[i].store(v, AtomicOrdering::Relaxed);
    }

    #[inline]
    pub fn user_sel(&self, i: usize) -> u8 {
        self.user_sel[i].load(AtomicOrdering::Relaxed)
    }

    #[inline]
    pub fn set_user_sel(&self, i: usize, v: u8) {
        self.user_sel[i].store(v, AtomicOrdering::Relaxed);
    }

    /// Бинарный поиск по функции `Cmp` оригинала: сначала сравниваются нижние
    /// регистры, при совпадении — точное написание.
    fn find(&self, key: &str, key_lc: &str) -> Option<usize> {
        let n = self.ye.len();
        if n == 0 {
            return None;
        }
        let cmp_at = |i: usize| -> Ordering {
            self.lc[i]
                .as_str()
                .cmp(key_lc)
                .then_with(|| self.ye[i].as_str().cmp(key))
        };
        if cmp_at(0) == Ordering::Equal {
            return Some(0);
        }
        if cmp_at(n - 1) == Ordering::Equal {
            return Some(n - 1);
        }
        let mut lo = 0usize;
        let mut hi = n - 1;
        while hi > lo + 1 {
            let mid = (lo + hi) / 2;
            match cmp_at(mid) {
                Ordering::Equal => return Some(mid),
                Ordering::Greater => hi = mid,
                Ordering::Less => lo = mid,
            }
        }
        None
    }

    /// `FindInStringsUL`: сначала ищем с «правильной» первой буквой
    /// (`RestLowerCase`), затем — целиком в нижнем регистре.
    ///
    /// `buf_a`/`buf_b` — переиспользуемые буферы движка.
    pub fn find_ul(&self, s: &str, buf_a: &mut String, buf_b: &mut String) -> Option<usize> {
        // RestLowerCase: первая буква как есть, остальные — в нижний регистр.
        buf_a.clear();
        buf_a.reserve(s.len());
        let mut it = s.chars();
        if let Some(c) = it.next() {
            buf_a.push(c);
            for c in it {
                buf_a.push(lower_char(c));
            }
        }
        lower_into(buf_a, buf_b);
        if let Some(i) = self.find(buf_a, buf_b) {
            return Some(i);
        }
        // LC(s): приведение к нижнему регистру идемпотентно, поэтому
        // `buf_b` уже содержит LC(LC(s)) — второй проход не нужен.
        lower_into(s, buf_a);
        self.find(buf_a, buf_b)
    }

    /// Сбросить «запомненные пользователем» ответы (меню «Сбросить …»).
    /// Атомарные счётчики, поэтому хватает `&self`.
    pub fn clear_user_sel(&self) {
        for v in self.user_sel.iter() {
            v.store(0, AtomicOrdering::Relaxed);
        }
        for v in self.answer.iter() {
            v.store(0, AtomicOrdering::Relaxed);
        }
    }
}

/// Достать номер версии из строки комментария (`# Версия: 1.502`).
fn parse_version(line: &str) -> Option<String> {
    let idx = line.find("ерсия")?; // «Версия» / «версия»
    let tail = &line[idx + "ерсия".len()..];
    let tail = tail.trim_start_matches([':', ' ', '\t']);
    let end = tail
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(tail.len());
    let v = tail[..end].trim_end_matches('.');
    if v.is_empty() {
        None
    } else {
        Some(v.to_owned())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  7. Словарь регулярных выражений (`TRegExprDic`)
// ═══════════════════════════════════════════════════════════════════════════

/// Одно правило: исходная строка `w0`, развёрнутый шаблон и скомпилированный
/// [`Regex`].
pub struct RegexRule {
    /// Исходная строка из `yo_regexpr.dat` — источник «правильных» е/ё.
    pub w0: String,
    /// Шаблон после `MakeValidRegExpr`.
    pub pattern: String,
    /// Скомпилированное выражение.
    pub re: Regex,
    /// Позиции (в символах) букв е/ё в `w0` — таблица `r` из `CalcYeYo`.
    pub ye_yo_pos: Vec<usize>,
    /// Сколько раз правило сработало.
    pub hits: AtomicU32,
}

/// Весь словарь регулярных выражений.
pub struct RegexDict {
    rules: Vec<RegexRule>,
    version: String,
    invalid: Vec<String>,
}

impl RegexDict {
    pub fn parse(bytes: &[u8]) -> Self {
        let text = decode_cp1251(bytes);
        let mut rules = Vec::new();
        let mut invalid = Vec::new();
        let mut version = String::new();

        for raw in text.lines() {
            let line = raw.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                if version.is_empty() {
                    if let Some(v) = parse_version(line) {
                        version = v;
                    }
                }
                continue;
            }
            let pattern = make_valid_regexpr(line);
            // ModifierI := not IsAnyUpperCase(RegStr0)
            let ci = !line.chars().any(is_upper);
            let full = if ci {
                format!("(?ims){pattern}")
            } else {
                format!("(?ms){pattern}")
            };
            // Контексты короткие, поэтому детерминизированный движок здесь
            // выигрывает; лимит страхует от раздутия.
            match RegexBuilder::new(&full).size_limit(8 << 20).build() {
                Ok(re) => {
                    let ye_yo_pos = line
                        .chars()
                        .enumerate()
                        .filter(|(_, c)| is_ye_yo(*c))
                        .map(|(i, _)| i)
                        .collect();
                    rules.push(RegexRule {
                        w0: line.to_owned(),
                        pattern,
                        re,
                        ye_yo_pos,
                        hits: AtomicU32::new(0),
                    })
                }
                Err(_) => invalid.push(line.to_owned()),
            }
        }

        Self {
            rules,
            version,
            invalid,
        }
    }

    #[inline]
    pub fn rules(&self) -> &[RegexRule] {
        &self.rules
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    #[inline]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Правила, которые не удалось скомпилировать.
    #[inline]
    pub fn invalid(&self) -> &[String] {
        &self.invalid
    }
}

/// `MakeValidRegExpr` — разворачивает «человеческий» шаблон из
/// `yo_regexpr.dat` в настоящее регулярное выражение:
///
/// * пробелы → `\s+`;
/// * `е`/`ё` → группа `([ёе])` — группа нужна, чтобы потом подставить в текст
///   именно ту букву, которая записана в словаре;
/// * `-` → `-(\r\n){0,1}` (перенос внутри слова);
/// * по краям, если там буквы, добавляются `\b`;
/// * содержимое `(...)`, `[...]` не трогается (счётчик скобок).
pub fn make_valid_regexpr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    let mut was_blank = false;
    let mut was_backslash = false;
    let mut brackets: i32 = 0;

    for c in s.chars() {
        if is_blank(c) {
            if !was_blank {
                out.push_str("\\s+");
            } else {
                was_blank = true; // сохранённое поведение оригинала
            }
        } else {
            was_blank = false;
            if !was_backslash && (c == '(' || c == '[') {
                brackets += 1;
            } else if !was_backslash && (c == ')' || c == ']') {
                brackets -= 1;
            }
            if brackets == 0 {
                if is_ye_yo(c) {
                    if is_upper(c) {
                        out.push_str("([ЁЕ])");
                    } else {
                        out.push_str("([ёе])");
                    }
                } else if c == '-' {
                    out.push_str("-(\\r\\n){0,1}");
                } else {
                    out.push(c);
                }
            } else {
                out.push(c);
            }
        }
        was_backslash = c == '\\';
    }

    if !out.is_empty() && out.starts_with(is_alpha) {
        out.insert_str(0, "\\b");
    }
    if !out.is_empty() && out.ends_with(is_alpha) {
        out.push_str("\\b");
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════════
//  8. Лениво-глобальные словари (не блокируют старт GUI)
// ═══════════════════════════════════════════════════════════════════════════

static DICT: OnceLock<Dictionary> = OnceLock::new();
static REDIC: OnceLock<RegexDict> = OnceLock::new();

/// Глобальный словарь слов (инициализируется при первом обращении).
pub fn dictionary() -> &'static Dictionary {
    DICT.get_or_init(|| Dictionary::parse(DICTIONARY_BYTES))
}

/// Глобальный словарь регулярных выражений.
pub fn regex_dictionary() -> &'static RegexDict {
    REDIC.get_or_init(|| RegexDict::parse(REGEXPR_BYTES))
}

/// Прогреть словари — вызывать из фонового потока, чтобы GUI не ждал.
pub fn preload() {
    let _ = dictionary();
    let _ = regex_dictionary();
}

/// Сбросить все запомненные ответы «всегда Е» / «всегда Ё».
pub fn clear_user_selections() {
    dictionary().clear_user_sel();
}

/// Сводка по встроенным базам: (слов, версия базы, правил, версия правил).
pub fn database_info() -> (usize, String, usize, String) {
    let d = dictionary();
    let r = regex_dictionary();
    (
        d.len(),
        d.version().to_owned(),
        r.len(),
        r.version().to_owned(),
    )
}

// ═══════════════════════════════════════════════════════════════════════════
//  9. Канал наружу: `Sink`
// ═══════════════════════════════════════════════════════════════════════════

/// Ответ пользователя на запрос о замене.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AskDecision {
    /// Оставить «Е» — кнопка «Е (1)».
    Ye,
    /// Поставить «Ё» — кнопка «Ё (2)».
    Yo,
    /// «Всегда Е (3)» — запомнить выбор для этого слова.
    YeAll,
    /// «Всегда Ё (4)» — запомнить выбор для этого слова.
    YoAll,
    /// Откатить последнюю замену.
    Undo,
    /// Прервать ёфикацию.
    Cancel,
}

/// Запрос на подтверждение замены (содержимое панели запроса).
#[derive(Clone, Debug)]
pub struct AskRequest {
    /// Вариант с «Е».
    pub word1: String,
    /// Вариант с «Ё».
    pub word2: String,
    /// Контекст слева.
    pub before: String,
    /// Контекст справа.
    pub after: String,
    /// В слове подозрительная «ё».
    pub wrong_yo: bool,
    /// «Ё» в слове заменяется на другую «ё».
    pub replaced_yo: bool,
    /// «Е» заменяется на «ё» без подтверждения.
    pub wrong_ye: bool,
    /// Какой вариант подсветить первым.
    pub focus_first: bool,
}

impl AskRequest {
    /// Готовый текст для панели запроса.
    pub fn context(&self) -> String {
        let mut s = String::with_capacity(self.before.len() + self.word1.len() + self.after.len());
        s.push_str(&self.before);
        s.push_str(&self.word1);
        s.push_str(&self.after);
        s
    }
}

/// Всё, что движок отдаёт наружу. Реализуется и GUI (через `mpsc::Sender`),
/// и консольным режимом (через `BufWriter`).
pub trait Sink {
    /// Очередной кусок готового текста. `base` — смещение в байтах от начала
    /// результата, `marks` — интервалы изменённых букв **относительно `base`**.
    fn chunk(&mut self, base: usize, text: &str, marks: &[(usize, usize)]);

    /// Промежуточная статистика (~15 раз в секунду).
    fn stats(&mut self, stats: Stats);

    /// Запрос подтверждения. Реализация вправе заблокировать поток — движок
    /// будет ждать ответа.
    fn ask(&mut self, req: AskRequest) -> AskDecision;

    /// Откат одной замены: по адресу `offset` надо вернуть `old` вместо `new`.
    fn undo(&mut self, offset: usize, old: &str, new: &str);

    /// Порция журнала замен («было → стало») для показа в интерфейсе.
    /// По умолчанию — пустая операция: движок наполняет журнал, только если
    /// его включил вызывающий код.
    fn journal(&mut self, _entries: &[(String, String)]) {}

    /// Пользователь нажал «Прервать».
    fn is_cancelled(&self) -> bool;
}

/// Заглушка: пишет в никуда (тесты, бенчмарки).
pub struct NullSink;

impl Sink for NullSink {
    #[inline]
    fn chunk(&mut self, _base: usize, _text: &str, _marks: &[(usize, usize)]) {}
    #[inline]
    fn stats(&mut self, _stats: Stats) {}
    #[inline]
    fn ask(&mut self, _req: AskRequest) -> AskDecision {
        AskDecision::Cancel
    }
    #[inline]
    fn undo(&mut self, _offset: usize, _old: &str, _new: &str) {}
    #[inline]
    fn is_cancelled(&self) -> bool {
        false
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  10. Движок
// ═══════════════════════════════════════════════════════════════════════════

/// Маска периодической проверки отмены / отправки статистики.
const CHECK_MASK: u64 = 0xFF;
/// Порог автосброса буфера результата.
const FLUSH_THRESHOLD: usize = 64 * 1024;
/// `WordsBufferLength2 = WordsBufferLength(5) div 2`.
const CONTEXT_WORDS: usize = 2;
/// Ограничения на вспомогательные списки — защита от разрастания памяти на
/// стомегабайтных файлах.
const MAX_MARKS: usize = 250_000;
const MAX_HISTORY: usize = 64;
/// Сколько записей журнала замен набирается между сбросами буфера.
const MAX_JOURNAL_PER_FLUSH: usize = 192;

/// Результат разбора одного слова.
#[derive(Default)]
struct Selection {
    /// Чем заменить (пусто — без изменений).
    new_word: String,
    /// Индекс в словаре, если слово найдено.
    dic_index: Option<usize>,
    /// Задавался ли вопрос пользователю.
    asked: bool,
    /// Подозрительная «ё».
    wrong_yo: bool,
    /// Прервать ёфикацию.
    cancel: bool,
    /// Откатить последнюю замену.
    undo: bool,
}

/// Движок ёфикации. Долгоживущий, переиспользует все буферы.
pub struct Engine {
    opts: YoOptions,
    stats: Stats,

    // ── буфер результата ─────────────────────────────────────────────────
    out: String,
    emitted: usize,
    marks: Vec<(usize, usize)>,
    /// Журнал замен для отката: (смещение в результате, было, стало).
    history: Vec<(usize, String, String)>,
    /// Отправлять ли приёмнику журнал замен (включает GUI).
    want_journal: bool,
    /// Накопитель журнала замен до очередного сброса буфера.
    journal_buf: Vec<(String, String)>,

    // ── переиспользуемые строки горячего пути ────────────────────────────
    word: String,
    b_key: String,
    b_lc: String,
    b_word1: String,
    b_word2: String,
    b_lc1: String,
    b_lc2: String,
    b_ctx: String,
    b_before: String,
    b_after: String,
}

impl Engine {
    pub fn new(opts: YoOptions) -> Self {
        Self {
            opts,
            stats: Stats::default(),
            out: String::with_capacity(FLUSH_THRESHOLD + 8192),
            emitted: 0,
            marks: Vec::new(),
            history: Vec::new(),
            want_journal: false,
            journal_buf: Vec::new(),
            word: String::with_capacity(64),
            b_key: String::with_capacity(64),
            b_lc: String::with_capacity(64),
            b_word1: String::with_capacity(64),
            b_word2: String::with_capacity(64),
            b_lc1: String::with_capacity(64),
            b_lc2: String::with_capacity(64),
            b_ctx: String::with_capacity(256),
            b_before: String::with_capacity(128),
            b_after: String::with_capacity(128),
        }
    }

    #[inline]
    pub fn options(&self) -> &YoOptions {
        &self.opts
    }

    #[inline]
    pub fn options_mut(&mut self) -> &mut YoOptions {
        &mut self.opts
    }

    #[inline]
    pub fn stats(&self) -> Stats {
        self.stats
    }

    // ────────────────────────────────────────────────────────────────────
    //  Главный цикл — аналог TMainForm.Yoficate
    // ────────────────────────────────────────────────────────────────────

    /// Ёфицировать `input`, отдавая результат порциями в `sink`.
    ///
    /// Диалог с пользователем идёт только через `Sink`, буфер сбрасывается
    /// наружу каждые 64 КБ — поэтому память остаётся ограниченной даже на
    /// стомегабайтных файлах, а интерфейс успевает отрисовывать прогресс.
    pub fn process(&mut self, input: &str, sink: &mut dyn Sink) -> Stats {
        let t0 = std::time::Instant::now();

        self.stats = Stats::default();
        self.out.clear();
        self.emitted = 0;
        self.marks.clear();
        self.history.clear();
        self.journal_buf.clear();

        // FictionBook: всё, что после <binary …>, не трогаем.
        let limit = if self.opts.fb_format {
            find_binary_tag(input)
        } else {
            input.len()
        };
        let (head, tail) = input.split_at(limit);

        let mut iter = head.char_indices();
        let mut word_start = 0usize;
        let mut word_len = 0usize;
        let mut word_ye_yo = false;
        let mut word_yo = false;
        let mut counter = 0u64;
        let mut stop_at = head.len();

        while let Some((idx, c)) = iter.next() {
            if is_alpha(c) {
                if self.word.is_empty() {
                    word_start = idx;
                    word_len = 0;
                    word_ye_yo = false;
                    word_yo = false;
                }
                self.word.push(c);
                word_len += 1;
                word_ye_yo |= is_ye_yo(c);
                word_yo |= is_yo(c);
                continue;
            }

            if !self.word.is_empty() {
                let word = std::mem::take(&mut self.word);
                let undo = self.handle_word(
                    &word, word_start, idx, c, word_len, word_ye_yo, word_yo, input, sink,
                );
                self.word = word;
                self.word.clear();
                self.stats.words += 1;
                counter += 1;
                if undo {
                    stop_at = idx;
                    break;
                }
                if counter & CHECK_MASK == 0 {
                    self.stats.elapsed = t0.elapsed();
                    self.flush(sink);
                    sink.stats(self.stats);
                    if sink.is_cancelled() {
                        stop_at = idx;
                        break;
                    }
                }
            }
            self.out.push(c);
            self.stats.chars += 1;
        }

        if stop_at >= head.len() && !self.word.is_empty() {
            // Последнее слово в проверяемой части текста.
            let word = std::mem::take(&mut self.word);
            self.handle_word(
                &word,
                word_start,
                head.len(),
                '\n',
                word_len,
                word_ye_yo,
                word_yo,
                input,
                sink,
            );
            self.word = word;
            self.word.clear();
            self.stats.words += 1;
        } else if stop_at < head.len() {
            // Отмена: добиваем необработанный остаток без изменений, чтобы
            // результат всегда совпадал с исходником по длине.
            self.word.clear();
            self.out.push_str(&head[stop_at..]);
            self.stats.chars += head[stop_at..].chars().count() as u64;
        }

        self.out.push_str(tail);
        self.stats.chars += tail.chars().count() as u64;
        self.stats.elapsed = t0.elapsed();
        self.flush(sink);
        sink.stats(self.stats);
        self.stats
    }

    /// Отправить накопленный буфер наружу.
    fn flush(&mut self, sink: &mut dyn Sink) {
        if !self.journal_buf.is_empty() {
            sink.journal(&self.journal_buf);
            self.journal_buf.clear();
        }
        if self.out.is_empty() {
            return;
        }
        let base = self.emitted;
        // Метки хранятся в абсолютных координатах — переводим в относительные.
        let mut rel: Vec<(usize, usize)> = Vec::with_capacity(self.marks.len());
        for &(s, e) in &self.marks {
            rel.push((s - base, e - base));
        }
        self.marks.clear();
        sink.chunk(base, &self.out, &rel);
        self.emitted += self.out.len();
        self.out.clear();
    }

    /// Обработка одного слова (`CheckWord` + `CheckWordEx`).
    ///
    /// Возвращает `true`, если пользователь откатил замену и надо
    /// остановиться (аналог `ToMakeUndo` в оригинале).
    #[allow(clippy::too_many_arguments)]
    fn handle_word(
        &mut self,
        word: &str,
        word_start: usize,
        word_end: usize,
        terminator: char,
        word_len: usize,
        word_ye_yo: bool,
        word_yo: bool,
        input: &str,
        sink: &mut dyn Sink,
    ) -> bool {
        self.stats.chars += word_len as u64;

        if word_len <= 1 || !word_ye_yo {
            self.out.push_str(word);
            return false;
        }
        if word_yo && !self.opts.check_yo {
            self.out.push_str(word);
            return false;
        }

        let sel = self.select_word(word, word_start, word_end, terminator, input, sink);

        if sel.cancel {
            // «Прервать» (и «Откат», который тоже останавливает прогон):
            // слово остаётся без изменений, обработка прекращается.
            self.out.push_str(word);
            if sel.undo {
                self.apply_undo(sink);
            }
            return true;
        }

        self.stats.checked += 1;
        if sel.new_word.is_empty() {
            self.stats.checked_not_changed += 1;
        } else {
            self.stats.checked_changed += 1;
            if word_yo && !is_yo_in(&sel.new_word) {
                self.stats.yo_to_e += 1;
            } else if !word_yo && is_yo_in(&sel.new_word) {
                self.stats.e_to_yo += 1;
            }
        }
        if sel.asked {
            self.stats.asked += 1;
            if sel.new_word.is_empty() {
                self.stats.asked_not_changed += 1;
            } else {
                self.stats.asked_changed += 1;
            }
        }
        if sel.wrong_yo {
            self.stats.wrong_yo += 1;
        }

        if sel.new_word.is_empty() {
            self.out.push_str(word);
            return false;
        }

        // ChangeLetter — побуквенная замена с пометкой изменённых позиций.
        let offset = self.emitted + self.out.len();
        let mut result = String::with_capacity(sel.new_word.len());
        let mut old_chars = word.chars();
        let mut byte_off = 0usize;
        for new_ch in sel.new_word.chars() {
            let old_ch = old_chars.next();
            let len = new_ch.len_utf8();
            if Some(new_ch) != old_ch && self.opts.mark && self.marks.len() < MAX_MARKS {
                self.marks
                    .push((offset + byte_off, offset + byte_off + len));
            }
            result.push(new_ch);
            byte_off += len;
        }
        if self.history.len() >= MAX_HISTORY {
            self.history.remove(0);
        }
        if self.want_journal && self.journal_buf.len() < MAX_JOURNAL_PER_FLUSH {
            self.journal_buf.push((word.to_owned(), result.clone()));
        }
        self.out.push_str(&result);
        self.history.push((offset, word.to_owned(), result));
        false
    }

    /// Включить/выключить журнал замен (его показывает панель «Журнал замен»).
    #[inline]
    pub fn set_journal(&mut self, on: bool) {
        self.want_journal = on;
        if !on {
            self.journal_buf.clear();
        }
    }

    /// Откат последней замены (`MakeUndo`).
    fn apply_undo(&mut self, sink: &mut dyn Sink) {
        let Some((offset, old, new)) = self.history.pop() else {
            return;
        };
        let base = self.emitted;
        if offset >= base {
            let rel = offset - base;
            if rel + new.len() <= self.out.len() && &self.out[rel..rel + new.len()] == new.as_str()
            {
                self.out.replace_range(rel..rel + new.len(), &old);
                self.marks
                    .retain(|&(s, _)| !(s >= offset && s < offset + new.len()));
                self.stats.checked_changed = self.stats.checked_changed.saturating_sub(1);
                self.stats.checked_not_changed += 1;
                return;
            }
        }
        // Замена уже ушла наружу — просим приёмник поправить показанный текст.
        sink.undo(offset, &old, &new);
        self.stats.checked_changed = self.stats.checked_changed.saturating_sub(1);
        self.stats.checked_not_changed += 1;
    }

    // ────────────────────────────────────────────────────────────────────
    //  SelectWord — точный порт конечного автомата оригинала
    // ────────────────────────────────────────────────────────────────────

    fn select_word(
        &mut self,
        c_word: &str,
        word_start: usize,
        word_end: usize,
        terminator: char,
        input: &str,
        sink: &mut dyn Sink,
    ) -> Selection {
        let mut sel = Selection::default();
        let dic = dictionary();

        // Word1 := ToYe(cWord)
        to_ye_into(c_word, &mut self.b_word1);

        // FindReplacement
        let (dic_index, mut to_ask, mut focus, always) =
            match dic.find_ul(&self.b_word1, &mut self.b_key, &mut self.b_lc) {
                Some(i) => {
                    sel.dic_index = Some(i);
                    let remembered = dic.user_sel(i);
                    if remembered != 0 {
                        (Some(i), false, remembered, true)
                    } else {
                        let ask = dic.ask(i);
                        (Some(i), ask, if ask { dic.answer(i) } else { 2 }, false)
                    }
                }
                None => (None, false, 1u8, false),
            };
        let has_w2 = dic_index.is_some();

        self.b_word2.clear();
        if let Some(i) = dic_index {
            self.b_word2.push_str(dic.yo(i));
        }

        // ToCheckAbbreviation / ToCheckEllipsis (аналог CheckWordEx)
        let to_check_abbr = has_w2
            && self.opts.to_confirm_abbr
            && terminator == '.'
            && is_abbreviation_at(input, word_end + terminator.len_utf8());
        let to_check_ell =
            has_w2 && self.opts.to_confirm_ellipsis && is_ellipsis_at(input, word_end);

        to_ask = to_ask
            || (has_w2 && to_check_abbr)
            || (has_w2 && to_check_ell)
            || (has_w2
                && self.opts.to_confirm_cap
                && c_word.chars().next().map_or(false, is_upper)
                && !always);

        let is_yo_c_word = is_yo_in(c_word);

        lower_into(&self.b_word2, &mut self.b_lc1);
        lower_into(c_word, &mut self.b_lc2);
        let lc_equal = self.b_lc1 == self.b_lc2;

        let wrong_yo = is_yo_c_word && !lc_equal;
        let replaced_yo = wrong_yo && has_w2;
        let wrong_ye = !is_yo_c_word && lc_equal && !to_ask && focus == 2;
        sel.wrong_yo = wrong_yo;

        if replaced_yo {
            let w = copy_case(&self.b_word2, c_word);
            self.b_word1 = w;
        }
        if (!has_w2 && is_yo_c_word) || replaced_yo {
            self.b_word2.clear();
            self.b_word2.push_str(c_word);
        }
        let cased = copy_case(&self.b_word2, c_word);
        self.b_word2 = cased;

        // «Только интерактивные замены»
        if !to_ask && self.opts.var_only && !wrong_yo {
            return sel;
        }

        // Словарь регулярных выражений
        if self.opts.reg_exprs && to_ask {
            let redic = regex_dictionary();
            let di = self.build_context(input, word_start, word_end, c_word);
            let ctx = self.b_ctx.clone();
            let mut word_n = 0u8;
            for rule in redic.rules() {
                let wn = match_regexp(rule, c_word, &self.b_word1, &self.b_word2, &ctx, di);
                if wn != 0 {
                    word_n = wn;
                    rule.hits.fetch_add(1, AtomicOrdering::Relaxed);
                    break;
                }
            }
            if word_n != 0 {
                to_ask = false;
                focus = word_n;
                self.stats.re += 1;
                let matched = (word_n == 1 && c_word == self.b_word2)
                    || (word_n == 2 && c_word == self.b_word1);
                if matched {
                    self.stats.re_changed += 1;
                } else {
                    self.stats.re_not_changed += 1;
                }
            } else {
                self.stats.no_re += 1;
            }
        }

        // «Только безопасные замены»
        if to_ask && self.opts.no_var_only {
            return sel;
        }

        // Неправильное «ё» никогда не меняется без запроса
        if wrong_yo {
            if self.opts.no_var_only {
                return sel;
            }
            to_ask = true;
        }
        if !to_ask && self.opts.always_ask && !self.b_word2.is_empty() {
            to_ask = true;
        }

        if to_ask {
            sel.asked = true;
            if self.opts.interactive() {
                let focus_first = if focus != 0 && self.opts.propose_last && !wrong_yo {
                    focus == 1
                } else {
                    !is_yo_c_word || wrong_yo
                };
                let req = AskRequest {
                    word1: self.b_word1.clone(),
                    word2: self.b_word2.clone(),
                    before: self.b_before.clone(),
                    after: self.b_after.clone(),
                    wrong_yo,
                    replaced_yo,
                    wrong_ye,
                    focus_first,
                };
                match sink.ask(req) {
                    AskDecision::Ye => focus = 1,
                    AskDecision::Yo => focus = 2,
                    AskDecision::YeAll => {
                        focus = 1;
                        if let Some(i) = dic_index {
                            dic.set_user_sel(i, 1);
                        }
                    }
                    AskDecision::YoAll => {
                        focus = 2;
                        if let Some(i) = dic_index {
                            dic.set_user_sel(i, 2);
                        }
                    }
                    AskDecision::Undo => {
                        sel.cancel = true;
                        sel.undo = true;
                    }
                    AskDecision::Cancel => sel.cancel = true,
                }
                if let Some(i) = dic_index {
                    if focus > 0 && focus < 3 {
                        dic.set_answer(i, focus);
                    }
                }
            } else {
                // Пакетный режим: подозрительную «ё» всегда оставляем как
                // есть, для остальных берём политику из настроек.
                focus = if wrong_yo {
                    2
                } else {
                    self.opts.ambiguous.focus()
                };
            }
        }

        if sel.cancel {
            return sel;
        }

        match focus {
            1 => {
                if self.b_word1 != c_word {
                    sel.new_word = self.b_word1.clone();
                }
            }
            2 => {
                if self.b_word2 != c_word {
                    sel.new_word = self.b_word2.clone();
                }
            }
            _ => sel.cancel = true,
        }
        sel
    }

    /// `GetContext` — собирает вокруг слова до [`CONTEXT_WORDS`] слов слева и
    /// справа. Возвращает `di` — количество символов слева от слова.
    fn build_context(
        &mut self,
        input: &str,
        word_start: usize,
        word_end: usize,
        c_word: &str,
    ) -> usize {
        self.b_ctx.clear();
        self.b_before.clear();
        self.b_after.clear();

        // ── назад ───────────────────────────────────────────────────────
        let mut pre: Vec<char> = Vec::with_capacity(96);
        let mut it = input[..word_start].char_indices().rev();
        let mut to_cut = false;
        for n in 1..=CONTEXT_WORDS {
            loop {
                match it.next() {
                    Some((_, c)) => {
                        pre.push(c);
                        if is_alpha(c) {
                            break;
                        }
                    }
                    None => break,
                }
            }
            loop {
                match it.next() {
                    Some((_, c)) => {
                        pre.push(c);
                        if !is_alpha(c) {
                            to_cut = n == CONTEXT_WORDS;
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
        pre.reverse();
        if to_cut && !pre.is_empty() {
            pre.remove(0);
        }
        let di = pre.len();
        self.b_before.extend(pre.iter().copied());
        self.b_ctx.extend(pre.iter().copied());

        // ── само слово ──────────────────────────────────────────────────
        self.b_ctx.push_str(c_word);

        // ── вперёд ──────────────────────────────────────────────────────
        let mut post: Vec<char> = Vec::with_capacity(96);
        let mut it = input[word_end..].char_indices();
        let mut to_cut = false;
        for n in 1..=CONTEXT_WORDS {
            loop {
                match it.next() {
                    Some((_, c)) => {
                        post.push(c);
                        if is_alpha(c) {
                            break;
                        }
                    }
                    None => break,
                }
            }
            loop {
                match it.next() {
                    Some((_, c)) => {
                        post.push(c);
                        if !is_alpha(c) {
                            to_cut = n == CONTEXT_WORDS;
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
        if to_cut {
            post.pop();
        }
        self.b_after.extend(post.iter().copied());
        self.b_ctx.extend(post.iter().copied());
        di
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  11. MatchRegExp — сопоставление с одним правилом
// ═══════════════════════════════════════════════════════════════════════════

/// Порт `MatchRegExp`: подставляет в найденный контекст те е/ё, которые
/// записаны в правиле, и смотрит, получилось `word1` или `word2`.
///
/// Возвращает `1` (вариант с «Е»), `2` (вариант с «Ё») или `0`.
pub fn match_regexp(
    rule: &RegexRule,
    c_word: &str,
    word1: &str,
    word2: &str,
    context: &str,
    di: usize,
) -> u8 {
    let w_len = c_word.chars().count();
    if w_len == 0 {
        return 0;
    }

    for caps in rule.re.captures_iter(context) {
        let mut chars: Vec<char> = context.chars().collect();
        let mut j = 0usize; // индекс в rule.ye_yo_pos
        let mut is_new_word = false;

        for gi in 1..caps.len() {
            let Some(m) = caps.get(gi) else { continue };
            let (s, e) = (m.start(), m.end());
            let Some(slice) = context.get(s..e) else {
                continue;
            };
            let mut slice_chars = slice.chars();
            let first = match slice_chars.next() {
                Some(c) => c,
                None => continue,
            };
            if slice_chars.next().is_some() {
                continue; // MatchLen <> 1 — как в оригинале: пропускаем
            }
            if !is_ye_yo(first) {
                continue;
            }
            // Символьный индекс группы в контексте (0-based).
            let ci = context[..s].chars().count();
            if let Some(&pos) = rule.ye_yo_pos.get(j) {
                if let Some(replacement) = rule.w0.chars().nth(pos) {
                    if let Some(slot) = chars.get_mut(ci) {
                        *slot = replacement;
                    }
                }
            }
            j += 1;
            if ci >= di && ci <= di + w_len - 1 {
                is_new_word = true;
            }
        }

        let substituted: String = chars.into_iter().collect();
        let new_ctx = copy_case(&substituted, context);
        let new_word: String = if is_new_word {
            new_ctx.chars().skip(di).take(w_len).collect()
        } else {
            String::new()
        };
        if new_word == word1 {
            return 1;
        }
        if new_word == word2 {
            return 2;
        }
    }
    0
}

// ═══════════════════════════════════════════════════════════════════════════
//  12. Кодировки файлов (DialogEnc.pas)
// ═══════════════════════════════════════════════════════════════════════════

/// Формат текстового файла.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TextEncoding {
    /// UTF-8 без BOM.
    #[default]
    Utf8,
    /// UTF-8 с BOM.
    Utf8Bom,
    /// UTF-16 little endian.
    Utf16Le,
    /// UTF-16 big endian.
    Utf16Be,
    /// Windows-1251.
    Cp1251,
}

impl TextEncoding {
    pub const ALL: [TextEncoding; 5] = [
        TextEncoding::Utf8,
        TextEncoding::Utf8Bom,
        TextEncoding::Utf16Le,
        TextEncoding::Utf16Be,
        TextEncoding::Cp1251,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TextEncoding::Utf8 => "UTF-8",
            TextEncoding::Utf8Bom => "UTF-8 (BOM)",
            TextEncoding::Utf16Le => "UTF-16 LE",
            TextEncoding::Utf16Be => "UTF-16 BE",
            TextEncoding::Cp1251 => "Windows-1251",
        }
    }
}

/// Распознать кодировку по BOM, иначе — «похоже на UTF-8?» (`AutoUnicode`).
pub fn decode_bytes(bytes: &[u8]) -> (String, TextEncoding) {
    if bytes.len() >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF {
        let s = String::from_utf8_lossy(&bytes[3..]).into_owned();
        return (s, TextEncoding::Utf8Bom);
    }
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let u: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return (String::from_utf16_lossy(&u), TextEncoding::Utf16Le);
    }
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let u: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return (String::from_utf16_lossy(&u), TextEncoding::Utf16Be);
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        (s.to_owned(), TextEncoding::Utf8)
    } else {
        (decode_cp1251(bytes), TextEncoding::Cp1251)
    }
}

/// Закодировать строку в выбранный формат.
pub fn encode_string(s: &str, enc: TextEncoding) -> Vec<u8> {
    match enc {
        TextEncoding::Utf8 => s.as_bytes().to_vec(),
        TextEncoding::Utf8Bom => {
            let mut v = vec![0xEF, 0xBB, 0xBF];
            v.extend_from_slice(s.as_bytes());
            v
        }
        TextEncoding::Utf16Le => {
            let mut v = vec![0xFF, 0xFE];
            for u in s.encode_utf16() {
                v.extend_from_slice(&u.to_le_bytes());
            }
            v
        }
        TextEncoding::Utf16Be => {
            let mut v = vec![0xFE, 0xFF];
            for u in s.encode_utf16() {
                v.extend_from_slice(&u.to_be_bytes());
            }
            v
        }
        TextEncoding::Cp1251 => encode_cp1251(s),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  13. Консольный (потоковый) приёмник — для файлов в сотни мегабайт
// ═══════════════════════════════════════════════════════════════════════════

/// [`Sink`], который пишет результат прямо в любой `std::io::Write`, не
/// держа текста в памяти.
pub struct WriterSink<W: std::io::Write> {
    writer: W,
    quiet: bool,
    last_report: std::time::Instant,
}

impl<W: std::io::Write> WriterSink<W> {
    pub fn new(writer: W, quiet: bool) -> Self {
        Self {
            writer,
            quiet,
            last_report: std::time::Instant::now(),
        }
    }

    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl<W: std::io::Write> Sink for WriterSink<W> {
    fn chunk(&mut self, _base: usize, text: &str, _marks: &[(usize, usize)]) {
        let _ = self.writer.write_all(text.as_bytes());
    }

    fn stats(&mut self, stats: Stats) {
        if self.quiet {
            return;
        }
        let now = std::time::Instant::now();
        if now.duration_since(self.last_report) < Duration::from_millis(200) {
            return;
        }
        self.last_report = now;
        eprint!("\r\x1b[2K  {}", stats.classic());
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }

    fn ask(&mut self, _req: AskRequest) -> AskDecision {
        // В консоли подтверждать некого — берём вариант с «Ё».
        AskDecision::Yo
    }

    fn undo(&mut self, _offset: usize, _old: &str, _new: &str) {}

    fn is_cancelled(&self) -> bool {
        false
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  14. Тесты
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    struct Collector {
        text: String,
    }

    impl Sink for Collector {
        fn chunk(&mut self, _base: usize, text: &str, _marks: &[(usize, usize)]) {
            self.text.push_str(text);
        }
        fn stats(&mut self, _s: Stats) {}
        fn ask(&mut self, _r: AskRequest) -> AskDecision {
            AskDecision::Cancel
        }
        fn undo(&mut self, _o: usize, _old: &str, _new: &str) {}
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    fn run(input: &str, opts: YoOptions) -> (String, Stats) {
        let mut engine = Engine::new(opts);
        let mut sink = Collector {
            text: String::new(),
        };
        let stats = engine.process(input, &mut sink);
        (sink.text, stats)
    }

    #[test]
    fn dictionary_is_loaded_and_sorted() {
        let d = dictionary();
        assert!(d.len() > 100_000, "словарь не загрузился: {}", d.len());
        assert!(!d.version().is_empty());
        assert!(d.is_sorted());
    }

    #[test]
    fn basic_yofication() {
        let (out, stats) = run("Актер вышел на сцену.", YoOptions::default());
        assert_eq!(out, "Актёр вышел на сцену.");
        assert_eq!(stats.checked_changed, 1);
        assert_eq!(stats.e_to_yo, 1);
    }

    #[test]
    fn length_is_preserved() {
        let src = "Все счастливые семьи похожи друг на друга, все несчастливые — по-своему.";
        let (out, _) = run(src, YoOptions::default());
        assert_eq!(src.chars().count(), out.chars().count());
    }

    #[test]
    fn latin_and_digits_are_separators() {
        let (out, _) = run("wordелка42елка", YoOptions::default());
        assert_eq!(out, "wordёлка42ёлка");
    }

    #[test]
    fn check_yo_off_keeps_yo() {
        let mut o = YoOptions::default();
        o.check_yo = false;
        let (out, _) = run("ёлка елка", o);
        assert_eq!(out, "ёлка ёлка");
    }

    #[test]
    fn single_letter_words_are_skipped() {
        let (out, _) = run("е ё а", YoOptions::default());
        assert_eq!(out, "е ё а");
    }

    #[test]
    fn make_valid_regexpr_transforms() {
        assert_eq!(
            make_valid_regexpr("всё-таки"),
            "\\bвс([ёе])-(\\r\\n){0,1}таки\\b"
        );
        assert_eq!(make_valid_regexpr("все, кто"), "\\bвс([ёе]),\\s+кто\\b");
    }

    #[test]
    fn regexp_dictionary_is_compiled() {
        let r = regex_dictionary();
        assert!(r.len() > 50, "правил слишком мало: {}", r.len());
        assert!(
            r.invalid().is_empty(),
            "не скомпилировалось: {:?}",
            r.invalid()
        );
    }

    #[test]
    fn cp1251_roundtrip() {
        let text = "Привет, ёлка! №1 — «Ё»";
        let bytes = encode_cp1251(text);
        assert_eq!(decode_cp1251(&bytes), text);
    }

    #[test]
    fn encoding_detection() {
        let (s, enc) = decode_bytes(&[0xEF, 0xBB, 0xBF, b'a', b'b']);
        assert_eq!(s, "ab");
        assert_eq!(enc, TextEncoding::Utf8Bom);
    }

    #[test]
    fn big_text_is_streamed_in_chunks() {
        let mut src = String::new();
        for _ in 0..20_000 {
            src.push_str("елка береза актер ");
        }
        let (out, stats) = run(&src, YoOptions::default());
        assert_eq!(out.chars().count(), src.chars().count());
        assert!(stats.checked_changed >= 60_000);
    }
}
