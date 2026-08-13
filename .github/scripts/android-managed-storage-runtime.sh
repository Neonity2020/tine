#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
android_root="$repo_root/src-tauri/gen/android"

"$android_root/gradlew" -p "$android_root" \
  :app:installUniversalDebug :app:installUniversalDebugAndroidTest
adb shell appops set --uid page.tine.app MANAGE_EXTERNAL_STORAGE allow
adb shell am instrument -w page.tine.app.test/androidx.test.runner.AndroidJUnitRunner
