//! Интеграционные тесты встроенных баз данных.
//!
//! Проверяют то, что нельзя проверить на маленьких рукописных строках:
//! целостность и упорядоченность настоящего `yox.dat` (140 324 записи) и
//! `yo_regexpr.dat` (94 правила), вкомпилированных в бинарник.

use yol::yo_core::{database_info, dictionary, regex_dictionary};

/// База слов загружается и не пуста.
#[test]
fn dictionary_loads() {
    let d = dictionary();
    assert!(!d.is_empty(), "словарь слов пуст");
    assert!(
        d.len() > 100_000,
        "в базе подозрительно мало слов: {}",
        d.len()
    );
}

/// Ключи (`е`-формы) строго возрастают под `Cmp` — иначе бинарный поиск врёт.
#[test]
fn dictionary_is_sorted() {
    assert!(
        dictionary().is_sorted(),
        "yox.dat не отсортирован: бинарный поиск давал бы неверные результаты"
    );
}

/// Номер версии вычитывается из шапки файла, а не захардкожен.
#[test]
fn dictionary_has_version() {
    let v = dictionary().version();
    assert!(!v.is_empty(), "версия базы слов не распознана");
    assert!(
        v.chars().next().unwrap().is_ascii_digit(),
        "версия базы слов выглядит странно: {v:?}"
    );
}

/// Сводка, которую GUI показывает в шапке и в окне «О программе»,
/// согласуется с самими базами.
#[test]
fn database_info_is_consistent_with_dictionaries() {
    let (words, dic_version, rules, re_version) = database_info();
    assert_eq!(words, dictionary().len());
    assert_eq!(rules, regex_dictionary().len());
    assert_eq!(dic_version, dictionary().version());
    assert_eq!(re_version, regex_dictionary().version());
}

/// Известные слова находятся и дают ожидаемую `ё`-форму.
///
/// Список короткий и состоит из слов, которые вряд ли исчезнут из базы:
/// это дымовой тест самой базы, а не алгоритма подстановки.
#[test]
fn known_words_are_present() {
    let d = dictionary();
    let mut a = String::new();
    let mut b = String::new();

    for (ye, yo) in [
        ("актер", "актёр"),
        ("береза", "берёза"),
        ("еж", "ёж"),
        ("елка", "ёлка"),
        ("темный", "тёмный"),
        ("легкий", "лёгкий"),
        ("орел", "орёл"),
        ("мед", "мёд"),
    ] {
        let i = d
            .find_ul(ye, &mut a, &mut b)
            .unwrap_or_else(|| panic!("слово {ye:?} не найдено в базе"));
        assert_eq!(d.ye(i), ye, "е-форма для {ye:?}");
        assert_eq!(d.yo(i), yo, "ё-форма для {ye:?}");
        assert!(!d.ask(i), "{ye:?} не должен требовать подтверждения");
    }
}

/// Регистр не влияет на поиск: `RestLowerCase` приводит хвост слова к нижнему.
#[test]
fn lookup_is_case_insensitive_on_the_tail() {
    let d = dictionary();
    let mut a = String::new();
    let mut b = String::new();

    let lower = d.find_ul("актер", &mut a, &mut b).expect("актер");
    let upper = d.find_ul("АКТЕР", &mut a, &mut b).expect("АКТЕР");
    let mixed = d.find_ul("АкТеР", &mut a, &mut b).expect("АкТеР");

    assert_eq!(lower, upper, "поиск должен игнорировать регистр");
    assert_eq!(lower, mixed, "поиск должен игнорировать регистр");
}

/// Слов, которых в русском языке нет, в базе быть не должно.
#[test]
fn unknown_words_are_absent() {
    let d = dictionary();
    let mut a = String::new();
    let mut b = String::new();
    for w in ["крокозябра", "фтыкалка", "ыыыыыы", "zzzzzz", "щщщщщщ"]
    {
        assert!(
            d.find_ul(w, &mut a, &mut b).is_none(),
            "{w:?} внезапно найдено в базе"
        );
    }
}

/// База регулярных выражений: правила есть и все скомпилировались.
#[test]
fn regex_database_compiles_every_rule() {
    let r = regex_dictionary();
    assert!(!r.is_empty(), "словарь регулярных выражений пуст");
    assert!(!r.version().is_empty(), "версия правил не распознана");
    assert!(
        r.invalid().is_empty(),
        "часть правил не скомпилировалась: {:?}",
        r.invalid()
    );
    assert_eq!(r.rules().len(), r.len());
}

/// У каждого правила отмечены позиции букв е/ё в исходной строке `w0`.
///
/// Именно по этим позициям `MatchRegExp` решает, какую букву ставить,
/// поэтому ошибка в таблице `ye_yo_pos` молча ломает всю логику правил.
#[test]
fn regex_rules_mark_yo_positions() {
    for rule in regex_dictionary().rules() {
        assert!(!rule.w0.is_empty(), "у правила пустая исходная строка");
        assert!(!rule.pattern.is_empty(), "у правила пустой шаблон");
        assert!(
            !rule.ye_yo_pos.is_empty(),
            "у правила {:?} не отмечены позиции е/ё",
            rule.w0
        );
        for &p in &rule.ye_yo_pos {
            let c = rule.w0.chars().nth(p).unwrap_or_else(|| {
                panic!("позиция {p} за концом строки {:?}", rule.w0);
            });
            assert!(
                matches!(c, 'е' | 'ё' | 'Е' | 'Ё'),
                "в правиле {:?} позиция {p} указывает на {c:?}, а не на е/ё",
                rule.w0
            );
        }
    }
}

/// Память ответов «всегда Е / всегда Ё» сбрасывается (меню «Сбросить …»).
///
/// Индекс берём с конца базы: там слово «ящерёнок», которое заведомо не
/// встречается в тестовых текстах, — так тест не влияет на остальные,
/// даже когда они идут в параллельных потоках.
#[test]
fn remembered_answers_can_be_cleared() {
    let d = dictionary();
    let i = d.len() - 1;

    d.set_user_sel(i, 2);
    d.set_answer(i, 2);
    assert_ne!(d.user_sel(i), 0, "UserSel не выставился");
    assert_ne!(d.answer(i), 0, "WordsAnswer не выставился");

    d.clear_user_sel();

    assert_eq!(d.user_sel(i), 0, "UserSel не сброшен");
    assert_eq!(d.answer(i), 0, "WordsAnswer не сброшен");
}

/// `preload` — прогрев баз из фонового потока; повторные вызовы безопасны.
#[test]
fn preload_is_idempotent() {
    yol::yo_core::preload();
    yol::yo_core::preload();
    assert!(!dictionary().is_empty());
}
