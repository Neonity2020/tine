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
adb shell am instrument -w page.tine.app.test/androidx.test.runner.AndroidJUnitRunner
