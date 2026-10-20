## Диагностика CI (временный workflow)

- ветка: `arena/7f855ed5-yo`, коммит: `d2ef577`
- fmt: `1` · clippy: `0` · build: `0` · test: `101`

### cargo fmt --all -- --check
```diff
Diff in /home/runner/work/yo/yo/tests/yofication.rs:323:
         "правило «всё время» должно сработать — иначе порт MatchRegExp сломан"
     );
     assert_eq!(with_sink.text, "всё время");
-    assert_eq!(
-        without.re, 0,
-        "с выключенными правилами их быть не должно"
-    );
+    assert_eq!(without.re, 0, "с выключенными правилами их быть не должно");
     assert_eq!(without.no_re, 0);
     assert_eq!(without_sink.text, "все время");
 }
```

### cargo clippy --all-targets --all-features -- -D warnings
```
(пусто)
```

### cargo test --all --no-run
```
(пусто)
```

### cargo test --all
```
     Running unittests src/lib.rs (target/debug/deps/yol-5b545d89ebb0e6e7)
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.31s
     Running unittests src/main.rs (target/debug/deps/yol-47a93e640578abb6)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
     Running tests/cli.rs (target/debug/deps/cli-e4fa25aa4e3f858e)
test stdin_to_stdout_is_yofied ... FAILED
failures:
---- stdin_to_stdout_is_yofied stdout ----
thread 'stdin_to_stdout_is_yofied' (3911) panicked at tests/cli.rs:214:5:
failures:
test result: FAILED. 11 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.39s
error: test failed, to rerun pass `--test cli`
```
