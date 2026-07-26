import { readJpegOrientation, sniffMediaType } from '@/native/attachments';

/** Base64 of `bytes` followed by enough filler to clear the 12-byte minimum. */
function b64(bytes: number[]): string {
  const padded = [...bytes, ...new Array(24).fill(0)];
  let binary = '';
  for (const byte of padded) binary += String.fromCharCode(byte);
  return globalThis.btoa(binary);
}

const ascii = (s: string) => [...s].map((c) => c.charCodeAt(0));

describe('sniffMediaType', () => {
  it('reads the four encodings the agent accepts', () => {
    expect(sniffMediaType(b64([0xff, 0xd8, 0xff, 0xe0]))).toBe('image/jpeg');
    expect(sniffMediaType(b64([0x89, ...ascii('PNG'), 0x0d, 0x0a, 0x1a, 0x0a]))).toBe('image/png');
    expect(sniffMediaType(b64(ascii('GIF89a')))).toBe('image/gif');
    expect(sniffMediaType(b64([...ascii('RIFF'), 0, 0, 0, 0, ...ascii('WEBP')]))).toBe('image/webp');
  });

  /** The bug this guards: an iPhone photo arrives as HEIC, which nothing downstream can decode. */
  it('rejects HEIC so it is re-encoded rather than sent', () => {
    expect(sniffMediaType(b64([0, 0, 0, 0x18, ...ascii('ftypheic')]))).toBeNull();
  });

  it('rejects a RIFF container that is not WebP', () => {
    expect(sniffMediaType(b64([...ascii('RIFF'), 0, 0, 0, 0, ...ascii('WAVE')]))).toBeNull();
  });

  it('rejects payloads too short to identify, without throwing', () => {
    expect(sniffMediaType('')).toBeNull();
    expect(sniffMediaType(globalThis.btoa('abc'))).toBeNull();
  });

  it('rejects text that is not valid base64, without throwing', () => {
    expect(sniffMediaType('not base64 at all!!')).toBeNull();
  });
});

/** Encode raw bytes as base64 the way the picker would hand them over. */
function toBase64(bytes: number[]): string {
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return globalThis.btoa(binary);
}

/**
 * A JPEG carrying one APP1 EXIF segment that declares `orientation`, followed by
 * a start-of-scan marker. `padTo` grows the segment so the tag can be pushed
 * beyond a short read.
 */
function jpegWithOrientation(orientation: number, { littleEndian = false, padTo = 0 } = {}): string {
  const u16 = (v: number) => (littleEndian ? [v & 0xff, v >> 8] : [v >> 8, v & 0xff]);
  const u32 = (v: number) =>
    littleEndian
      ? [v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff, (v >> 24) & 0xff]
      : [(v >> 24) & 0xff, (v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff];

  const tiff = [
    ...ascii(littleEndian ? 'II' : 'MM'),
    ...u16(42),
    ...u32(8), // IFD0 begins right after this header
    ...u16(1), // one entry
    ...u16(0x0112), // Orientation
    ...u16(3), // SHORT
    ...u32(1),
    ...u16(orientation),
    ...[0, 0], // SHORT occupies the first half of the 4-byte value slot
    ...u32(0), // no next IFD
  ];
  const payload = [...ascii('Exif'), 0, 0, ...tiff, ...new Array(padTo).fill(0)];
  const segment = [0xff, 0xe1, ...u16BE(payload.length + 2), ...payload];
  return toBase64([0xff, 0xd8, ...segment, 0xff, 0xda, 0, 2]);
}

const u16BE = (v: number) => [v >> 8, v & 0xff];

describe('readJpegOrientation', () => {
  it('reads an upright JPEG as 1', () => {
    expect(readJpegOrientation(jpegWithOrientation(1))).toBe(1);
  });

  /** The tag that made a camera photo render upside down on the desktop. */
  it('reads the 180-degree tag that a camera photo carries', () => {
    expect(readJpegOrientation(jpegWithOrientation(3))).toBe(3);
  });

  it('reads a little-endian EXIF block', () => {
    expect(readJpegOrientation(jpegWithOrientation(6, { littleEndian: true }))).toBe(6);
  });

  it('reads a tag sitting deep in a padded segment', () => {
    expect(readJpegOrientation(jpegWithOrientation(8, { padTo: 4096 }))).toBe(8);
  });

  /** Anything unreadable must claim upright, so the image is repaired, not trusted. */
  it('treats a non-JPEG as upright so the format check decides instead', () => {
    expect(readJpegOrientation(toBase64([0x89, ...ascii('PNG'), 0x0d, 0x0a, 0x1a, 0x0a]))).toBe(1);
  });

  it('treats a JPEG with no EXIF as upright', () => {
    expect(readJpegOrientation(toBase64([0xff, 0xd8, 0xff, 0xda, 0, 2]))).toBe(1);
  });

  it('treats an out-of-range tag value as upright', () => {
    expect(readJpegOrientation(jpegWithOrientation(99))).toBe(1);
  });

  it('does not throw on truncated or invalid input', () => {
    expect(readJpegOrientation('')).toBe(1);
    expect(readJpegOrientation('not base64 at all!!')).toBe(1);
    expect(readJpegOrientation(toBase64([0xff, 0xd8]))).toBe(1);
  });
});
