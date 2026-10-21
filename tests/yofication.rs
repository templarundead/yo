//! Интеграционные тесты движка ёфикации.
//!
//! Проверяют `Engine` целиком — вместе со словарём, словарём регулярных
//! выражений и конечным автоматом `SelectWord` — на уровне «что получилось
//! в тексте» и «что показала статистика».
//!
//! Ожидания сверены с реальной базой `assets/yox.dat`:
//! `актёр`, `берёза`, `ёлка` — обычные (не интерактивные) записи,
//! `всё`/`все` — интерактивная, а `вышел`, `сцену`, `время`, `мама` в базе
//! отсутствуют вовсе.

use yol::yo_core::{AmbiguousPolicy, AskDecision, AskRequest, Engine, Sink, Stats, YoOptions};

// ─────────────────────────────────────────────────────────────────────────────
//  Вспомогательный приёмник
// ─────────────────────────────────────────────────────────────────────────────

/// Собирает результат в памяти и заодно проверяет контракт `Sink`:
/// куски обязаны приходить по порядку и без пропусков.
#[derive(Default)]
struct Collect {
    text: String,
    marks: Vec<(usize, usize)>,
    journal: Vec<(String, String)>,
    chunks: usize,
    undos: usize,
    asks: Vec<AskRequest>,
    cancels_after: Option<usize>,
}

impl Collect {
    fn cancelling_after(n: usize) -> Self {
        Self {
            cancels_after: Some(n),
            ..Default::default()
        }
    }
}

impl Sink for Collect {
    fn chunk(&mut self, base: usize, text: &str, marks: &[(usize, usize)]) {
        let expected = self.text.len();
        assert_eq!(base, expected, "куски должны приходить строго по порядку");
        for &(s, e) in marks {
            self.marks.push((base + s, base + e));
        }
        self.text.push_str(text);
        self.chunks += 1;
    }

    fn stats(&mut self, _stats: Stats) {}

    fn ask(&mut self, req: AskRequest) -> AskDecision {
        self.asks.push(req);
        AskDecision::Yo
    }

    fn undo(&mut self, offset: usize, old: &str, new: &str) {
        self.undos += 1;
        let end = offset + new.len();
        if end <= self.text.len() && &self.text[offset..end] == new {
            self.text.replace_range(offset..end, old);
        }
        self.marks.retain(|&(s, _)| !(s >= offset && s < end));
    }

    fn journal(&mut self, entries: &[(String, String)]) {
        self.journal.extend_from_slice(entries);
    }

    fn is_cancelled(&self) -> bool {
        matches!(self.cancels_after, Some(n) if self.chunks >= n)
    }
}

fn process(input: &str, opts: YoOptions) -> (Collect, Stats) {
    let mut engine = Engine::new(opts);
    engine.set_journal(true);
    let mut sink = Collect::default();
    let stats = engine.process(input, &mut sink);
    (sink, stats)
}

/// То же, но с ёфикацией начиная с байтового смещения (`F9`).
fn process_from(input: &str, from: usize, opts: YoOptions) -> (Collect, Stats) {
    let mut engine = Engine::new(opts);
    engine.set_journal(true);
    let mut sink = Collect::default();
    let stats = engine.process_from(input, from, &mut sink);
    (sink, stats)
}

// ─────────────────────────────────────────────────────────────────────────────
//  Единый буфер («на месте», как единственный RichEdit оригинала)
// ─────────────────────────────────────────────────────────────────────────────

/// Приёмник, повторяющий логику интерфейса с единым буфером: каждая порция
/// применяется к тому же самому `String`, что показывает редактор.
///
/// Так же, как в `YoApp::apply_chunk`: `text.truncate(base)`, порция
/// результата, затем ещё не обработанный остаток исходника. Длина буфера
/// обязана оставаться неизменной — иначе «на месте» ничего не выйдет.
struct InPlace {
    text: String,
    source: String,
    emitted: usize,
    lengths: Vec<usize>,
    marks: Vec<(usize, usize)>,
}

impl InPlace {
    fn new(source: &str) -> Self {
        Self {
            text: source.to_owned(),
            source: source.to_owned(),
            emitted: 0,
            lengths: Vec::new(),
            marks: Vec::new(),
        }
    }
}

impl Sink for InPlace {
    fn chunk(&mut self, base: usize, text: &str, marks: &[(usize, usize)]) {
        assert_eq!(base, self.emitted, "порции должны идти строго по порядку");
        assert!(
            self.text.is_char_boundary(base),
            "base не на границе символа"
        );

        for &(s, e) in marks {
            self.marks.push((base + s, base + e));
        }

        // Ровно то, что делает GUI: обрезаем префикс и переписываем его.
        self.text.truncate(base);
        self.text.push_str(text);
        self.emitted = base + text.len();

        // Возвращаем ещё не обработанный хвост исходника.
        if self.emitted < self.source.len() {
            assert!(
                self.source.is_char_boundary(self.emitted),
                "хвост исходника разрезан посередине символа"
            );
            self.text.push_str(&self.source[self.emitted..]);
        }

        assert_eq!(
            self.text.len(),
            self.source.len(),
            "единый буфер не должен менять длину"
        );
        self.lengths.push(self.text.len());
    }

    fn stats(&mut self, _stats: Stats) {}

    fn ask(&mut self, _req: AskRequest) -> AskDecision {
        AskDecision::Yo
    }

    fn undo(&mut self, offset: usize, old: &str, new: &str) {
        let end = offset + new.len();
        if end <= self.text.len() && &self.text[offset..end] == new {
            self.text.replace_range(offset..end, old);
        }
        self.marks.retain(|&(s, _)| !(s >= offset && s < end));
    }

    fn is_cancelled(&self) -> bool {
        false
    }
}

#[test]
fn single_buffer_is_rewritten_in_place() {
    let src = "Актер вышел на сцену, а береза стояла у елки. ".repeat(2000);
    let mut engine = Engine::new(YoOptions::default());
    let mut sink = InPlace::new(&src);
    let stats = engine.process(&src, &mut sink);

    assert!(sink.lengths.len() > 1, "текст должен приходить порциями");
    assert!(stats.checked_changed > 0);
    assert_eq!(
        sink.text,
        process(&src, YoOptions::default()).0.text,
        "единый буфер обязан совпасть с обычным результатом"
    );
    assert_eq!(sink.text.len(), src.len());
    assert!(
        !sink.marks.is_empty(),
        "изменённые буквы должны быть помечены"
    );
}

#[test]
fn ask_request_points_at_the_word_in_the_single_buffer() {
    let src = "Мама мыла раму, актер вышел";
    let (sink, _) = process(src, opts(|o| o.always_ask = true));

    assert_eq!(sink.asks.len(), 1, "вопросы: {:?}", sink.asks);
    let ask = &sink.asks[0];
    assert_eq!(&src[ask.word_start..ask.word_end], "актер");
    assert_eq!(
        ask.char_range(src),
        Some((16, 21)),
        "GUI выделяет слово по индексам символов"
    );
}

fn opts(mutate: impl FnOnce(&mut YoOptions)) -> YoOptions {
    let mut o = YoOptions::default();
    mutate(&mut o);
    o
}

fn yofy(input: &str) -> String {
    process(input, YoOptions::default()).0.text
}

fn yofy_with(input: &str, mutate: impl FnOnce(&mut YoOptions)) -> String {
    process(input, opts(mutate)).0.text
}

/// Замена е↔ё — единственное, что движок вправе сделать с текстом.
fn assert_only_ye_yo_changed(src: &str, out: &str) {
    assert_eq!(
        out.chars().count(),
        src.chars().count(),
        "длина изменилась: {src:?} -> {out:?}"
    );
    for (a, b) in src.chars().zip(out.chars()) {
        assert!(
            a == b || matches!((a, b), ('е', 'ё') | ('ё', 'е') | ('Е', 'Ё') | ('Ё', 'Е')),
            "недопустимая замена {a:?} -> {b:?} в {src:?} -> {out:?}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Базовая замена
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn replaces_known_words() {
    assert_eq!(yofy("Актер вышел на сцену."), "Актёр вышел на сцену.");
    assert_eq!(yofy("береза"), "берёза");
    assert_eq!(yofy("елка"), "ёлка");
}

#[test]
fn leaves_unknown_words_alone() {
    let src = "крокозябра фтыкалка";
    assert_eq!(yofy(src), src);
}

#[test]
fn latin_text_is_untouched() {
    let src = "The quick brown fox jumps over the lazy dog 42 times.";
    assert_eq!(yofy(src), src);
}

#[test]
fn only_ye_yo_ever_changes() {
    for src in [
        "Актер вышел на сцену, а береза стояла у елки.",
        "Все счастливые семьи похожи друг на друга.",
        "ёлка елка ЁЛКА ЕЛКА",
        "The quick brown fox 42",
    ] {
        assert_only_ye_yo_changed(src, &yofy(src));
    }
}

#[test]
fn empty_input_is_fine() {
    let (sink, stats) = process("", YoOptions::default());
    assert_eq!(sink.text, "");
    assert_eq!(stats.chars, 0);
    assert_eq!(stats.words, 0);
    assert_eq!(stats.checked_changed, 0);
}

#[test]
fn whitespace_only_input_is_fine() {
    let src = "\n\n\t   \r\n";
    assert_eq!(yofy(src), src);
}

// ─────────────────────────────────────────────────────────────────────────────
//  Регистр (`CopyCase`)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn case_style_is_preserved() {
    assert_eq!(yofy("актер"), "актёр", "строчные");
    assert_eq!(yofy("Актер"), "Актёр", "первая прописная");
    assert_eq!(yofy("АКТЕР"), "АКТЁР", "прописные");
}

#[test]
fn mixed_case_words_stay_valid() {
    // Регистр внутри слова может быть каким угодно; важный инвариант —
    // движок всё равно меняет только е/ё и не портит длину.
    for src in ["аКтЕр", "АКТеР", "аКТЕР"] {
        assert_only_ye_yo_changed(src, &yofy(src));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Границы слов
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn punctuation_works_as_a_separator() {
    assert_eq!(
        yofy("елка, елка. елка! (елка) — елка"),
        "ёлка, ёлка. ёлка! (ёлка) — ёлка"
    );
}

#[test]
fn latin_and_digits_are_separators() {
    // Латиница и цифры не считаются буквами (`IsAlpha` покрывает только
    // кириллицу), поэтому «елка» между ними — полноценное слово.
    assert_eq!(yofy("word елка 42"), "word ёлка 42");
}

#[test]
fn single_letter_words_are_skipped() {
    assert_eq!(yofy("е ё а я"), "е ё а я");
}

#[test]
fn words_without_ye_yo_are_skipped() {
    let src = "мама папа дом кот";
    assert_eq!(yofy(src), src);
}

// ─────────────────────────────────────────────────────────────────────────────
//  Флажки вкладки «Ёфикация»
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn check_yo_off_leaves_yo_words_alone() {
    // «ёлка» движок не проверяет вовсе, а «елку» всё равно ёфицирует.
    assert_eq!(yofy_with("ёлка елка", |o| o.check_yo = false), "ёлка ёлка");
}

#[test]
fn only_safe_replacements() {
    // «актер» — безопасная замена, «всё» — нет. Правила выключаем, чтобы
    // проверить именно флажок: без них неоднозначное слово остаётся как есть.
    let out = yofy_with("актер все время", |o| {
        o.no_var_only = true;
        o.reg_exprs = false;
    });
    assert_eq!(out, "актёр все время");
}

#[test]
fn only_interactive_replacements() {
    let out = yofy_with("актер все время", |o| o.var_only = true);
    assert!(
        out.starts_with("актер "),
        "безопасная замена не должна проходить: {out:?}"
    );
}

#[test]
fn always_ask_routes_replacements_through_the_sink() {
    // «вышел» в базе нет, поэтому спрашивают ровно про «актер».
    let (sink, stats) = process("актер вышел", opts(|o| o.always_ask = true));
    assert_eq!(sink.asks.len(), 1, "вопросы: {:?}", sink.asks);
    assert_eq!(sink.asks[0].word1, "актер");
    assert_eq!(sink.asks[0].word2, "актёр");
    assert_eq!(sink.text, "актёр вышел", "приёмник ответил «Ё»");
    assert_eq!(stats.asked, 1);
}

#[test]
fn cancel_stops_the_run() {
    struct Cancel;
    impl Sink for Cancel {
        fn chunk(&mut self, _b: usize, _t: &str, _m: &[(usize, usize)]) {}
        fn stats(&mut self, _s: Stats) {}
        fn ask(&mut self, _r: AskRequest) -> AskDecision {
            AskDecision::Cancel
        }
        fn undo(&mut self, _o: usize, _old: &str, _new: &str) {}
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    let mut sink = Cancel;
    let stats =
        Engine::new(opts(|o| o.always_ask = true)).process("актер вышел на сцену", &mut sink);
    assert_eq!(stats.asked, 1);
    assert!(
        stats.words <= 2,
        "после отмены слов почти не обработано: {}",
        stats.words
    );
}

/// Неоднозначные слова в пакетном режиме разрешаются политикой, без вопросов.
#[test]
fn ambiguous_policy_decides_without_asking() {
    let src = "все актер";
    let ye = process(
        src,
        opts(|o| {
            o.ambiguous = AmbiguousPolicy::Ye;
            o.reg_exprs = false;
        }),
    );
    let yo = process(
        src,
        opts(|o| {
            o.ambiguous = AmbiguousPolicy::Yo;
            o.reg_exprs = false;
        }),
    );

    assert!(
        ye.0.asks.is_empty() && yo.0.asks.is_empty(),
        "пакетный режим вопросов не задаёт"
    );
    assert_eq!(ye.0.text, "все актёр", "политика «оставить Е»");
    assert_eq!(yo.0.text, "всё актёр", "политика «ставить Ё»");
}

// ─────────────────────────────────────────────────────────────────────────────
//  Словарь регулярных выражений
// ─────────────────────────────────────────────────────────────────────────────

/// `MakeValidRegExpr` + `MatchRegExp`: правило «всё время» снимает
/// неоднозначность, которую без него пришлось бы спрашивать у пользователя.
#[test]
fn regexp_rule_resolves_an_ambiguous_word() {
    let mut o = opts(|o| o.ambiguous = AmbiguousPolicy::Ye);
    let (with_sink, with) = process("все время", o.clone());

    o.reg_exprs = false;
    let (without_sink, without) = process("все время", o);

    assert!(
        with.re > 0,
        "правило «всё время» должно сработать — иначе порт MatchRegExp сломан"
    );
    assert_eq!(with_sink.text, "всё время");
    assert_eq!(without.re, 0, "с выключенными правилами их быть не должно");
    assert_eq!(without.no_re, 0);
    assert_eq!(without_sink.text, "все время");
}

// ─────────────────────────────────────────────────────────────────────────────
//  FictionBook
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn fb2_binary_tail_is_not_touched() {
    // NB: латиница и цифры в оригинале — разделители слов, поэтому хвост
    // вида «base64елкаелка» не ёфицируется и без fb2-режима: «елкаелка»
    // в словаре нет. Проверяем на слове, отделённом пробелом.
    let src = "Актер вышел.\n<binary id=\"cover.jpg\">base64ZWxrYQ==</binary>\nелка\n";

    let with_fb = yofy_with(src, |o| o.fb_format = true);
    let without_fb = yofy_with(src, |o| o.fb_format = false);

    assert!(
        with_fb.starts_with("Актёр вышел."),
        "до <binary> текст ёфицируется: {with_fb:?}"
    );
    assert!(
        with_fb.ends_with("\nелка\n"),
        "после <binary> текст не должен меняться: {with_fb:?}"
    );
    assert!(
        without_fb.ends_with("\nёлка\n"),
        "без fb2-режима хвост ёфицируется: {without_fb:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
//  Статистика и пометки
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn stats_count_what_the_gui_shows() {
    let src = "Актер вышел на сцену.";
    let (_, s) = process(src, YoOptions::default());
    // Слова с е/ё длиннее одного символа: «Актер», «вышел», «сцену».
    assert_eq!(s.checked, 3);
    assert_eq!(s.checked_changed, 1);
    assert_eq!(s.checked_not_changed, 2); // «вышел» и «сцену» в базе нет
    assert_eq!(s.e_to_yo, 1);
    assert_eq!(s.yo_to_e, 0);
    assert_eq!(s.chars, src.chars().count() as u64);
    assert!(!s.classic().is_empty(), "строка состояния пустая");
    assert!(
        s.classic().contains("REQ:"),
        "строка состояния: {}",
        s.classic()
    );
}

#[test]
fn marks_point_at_the_changed_letters() {
    let (sink, _) = process("Актер", YoOptions::default());
    assert_eq!(sink.text, "Актёр");
    assert!(!sink.marks.is_empty(), "замена должна быть помечена");
    for &(s, e) in &sink.marks {
        assert!(e > s && e <= sink.text.len(), "кривой интервал ({s}, {e})");
        assert!(sink.text.is_char_boundary(s) && sink.text.is_char_boundary(e));
        assert!(
            sink.text[s..e].contains('ё'),
            "метка ({s}, {e}) указывает на {:?}, а не на ё",
            &sink.text[s..e]
        );
    }
}

#[test]
fn marks_are_disabled_when_marking_is_off() {
    let (sink, _) = process("Актер", opts(|o| o.mark = false));
    assert_eq!(sink.text, "Актёр");
    assert!(
        sink.marks.is_empty(),
        "с выключенной пометкой меток быть не должно"
    );
}

#[test]
fn journal_records_replacements() {
    let (sink, _) = process("Актер береза елка", YoOptions::default());
    assert_eq!(sink.journal.len(), 3, "журнал: {:?}", sink.journal);
    assert!(sink
        .journal
        .iter()
        .any(|(a, b)| a == "Актер" && b == "Актёр"));
    assert!(sink.journal.iter().all(|(a, b)| a != b));
}

// ─────────────────────────────────────────────────────────────────────────────
//  Потоковость и отмена
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn large_text_is_emitted_in_ordered_chunks() {
    let src = "елка ".repeat(40_000); // 200 КБ — заведомо больше одного куска
    let (sink, stats) = process(&src, YoOptions::default());

    assert_eq!(sink.text, "ёлка ".repeat(40_000));
    assert!(sink.chunks > 1, "текст должен был разбиться на куски");
    assert_eq!(stats.chars, src.chars().count() as u64);
    assert_eq!(stats.checked_changed, 40_000);
    assert_eq!(sink.text.chars().count(), src.chars().count());
}

#[test]
fn cancellation_stops_early_but_keeps_the_text_consistent() {
    let src = "елка ".repeat(80_000);

    let mut engine = Engine::new(YoOptions::default());
    let mut sink = Collect::cancelling_after(1);
    let stats = engine.process(&src, &mut sink);

    assert!(
        stats.words < 80_000,
        "после отмены обработано не всё: {} слов из 80 000",
        stats.words
    );
    // Длина всё равно совпадает: необработанный хвост дописывается как есть.
    assert_eq!(sink.text.chars().count(), src.chars().count());
    assert!(sink.text.starts_with("ёлка"));
}

#[test]
fn writer_sink_streams_without_holding_the_text() {
    use yol::yo_core::WriterSink;

    let src = "елка ".repeat(20_000);
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut sink = WriterSink::new(&mut buf, true);
        let stats = Engine::new(YoOptions::default()).process(&src, &mut sink);
        assert_eq!(stats.checked_changed, 20_000);
    }
    let out = String::from_utf8(buf).expect("результат должен быть корректным UTF-8");
    assert_eq!(out, "ёлка ".repeat(20_000));
}

// ─────────────────────────────────────────────────────────────────────────────
//  Ёфикация с заданной позиции (F9 против Ctrl+F9)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn process_from_leaves_the_prefix_untouched() {
    let src = "Актер вышел на сцену. Актер снова вышел.";
    let from = src.find("Актер снова").expect("вторая половина");
    let (sink, _) = process_from(src, from, YoOptions::default());

    // Первая половина не тронута, вторая — ёфицирована.
    assert_eq!(sink.text, "Актер вышел на сцену. Актёр снова вышел.");
    assert_eq!(sink.text.len(), src.len(), "длина обязана сохраниться");
}

#[test]
fn process_from_counts_the_prefix_in_statistics() {
    let src = "Актер вышел на сцену. Актер снова вышел.";
    let from = src.find("Актер снова").expect("вторая половина");
    let (_, stats) = process_from(src, from, YoOptions::default());
    assert_eq!(
        stats.chars,
        src.chars().count() as u64,
        "префикс тоже считается обработанным"
    );
    assert_eq!(stats.checked_changed, 1, "замена только во второй половине");
}

#[test]
fn process_from_snaps_to_the_start_of_the_word() {
    let src = "Актер вышел на сцену.";
    // «ктер» внутри слова: движок обязан начать с «Актер», иначе получилось
    // бы «Актер» → «Актёр» с испорченным обрывком.
    let inside = src.find('к').expect("буква «к»");
    let (sink, _) = process_from(src, inside, YoOptions::default());
    assert_eq!(sink.text, "Актёр вышел на сцену.");
}

#[test]
fn process_from_at_zero_equals_process() {
    let src = "Актер вышел на сцену, а береза стояла у елки.";
    let (a, sa) = process(src, YoOptions::default());
    let (b, sb) = process_from(src, 0, YoOptions::default());
    assert_eq!(a.text, b.text);
    // `elapsed` — единственное поле, которое законно различается.
    assert_eq!(
        Stats {
            elapsed: Default::default(),
            ..sa
        },
        Stats {
            elapsed: Default::default(),
            ..sb
        }
    );
}

#[test]
fn process_from_the_end_changes_nothing() {
    let src = "Актер вышел на сцену.";
    let (sink, stats) = process_from(src, src.len(), YoOptions::default());
    assert_eq!(sink.text, src);
    assert_eq!(stats.checked_changed, 0);
    assert_eq!(stats.chars, src.chars().count() as u64);
}

#[test]
fn process_from_ignores_an_out_of_range_offset() {
    let src = "Актер вышел на сцену.";
    let (sink, _) = process_from(src, src.len() + 100, YoOptions::default());
    assert_eq!(sink.text, src, "смещение за концом — просто весь текст");
}

#[test]
fn custom_databases_are_used_by_the_engine() {
    // Две разные базы: встроенная и «пустая» — движок обязан взять ту,
    // которую передали, а не глобальную.
    let empty = Box::leak(Box::new(yol::yo_core::Dictionary::parse(b"YOX 1.0\n")));
    let re = yol::yo_core::regex_dictionary();
    let mut engine = Engine::with_databases(YoOptions::default(), empty, re);
    let mut sink = Collect::default();
    let stats = engine.process("Актер вышел на сцену.", &mut sink);
    assert_eq!(sink.text, "Актер вышел на сцену.");
    assert_eq!(stats.checked_changed, 0);
}
