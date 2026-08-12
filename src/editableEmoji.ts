export type EditableEmojiPlatform = "windows" | "apple" | "android" | "safe-monochrome";

export function editableEmojiPlatform(userAgent: string): EditableEmojiPlatform {
  if (/Android/i.test(userAgent)) return "android";
  if (/Windows/i.test(userAgent)) return "windows";
  if (/(Macintosh|Mac OS|iPhone|iPad|iPod)/i.test(userAgent)) return "apple";
  return "safe-monochrome";
}

/** Select the editable-control emoji face before first paint. Display surfaces
 * continue to use Twemoji SVGs; only native input/textarea font fallback changes. */
export function installEditableEmojiPlatform(userAgent = navigator.userAgent): EditableEmojiPlatform {
  const platform = editableEmojiPlatform(userAgent);
  document.documentElement.dataset.editableEmoji = platform;
  return platform;
}
