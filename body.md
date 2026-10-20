## Диагностика CI (временный workflow)

- ветка: `arena/7f855ed5-yo`, коммит: `9bb30c0`
- fmt: `1` · clippy: `0` · build: `0` · test: `101`

### cargo fmt --all -- --check
```diff
Diff in /home/runner/work/yo/yo/src/main.rs:1687:
         !self.help
             && !self.version
             && !self.gui
-            && (self.input.is_some()
-                || self.output.is_some()
-                || !std::io::stdin().is_terminal())
+            && (self.input.is_some() || self.output.is_some() || !std::io::stdin().is_terminal())
     }
 }
 
Diff in /home/runner/work/yo/yo/tests/cli.rs:243:
         "stderr: {}",
         String::from_utf8_lossy(&out.stderr)
     );
-    assert_eq!(String::from_utf8(out.stdout).expect("stdout не в UTF-8"), "ёлка");
+    assert_eq!(
+        String::from_utf8(out.stdout).expect("stdout не в UTF-8"),
+        "ёлка"
+    );
 }
 
 // ─────────────────────────────────────────────────────────────────────────────
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
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.48s
     Running unittests src/main.rs (target/debug/deps/yol-47a93e640578abb6)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
     Running tests/cli.rs (target/debug/deps/cli-e4fa25aa4e3f858e)
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.93s
     Running tests/dictionary.rs (target/debug/deps/dictionary-2304c42ce4df7243)
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.47s
     Running tests/encoding.rs (target/debug/deps/encoding-48837f063d5c9d52)
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
     Running tests/yofication.rs (target/debug/deps/yofication-6f2a6aef793c1e62)
test cancel_stops_the_run ... FAILED
test fb2_binary_tail_is_not_touched ... FAILED
test cancellation_stops_early_but_keeps_the_text_consistent ... FAILED
test stats_count_what_the_gui_shows ... FAILED
failures:
---- cancel_stops_the_run stdout ----
thread 'cancel_stops_the_run' (4177) panicked at tests/yofication.rs:272:5:
---- fb2_binary_tail_is_not_touched stdout ----
thread 'fb2_binary_tail_is_not_touched' (4182) panicked at tests/yofication.rs:350:5:
---- cancellation_stops_early_but_keeps_the_text_consistent stdout ----
thread 'cancellation_stops_early_but_keeps_the_text_consistent' (4178) panicked at tests/yofication.rs:438:5:
---- stats_count_what_the_gui_shows stdout ----
thread 'stats_count_what_the_gui_shows' (4198) panicked at tests/yofication.rs:365:5:
failures:
test result: FAILED. 23 passed; 4 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.54s
error: test failed, to rerun pass `--test yofication`
   Doc-tests yol
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.34s
error: 1 target failed:
```
