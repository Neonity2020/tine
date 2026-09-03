// Android media capture bridge: camera / photo-picker (`capture_photo`) and
// voice-memo recording (`start_recording` / `stop_recording` / `cancel_recording`).
// BOTH return the same shape: a native-cache `path` plus a file `ext`. Captured
// bytes never cross the bridge — Rust streams the path token into the graph
// (`import_native_capture`) before the frontend inserts the media ref, and
// `src/components/Block.tsx` reads `res.path` for a photo exactly as it does for
// a voice memo. `MediaCaptureResult` has no `data` field; `mod tests` pins that.
// Mirrors android_folder_picker.rs. Non-android targets get erroring stubs so the
// desktop build links and the JS calls fail gracefully.
#[cfg(target_os = "android")]
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
#[cfg(target_os = "android")]
use tauri::{
    plugin::{Builder, PluginApi, PluginHandle, TauriPlugin},
    AppHandle, Manager, Runtime, State,
};

#[cfg(target_os = "android")]
const PLUGIN_IDENTIFIER: &str = "page.tine.app";

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct MediaCaptureResult {
    /// "ok" (path + ext set), "cancelled", or "recording" (start ack).
    status: String,
    /// Native app-cache token (present for successful photo or voice capture).
    path: Option<String>,
    /// File extension without the dot, e.g. "jpg" / "png" / "m4a".
    ext: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::MediaCaptureResult;

    #[test]
    fn native_capture_path_survives_mobile_plugin_deserialization() {
        let result: MediaCaptureResult = serde_json::from_str(
            r#"{"status":"ok","path":"/data/user/0/page.tine.app/cache/voice.m4a","ext":"m4a"}"#,
        )
        .expect("voice memo result should deserialize");

        assert_eq!(
            result.path.as_deref(),
            Some("/data/user/0/page.tine.app/cache/voice.m4a")
        );
        assert_eq!(result.ext.as_deref(), Some("m4a"));
    }

    /// A photo comes back through the SAME shape as a voice memo. The header
    /// once said photos returned base64 bytes in a `data` field; they never did,
    /// and `Block.tsx` has only ever read `res.path` for both.
    #[test]
    fn photo_capture_returns_the_same_path_shape_as_a_voice_memo() {
        let result: MediaCaptureResult = serde_json::from_str(
            r#"{"status":"ok","path":"/data/user/0/page.tine.app/cache/shot.jpg","ext":"jpg"}"#,
        )
        .expect("photo capture result should deserialize");

        assert_eq!(
            result.path.as_deref(),
            Some("/data/user/0/page.tine.app/cache/shot.jpg")
        );
        assert_eq!(result.ext.as_deref(), Some("jpg"));
    }

    /// Captured bytes must not cross the bridge. A `data`-carrying variant would
    /// be a new IPC contract (and a large base64 payload on the UI thread), so
    /// it is a deliberate edit here, not a quiet field addition.
    #[test]
    fn the_capture_result_carries_a_path_and_never_the_bytes() {
        let value = serde_json::to_value(MediaCaptureResult {
            status: "ok".into(),
            path: Some("/cache/shot.jpg".into()),
            ext: Some("jpg".into()),
        })
        .expect("serializable");
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["ext", "path", "status"],
            "the Android media bridge result shape changed; update the module \
             header, which says what crosses the bridge"
        );
    }
}

#[cfg(target_os = "android")]
pub(crate) struct AndroidMedia<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
impl<R: Runtime> AndroidMedia<R> {
    fn call(&self, method: &str) -> Result<MediaCaptureResult, crate::command_error::CommandError> {
        self.0
            .run_mobile_plugin(method, ())
            .map_err(crate::command_error::CommandError::platform)
    }
}

#[cfg(target_os = "android")]
macro_rules! android_media_command {
    ($name:ident, $method:literal) => {
        #[tauri::command]
        pub(crate) async fn $name<R: Runtime>(
            _app: AppHandle<R>,
            media: State<'_, AndroidMedia<R>>,
        ) -> Result<MediaCaptureResult, crate::command_error::CommandError> {
            media.call($method)
        }
    };
}

#[cfg(not(target_os = "android"))]
macro_rules! android_media_command {
    ($name:ident, $method:literal) => {
        #[tauri::command]
        pub(crate) async fn $name() -> Result<MediaCaptureResult, crate::command_error::CommandError> {
            Err(crate::command_error::CommandError::prose("Media capture is only supported on Android"))
        }
    };
}

android_media_command!(capture_photo, "capturePhoto");
android_media_command!(start_recording, "startRecording");
android_media_command!(stop_recording, "stopRecording");
android_media_command!(cancel_recording, "cancelRecording");

#[cfg(target_os = "android")]
fn init_android<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> Result<AndroidMedia<R>, Box<dyn std::error::Error>> {
    let handle = api.register_android_plugin(PLUGIN_IDENTIFIER, "MediaCapturePlugin")?;
    Ok(AndroidMedia(handle))
}

#[cfg(target_os = "android")]
pub(crate) fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("android-media")
        .setup(|app, api| {
            let media = init_android(app, api)?;
            app.manage(media);
            Ok(())
        })
        .build()
}
