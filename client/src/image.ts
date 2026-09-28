// Mirrors the server limits in src/images.rs; the server remains the authority.
export const MAX_IMAGE_BYTES = 20 * 1024 * 1024;
export const IMAGE_TYPES = ["image/png", "image/jpeg", "image/webp"];

/** Put an uploaded file path on its own line after the current text. */
export function appendPath(draft: string, path: string): string {
  const separator = draft && !draft.endsWith("\n") ? "\n" : "";
  return `${draft}${separator}${path}\n`;
}

/** Why this file cannot be uploaded, or "" when it can. */
export function imageProblem(file: { type: string; size: number }): string {
  if (!IMAGE_TYPES.includes(file.type))
    return "PNG・JPEG・WebP の画像を選んでください。HEIC などは JPEG で保存し直すと送れます。";
  if (file.size === 0) return "空のファイルは送れません。";
  if (file.size > MAX_IMAGE_BYTES)
    return "画像は 20 MiB までです。縮小してから選び直してください。";
  return "";
}
