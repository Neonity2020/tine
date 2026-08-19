#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
android_root="$repo_root/src-tauri/gen/android"
apk_root="$android_root/app/build/outputs/apk"

mapfile -t app_apks < <(find "$apk_root" -type f -name '*.apk' ! -name '*androidTest*' -print)
mapfile -t test_apks < <(find "$apk_root" -type f -name '*androidTest.apk' -print)
if [[ ${#app_apks[@]} -ne 1 || ${#test_apks[@]} -ne 1 ]]; then
  printf 'expected one app APK and one instrumentation APK, found %d and %d\n' \
    "${#app_apks[@]}" "${#test_apks[@]}" >&2
  printf 'APK inventory:\n' >&2
  find "$apk_root" -type f -name '*.apk' -print >&2
  exit 1
fi

# The Rust library was built while Tauri's one-shot build server was alive.
# Installing through Gradle here would try to rebuild every ABI after that
# server has exited. Install the exact, already-built APK pair instead.
adb install -r "${app_apks[0]}"
adb install -r "${test_apks[0]}"
adb shell appops set --uid page.tine.app MANAGE_EXTERNAL_STORAGE allow
# A device-only defect can only be diagnosed from the device's own log, and
# "Process crashed." on its own names nothing. Clear the buffer first so what
# is dumped on failure belongs to this run.
adb logcat -c || true
instrumentation_output="$(
  adb shell am instrument -w page.tine.app.test/androidx.test.runner.AndroidJUnitRunner
)"
printf '%s\n' "$instrumentation_output"

# `am instrument` reports a successful shell command even when AndroidJUnitRunner
# prints a failed test suite.  Treat only the runner's explicit OK summary as
# evidence; otherwise a real app-UID storage failure becomes a green CI job.
runner_log="$(adb logcat -d -s TestRunner:V 2>/dev/null || true)"
started="$(grep -Eo 'run started: [0-9]+ tests?' <<<"$runner_log" | grep -Eo '[0-9]+' | tail -1)"
finished="$(grep -c 'TestRunner.*finished: ' <<<"$runner_log" || true)"
failed="$(grep -c 'TestRunner.*failed: ' <<<"$runner_log" || true)"

# The runner's own per-test lines, not its closing summary, are the evidence.
# The emulator's software-GL stack can abort the app process in libhwui's
# CommonPool during activity teardown — SIGABRT on `hwuiTask1`, zero Tine
# frames, strictly AFTER the last test logged `finished:` — and that kills the
# "OK (N tests)" line the check used to require. A suite where every started
# test finished and none failed passed, whatever happened to the process on
# the way out; a test that never logs `finished:` still fails the count, so an
# in-test crash cannot hide here.
if grep -Fq 'FAILURES!!!' <<<"$instrumentation_output" ||
  { ! grep -Eq 'OK \([0-9]+ tests?\)' <<<"$instrumentation_output" &&
    ! { [[ -n "$started" && "$failed" -eq 0 && "$finished" -eq "$started" ]]; }; }; then
  printf 'Android managed-storage instrumentation did not pass\n' >&2
  printf 'runner accounting: started=%s finished=%s failed=%s\n' \
    "${started:-none}" "$finished" "$failed" >&2
  # The crash trace is the finding. Print the app's and the runner's own lines
  # plus every fatal one, rather than the whole buffer.
  printf '\n===== logcat (app, runner, crashes) =====\n' >&2
  adb logcat -d -v time \
    AndroidRuntime:E \
    DEBUG:V \
    Tine:V \
    chromium:E \
    TestRunner:V \
    libc:F \
    '*:S' >&2 || true
  exit 1
fi

if ! grep -Eq 'OK \([0-9]+ tests?\)' <<<"$instrumentation_output"; then
  printf 'warning: all %s tests finished with no failures, but the process did not exit cleanly\n' \
    "$started" >&2
  adb logcat -d -v time AndroidRuntime:E DEBUG:V libc:F '*:S' >&2 || true
fi
