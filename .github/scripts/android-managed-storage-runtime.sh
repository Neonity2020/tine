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
run_instrumentation_class() {
  local test_class="$1"
  local instrumentation_output runner_log started finished failed

  # Managed-storage smoke tears down and recreates the native runtime. Safe
  # Back is an independent activity-lifecycle contract. Keeping both in one
  # instrumentation process lets Android's graphics/native teardown from the
  # first class poison the second class with a destroyed-mutex abort.
  adb shell am force-stop page.tine.app || true
  adb shell am force-stop page.tine.app.test || true
  adb logcat -c || true
  instrumentation_output="$(
    adb shell am instrument -w \
      -e class "$test_class" \
      page.tine.app.test/androidx.test.runner.AndroidJUnitRunner
  )"
  printf '%s\n' "$instrumentation_output"

  # `am instrument` reports a successful shell command even when
  # AndroidJUnitRunner prints a failed suite. Treat only the runner's explicit
  # OK summary or complete per-test accounting as evidence.
  runner_log="$(adb logcat -d -s TestRunner:V 2>/dev/null || true)"
  started="$(grep -Eo 'run started: [0-9]+ tests?' <<<"$runner_log" | grep -Eo '[0-9]+' | tail -1)"
  finished="$(grep -c 'TestRunner.*finished: ' <<<"$runner_log" || true)"
  failed="$(grep -c 'TestRunner.*failed: ' <<<"$runner_log" || true)"

  if grep -Fq 'FAILURES!!!' <<<"$instrumentation_output" ||
    { ! grep -Eq 'OK \([0-9]+ tests?\)' <<<"$instrumentation_output" &&
      ! { [[ -n "$started" && "$failed" -eq 0 && "$finished" -eq "$started" ]]; }; }; then
    printf 'Android instrumentation class %s did not pass\n' "$test_class" >&2
    printf 'runner accounting: started=%s finished=%s failed=%s\n' \
      "${started:-none}" "$finished" "$failed" >&2
    printf '\n===== logcat (app, runner, crashes) =====\n' >&2
    adb logcat -d -v time \
      AndroidRuntime:E DEBUG:V Tine:V chromium:E TestRunner:V libc:F '*:S' >&2 || true
    return 1
  fi

  if ! grep -Eq 'OK \([0-9]+ tests?\)' <<<"$instrumentation_output"; then
    printf 'warning: all %s tests in %s finished with no failures, but the process did not exit cleanly\n' \
      "$started" "$test_class" >&2
    adb logcat -d -v time AndroidRuntime:E DEBUG:V libc:F '*:S' >&2 || true
  fi
}

run_instrumentation_class page.tine.app.ManagedStorageSmokeTest
run_instrumentation_class page.tine.app.SafeBackOwnershipTest
