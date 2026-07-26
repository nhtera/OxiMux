import { ImageManipulator, SaveFormat } from 'expo-image-manipulator';
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
 * The JPEG quality used when an attachment has to be re-encoded.
 *
 * An agent reading a photo does not need the last decile of JPEG quality to do
 * it, and the difference is most of the payload. Applies only to an image that
 * had to be re-rendered anyway — one that is already sendable is forwarded
 * byte-for-byte (see [`isSendable`]).
 *
 * Note this alone does not make an attachment smaller than the camera's own
 * file: JPEG is the weaker codec, so a straight transcode lands somewhat larger
 * than the HEIC it replaces (measured: 2.8 MB → 3.5 MB). The resize to
 * [`MAX_EDGE`] is what actually brings a photo down; the quality setting only
 * trims what is left.
 */
const QUALITY = 0.7;

/** How many images one prompt may carry, matching the desktop composer. */
export const MAX_ATTACHMENTS = 8;

/**
 * The longest edge an attachment is sent at.
 *
 * The agent's vision pipeline downsamples to roughly this before it looks at
 * anything, so pixels beyond it are discarded after being paid for twice — once
 * in transfer budget, once in the transcript that stores them forever. Resizing
 * here is the rare case where the cheaper thing is also the lossless one.
 */
const MAX_EDGE = 1568;

/**
 * The base64 ceiling for one attachment.
 *
 * The core refuses a prompt whose attachments cannot fit in a single transport
 * frame — roughly 15 MB across all of them — and that refusal names the total,
 * not the photo responsible, so a user who picked several has nothing to act on
 * beyond removing them one at a time. Holding each image to the frame budget
 * divided by [`MAX_ATTACHMENTS`] means a full set always fits and the refusal
 * becomes unreachable in normal use.
 */
export const MAX_ATTACHMENT_BASE64 = 1_800_000;

/**
 * Decode the first `bytes` of a base64 payload into a binary string.
 *
 * Slices on a 4-character boundary because `atob` rejects a partial group — the
 * point is to read a header without decoding megabytes of pixel data behind it.
 * `null` if the text is not valid base64.
 */
function decodeHead(base64: string, bytes: number): string | null {
  try {
    return globalThis.atob(base64.slice(0, Math.ceil(bytes / 3) * 4));
  } catch {
    return null;
  }
}

/**
 * The EXIF orientation a JPEG declares: 1 when upright, 2–8 for a flip/rotation,
 * and 1 for anything that is not a JPEG or carries no readable tag.
 *
 * This decides whether an image can be forwarded untouched. A photo stores its
 * pixels in the sensor's orientation and leaves this tag to say how to turn them
 * — iOS applies it, but the desktop's decoder and the agent both read raw pixels,
 * so a tag other than 1 means the image must be rendered flat before sending or
 * it arrives rotated. Only JPEG is inspected: PNG has no orientation concept, and
 * the EXIF chunk WebP allows is rare enough that reading it would cost more than
 * it saves.
 */
export function readJpegOrientation(base64: string): number {
  // 64 KB comfortably contains the APP1 segment, which sits within the first few
  // KB of a JPEG in practice — far cheaper than decoding a multi-megabyte photo.
  const d = decodeHead(base64, 64 * 1024);
  if (d === null || d.length < 4) return 1;
  const u8 = (i: number) => d.charCodeAt(i) & 0xff;
  const u16be = (i: number) => (u8(i) << 8) | u8(i + 1);
  if (u8(0) !== 0xff || u8(1) !== 0xd8) return 1;

  let i = 2;
  while (i + 4 <= d.length) {
    if (u8(i) !== 0xff) return 1;
    const marker = u8(i + 1);
    // Start-of-scan or end-of-image: pixel data follows, so there is no EXIF left.
    if (marker === 0xda || marker === 0xd9) return 1;
    const len = u16be(i + 2);
    if (len < 2) return 1;
    if (marker === 0xe1 && d.slice(i + 4, i + 10) === 'Exif\x00\x00') {
      return orientationFromTiff(d, i + 10, Math.min(i + 2 + len, d.length));
    }
    i += 2 + len;
  }
  return 1;
}

/** Read tag 0x0112 out of the TIFF block an APP1 EXIF segment wraps. */
function orientationFromTiff(d: string, start: number, end: number): number {
  if (start + 8 > end) return 1;
  const little = d.slice(start, start + 2) === 'II';
  const u8 = (i: number) => d.charCodeAt(i) & 0xff;
  const u16 = (i: number) => (little ? u8(i) | (u8(i + 1) << 8) : (u8(i) << 8) | u8(i + 1));
  const u32 = (i: number) =>
    (little
      ? u8(i) | (u8(i + 1) << 8) | (u8(i + 2) << 16) | (u8(i + 3) << 24)
      : (u8(i) << 24) | (u8(i + 1) << 16) | (u8(i + 2) << 8) | u8(i + 3)) >>> 0;

  const ifd = start + u32(start + 4);
  if (ifd + 2 > end) return 1;
  const entries = u16(ifd);
  for (let k = 0; k < entries; k++) {
    const e = ifd + 2 + k * 12;
    if (e + 12 > end) break;
    if (u16(e) === 0x0112) {
      const value = u16(e + 8);
      return value >= 1 && value <= 8 ? value : 1;
    }
  }
  return 1;
}

/**
 * Derive the media type from the payload's own magic bytes.
 *
 * The wire type has to describe the payload, and every other source for it can
 * drift: the picker's `mimeType` names the file it wrote rather than the bytes
 * it handed back, and our own re-encode is a native call whose output format we
 * assume rather than observe. The magic bytes cannot disagree with themselves.
 *
 * `null` for anything outside the four encodings the agent accepts, including a
 * payload too short to identify.
 */
export function sniffMediaType(base64: string): string | null {
  // 18 bytes reaches past WebP's `WEBP` tag at offset 8.
  const head = decodeHead(base64, 18);
  if (head === null || head.length < 12) return null;
  if (head.charCodeAt(0) === 0xff && head.charCodeAt(1) === 0xd8 && head.charCodeAt(2) === 0xff) {
    return 'image/jpeg';
  }
  if (head.startsWith('\x89PNG\r\n\x1a\n')) return 'image/png';
  if (head.startsWith('GIF87a') || head.startsWith('GIF89a')) return 'image/gif';
  if (head.startsWith('RIFF') && head.slice(8, 12) === 'WEBP') return 'image/webp';
  return null;
}

/**
 * Whether a picked image can go on the wire exactly as it came off the library.
 *
 * Three separate ways an image is unusable, checked together because they share
 * one remedy — re-rendering it — and because leaving any of them out produces a
 * failure a long way from here:
 *
 * - **Format.** A camera-roll photo is HEIC, which neither the desktop renderer
 *   nor the agent can decode.
 * - **Orientation.** A photo's pixels are stored in the sensor's orientation with
 *   an EXIF tag saying how to turn them. iOS honours that tag, so the phone looks
 *   right; the desktop's decoder and the agent both read raw pixels, so the same
 *   bytes arrive rotated.
 * - **Size.** Attachments share one transport frame, and a full-resolution photo
 *   is several megabytes of base64 on its own, so a few of them are refused as a
 *   group with no indication which one to drop.
 *
 * `mediaType` is passed in already sniffed rather than re-read, so the caller
 * cannot check the encoding here and then send a different one.
 */
export function isSendable(base64: string, mediaType: string): boolean {
  if (base64.length > MAX_ATTACHMENT_BASE64) return false;
  if (mediaType === 'image/jpeg' && readJpegOrientation(base64) !== 1) return false;
  return true;
}

/** What one trip through the picker produced. */
export type PickResult = {
  attachments: Attachment[];
  /** Picked images dropped because they could not be read or re-encoded. */
  skipped: number;
};

/**
 * Render a picked image flat at `format`, downscaled to [`MAX_EDGE`].
 *
 * Rendering to a bitmap is what fixes an unsendable image, and it fixes all
 * three faults at once by construction: the output carries whatever encoding is
 * asked for, the EXIF rotation is baked into the pixels so a consumer with no
 * EXIF support still sees the photo upright, and the resize brings the payload
 * down. It also drops the remaining metadata — GPS, device serial — rather than
 * leaving that to whichever branch the picker happened to take.
 *
 * `null` if the image cannot be decoded at all — the one case where an
 * attachment genuinely has to be dropped.
 */
async function encode(
  source: { uri: string; width: number; height: number },
  format: SaveFormat,
): Promise<ChatImage | null> {
  const context = ImageManipulator.manipulate(source.uri);
  // Only the long edge is given, so the other is derived and the aspect ratio
  // cannot drift. Skipped entirely when the image is already small enough —
  // upscaling a modest screenshot to the cap would invent detail and cost size.
  if (Math.max(source.width, source.height) > MAX_EDGE) {
    context.resize(
      source.width >= source.height ? { width: MAX_EDGE } : { height: MAX_EDGE },
    );
  }
  let image: Awaited<ReturnType<typeof context.renderAsync>> | null = null;
  try {
    image = await context.renderAsync();
    // `compress` is a JPEG parameter; a PNG save ignores it and stays lossless.
    const saved = await image.saveAsync({ format, compress: QUALITY, base64: true });
    const data = saved.base64;
    if (!data) return null;
    // Trust the bytes, not what we believe we just asked for. This is the single
    // place the wire type is decided, and a payload whose label disagrees with
    // its bytes fails far downstream — a blank bubble on the desktop, an error
    // from the agent — with nothing pointing back here.
    const mediaType = sniffMediaType(data);
    return mediaType ? { mediaType, data } : null;
  } catch {
    return null;
  } finally {
    image?.release();
    context.release();
  }
}

/**
 * Re-render an image into something sendable, keeping PNG when the source was
 * PNG.
 *
 * Format is preserved rather than normalised to JPEG because the two kinds of
 * image that reach here fail differently. A screenshot is PNG, and JPEG rings
 * around exactly the text an agent was asked to read; a photo has no such detail
 * to lose and compresses far better. Since the resize alone usually brings a
 * screenshot under the ceiling, the lossless path is also normally the
 * sufficient one — and where it is not, JPEG is the fallback rather than the
 * default, so the cost is only paid by images that actually need it.
 */
async function repair(
  source: { uri: string; width: number; height: number },
  sourceWasPng: boolean,
): Promise<ChatImage | null> {
  if (!sourceWasPng) return encode(source, SaveFormat.JPEG);
  const lossless = await encode(source, SaveFormat.PNG);
  if (lossless && lossless.data.length <= MAX_ATTACHMENT_BASE64) return lossless;
  // Still too large to be worth its losslessness — or unencodable as PNG.
  return (await encode(source, SaveFormat.JPEG)) ?? lossless;
}

/**
 * Open the photo library and return what the user picked.
 *
 * Returns nothing picked and nothing skipped when they cancel — a cancel is not
 * an error and must not surface as one. Permission is requested by the picker
 * itself on both platforms; a denial also arrives as a cancel, which is why
 * there is no separate branch for it here.
 *
 * An image that is already sendable (see [`isSendable`]) is forwarded
 * byte-for-byte; only one that fails a test is repaired, and then only as far as
 * that test requires. The distinction matters most for the screenshot case:
 * screenshots are PNG, upright, and usually modest, so they cross untouched
 * rather than picking up JPEG ringing around exactly the text an agent was asked
 * to read.
 */
export async function pickImages(remaining: number): Promise<PickResult> {
  const result = await ImagePicker.launchImageLibraryAsync({
    mediaTypes: ['images'],
    allowsMultipleSelection: remaining > 1,
    selectionLimit: remaining,
    // Quality 1 takes the picker's copy-the-original path, so `base64` is the
    // untouched source rather than something it re-encoded on the way out. That
    // is what makes a byte-for-byte forward possible below; asking for a quality
    // here would silently re-encode every photo before we ever saw it.
    quality: 1,
    base64: true,
    // The agent gets pixels, not provenance: EXIF carries GPS coordinates and a
    // device serial, which would otherwise ride along to the desktop and into a
    // persisted transcript without the user ever choosing to share a location.
    exif: false,
  });

  if (result.canceled) return { attachments: [], skipped: 0 };

  const picked = await Promise.all(
    result.assets.map(async (asset, index): Promise<Attachment | null> => {
      const id = `${asset.assetId ?? asset.uri}:${index}`;
      // Forward the original when nothing is wrong with it. Every condition is
      // read off the bytes rather than the picker's description of them, so this
      // cannot forward something the desktop or the agent will choke on.
      const source = asset.base64;
      const sourceType = source ? sniffMediaType(source) : null;
      if (source && sourceType && isSendable(source, sourceType)) {
        return { mediaType: sourceType, data: source, uri: asset.uri, id };
      }
      const repaired = await repair(asset, sourceType === 'image/png');
      if (!repaired) return null;
      return { ...repaired, uri: asset.uri, id };
    }),
  );

  const attachments = picked.filter((a): a is Attachment => a !== null);
  return { attachments, skipped: picked.length - attachments.length };
}
