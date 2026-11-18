import jsQR from 'jsqr';
import { describe, expect, it } from 'vitest';
import { encodeQr } from '../src/lib/qr';

/** Renders a code as RGBA pixels, `scale` pixels a module, with a quiet zone, and reads it back. */
function decode(text: string, scale = 4): string | undefined {
  const { size, modules } = encodeQr(text);
  const margin = 4;
  const dim = (size + margin * 2) * scale;
  const pixels = new Uint8ClampedArray(dim * dim * 4).fill(255);
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      if (!modules[y][x]) continue;
      for (let dy = 0; dy < scale; dy++) {
        for (let dx = 0; dx < scale; dx++) {
          const i = (((y + margin) * scale + dy) * dim + (x + margin) * scale + dx) * 4;
          pixels[i] = pixels[i + 1] = pixels[i + 2] = 0;
        }
      }
    }
  }
  return jsQR(pixels, dim, dim)?.data;
}

describe('QR codes', () => {
  it('decode to the text they encode, at every size a join link needs', () => {
    const samples = [
      'a',
      'https://play.moul.dev/',
      'https://play.moul.dev/?db=notes#moul-join=' + 'x'.repeat(120),
      'https://play.moul.dev/?db=notes#moul-join=' + 'eyJlbmRwb2ludCI6Imh0dHBzOi8vYWNjdC5yMi5jbG91ZGZsYXJlc3RvcmFnZS5jb20i'.repeat(6),
      'ünïcödé ✓ ' + 'y'.repeat(700),
    ];
    for (const text of samples) expect(decode(text)).toBe(text);
  });

  it('grow with the text', () => {
    expect(encodeQr('a').size).toBe(21);
    expect(encodeQr('z'.repeat(300)).size).toBeGreaterThan(45);
    expect(() => encodeQr('z'.repeat(3000))).toThrow(RangeError);
  });
});
