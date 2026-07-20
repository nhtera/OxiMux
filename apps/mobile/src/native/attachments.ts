import * as ImagePicker from 'expo-image-picker';
import type { ChatImage } from 'oximux-core';

/**
 * A picked image, held by the composer until the prompt is sent.
 *
 * Carries the local `uri` alongside the payload purely so the composer can show
 * a thumbnail: rendering the base64 back as a data URI would decode the whole
 * image a second time on the JS thread for every keystroke-triggered re-render.
 */
export type Attachment = ChatImage & {
  /** Local file URI, for the preview thumbnail only — never sent. */
  uri: string;
  /** Stable key for the list; the URI can repeat if the same photo is picked twice. */
  id: string;
};

/** Strip the preview fields, leaving what crosses the FFI. */
export function toChatImage({ mediaType, data }: Attachment): ChatImage {
  return { mediaType, data };
}

/**
 * Re-encode at this quality when the source is a JPEG.
 *
 * The core refuses a prompt whose attachments cannot fit in one transport frame,
 * and base64 inflates by ~4/3, so full-resolution phone photos reach that ceiling
 * in a handful. Compressing here means the common case never hits the refusal —
 * and an agent reading a screenshot or a photo does not need the last decile of
 * JPEG quality to do it.
 */
const QUALITY = 0.7;

/** How many images one prompt may carry, matching the desktop composer. */
export const MAX_ATTACHMENTS = 8;

/**
 * Open the photo library and return what the user picked.
 *
 * Returns `[]` when they cancel — a cancel is not an error and must not surface
 * as one. Permission is requested by the picker itself on both platforms; a
 * denial also arrives as a cancel, which is why there is no separate branch for
 * it here.
 */
export async function pickImages(remaining: number): Promise<Attachment[]> {
  const result = await ImagePicker.launchImageLibraryAsync({
    mediaTypes: ['images'],
    allowsMultipleSelection: remaining > 1,
    selectionLimit: remaining,
    quality: QUALITY,
    base64: true,
    // The agent gets pixels, not provenance: EXIF carries GPS coordinates and a
    // device serial, which would otherwise ride along to the desktop and into a
    // persisted transcript without the user ever choosing to share a location.
    exif: false,
  });

  if (result.canceled) return [];

  return result.assets.flatMap((asset, index) => {
    // An asset without base64 cannot be sent; skipping it silently would look
    // like the pick succeeded, so the caller counts what came back instead.
    if (!asset.base64) return [];
    return [
      {
        mediaType: asset.mimeType ?? 'image/jpeg',
        data: asset.base64,
        uri: asset.uri,
        id: `${asset.assetId ?? asset.uri}:${index}`,
      },
    ];
  });
}
