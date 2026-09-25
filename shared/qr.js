/**
 * A QR code, drawn locally as an SVG.
 *
 * The Webmail shows an `otpauth://` URI while the page is holding the shared secret
 * it encodes, so the drawing has to happen here. A library would be a dependency
 * the front ends do not otherwise have — there is no build step to vendor one into —
 * and asking another service to render it would hand that service the secret.
 *
 * What is implemented is the slice an authenticator actually scans: QR model 2, byte
 * mode, the error correction a camera can still read off a screen (M, about 15%),
 * and versions 1 to 10, which is as long as an `otpauth://` URI gets. Anything
 * longer is refused rather than guessed at.
 *
 * The block layout below is the one ISO/IEC 18004 fixes for error correction M.
 * It is data, not an algorithm someone might reasonably re-derive, which is why it
 * is written out; everything else — the generator polynomial, the alignment
 * centres, the format and version bits — is computed.
 */

/**
 * How a version at error correction M is split into blocks: `[blocks, data
 * codewords per block]`, and a second pair when the version has a short group.
 * Derived from the raw module count, the per-block codeword count and the block
 * count that ISO/IEC 18004 fixes for each version.
 */
const BLOCKS = [
  null,
  [[1, 16]],
  [[1, 28]],
  [[1, 44]],
  [[2, 32]],
  [[2, 43]],
  [[4, 27]],
  [[4, 31]],
  [[2, 38], [2, 39]],
  [[3, 36], [2, 37]],
  [[4, 43], [1, 44]],
];

/** Bytes of error-correction codewords per block, indexed by version (1-based). */
const EC_CODEWORDS = [0, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26];

/**
 * Alignment-pattern centres, one list per version. Version 1 has none; from
 * version 2 the first centre is always 6 and the last is always `size − 7`.
 */
function alignmentCentres(version) {
  if (version === 1) return [];
  const size = 17 + 4 * version;
  const count = Math.floor(version / 7) + 2;
  const step = Math.floor((version * 8 + count * 3 + 5) / (count * 4 - 4)) * 2;
  const centres = [6];
  for (let pos = size - 7; centres.length < count; pos -= step) centres.push(pos);
  return centres.sort((a, b) => a - b);
}

/**
 * Format information for error correction M and mask 0, already XOR-masked with
 * the fixed pattern. Mask 0 is the only mask used: a URI is not the pathological
 * input the other seven masks exist to rescue, and picking one keeps the drawing
 * deterministic.
 */
const FORMAT_BITS = 0b101010000010010;

/**
 * The Reed–Solomon generator for `degree` error-correction codewords: the
 * coefficients of (x − α⁰)(x − α¹)…(x − α^(degree−1)), highest power first and
 * without the leading 1. Computed rather than tabulated, so there is no
 * coefficient list to transcribe wrong.
 * @param {number} degree
 */
function generator(degree) {
  const coeff = new Array(degree).fill(0);
  coeff[degree - 1] = 1;
  let root = 1;
  for (let i = 0; i < degree; i += 1) {
    for (let j = 0; j < coeff.length; j += 1) {
      coeff[j] = gfMul(coeff[j], root);
      if (j + 1 < coeff.length) coeff[j] ^= coeff[j + 1];
    }
    root = gfMul(root, 2);
  }
  return coeff;
}

const GF_EXP = new Uint8Array(512);
const GF_LOG = new Uint8Array(256);

// α = 2 in GF(2^8) with the QR primitive polynomial 0x11d.
let value = 1;
for (let i = 0; i < 255; i += 1) {
  GF_EXP[i] = value;
  GF_LOG[value] = i;
  value <<= 1;
  if (value & 0x100) value ^= 0x11d;
}
for (let i = 255; i < 512; i += 1) GF_EXP[i] = GF_EXP[i - 255];

/** @param {number} a @param {number} b */
function gfMul(a, b) {
  if (a === 0 || b === 0) return 0;
  return GF_EXP[GF_LOG[a] + GF_LOG[b]];
}

/**
 * The error-correction codewords for one block of data.
 * @param {number[]} data
 * @param {number} count how many codewords to append
 */
function errorCorrection(data, count) {
  const divisor = generator(count);
  const rest = new Array(count).fill(0);
  for (const byte of data) {
    const factor = byte ^ rest[0];
    rest.copyWithin(0, 1);
    rest[count - 1] = 0;
    for (let i = 0; i < count; i += 1) rest[i] ^= gfMul(divisor[i], factor);
  }
  return rest;
}

/**
 * Encode `text` as the codeword stream of the smallest version that holds it.
 * @param {string} text
 * @returns {{version: number, codewords: number[]}}
 */
function encode(text) {
  const bytes = new TextEncoder().encode(text);
  for (let version = 1; version < BLOCKS.length; version += 1) {
    const groups = BLOCKS[version];
    const dataCodewords = groups.reduce((sum, [blocks, size]) => sum + blocks * size, 0);
    const capacity = dataCodewords * 8;
    // The byte-mode count is 8 bits through version 9 and 16 bits from version 10.
    const countBits = version < 10 ? 8 : 16;
    const bitsNeeded = 4 + countBits + bytes.length * 8;
    if (bitsNeeded > capacity) continue;

    const bits = [];
    const push = (value, width) => {
      for (let i = width - 1; i >= 0; i -= 1) bits.push((value >>> i) & 1);
    };
    push(0b0100, 4);
    push(bytes.length, countBits);
    for (const byte of bytes) push(byte, 8);
    for (let i = 0; i < 4 && bits.length < capacity; i += 1) bits.push(0);
    while (bits.length % 8 !== 0) bits.push(0);
    const data = [];
    for (let i = 0; i < bits.length; i += 8) {
      let byte = 0;
      for (let b = 0; b < 8; b += 1) byte = (byte << 1) | bits[i + b];
      data.push(byte);
    }
    // Pad bytes alternate 0xEC, 0x11, and the first one is always 0xEC — the
    // phase follows how many pad bytes have been added, not the data length.
    for (let i = 0; data.length < dataCodewords; i += 1) data.push(i % 2 === 0 ? 0b11101100 : 0b00010001);

    // Split into blocks, append each block's error correction, then interleave.
    const blocks = [];
    let offset = 0;
    for (const [count, size] of groups) {
      for (let i = 0; i < count; i += 1) {
        const slice = data.slice(offset, offset + size);
        offset += size;
        blocks.push({ data: slice, ec: errorCorrection(slice, EC_CODEWORDS[version]) });
      }
    }
    const codewords = [];
    const longest = Math.max(...blocks.map((block) => block.data.length));
    for (let i = 0; i < longest; i += 1) {
      for (const block of blocks) if (i < block.data.length) codewords.push(block.data[i]);
    }
    for (let i = 0; i < EC_CODEWORDS[version]; i += 1) {
      for (const block of blocks) codewords.push(block.ec[i]);
    }
    return { version, codewords };
  }
  throw new Error('the text does not fit a QR code this module can draw');
}

/**
 * The matrix for one encoded payload, with function patterns and mask 0 applied.
 * `true` is a dark module.
 * @param {number} version
 * @param {number[]} codewords
 */
function matrix(version, codewords) {
  const size = 17 + 4 * version;
  const modules = Array.from({ length: size }, () => new Array(size).fill(false));
  const reserved = Array.from({ length: size }, () => new Array(size).fill(false));

  const mark = (row, col) => {
    modules[row][col] = true;
    reserved[row][col] = true;
  };
  const reserve = (row, col) => {
    reserved[row][col] = true;
  };

  // A finder is a 7×7 ring centred three modules in from its corner, plus the
  // one-module light separator around it. Measuring from the corner rather than
  // the centre turns the separator into a solid band.
  const finder = (cx, cy) => {
    for (let row = -4; row <= 4; row += 1) {
      for (let col = -4; col <= 4; col += 1) {
        const r = cy + row;
        const c = cx + col;
        if (r < 0 || c < 0 || r >= size || c >= size) continue;
        const dist = Math.max(Math.abs(row), Math.abs(col));
        mark(r, c);
        if (dist === 2 || dist === 4) modules[r][c] = false;
      }
    }
  };
  finder(3, 3);
  finder(size - 4, 3);
  finder(3, size - 4);

  for (const centre of alignmentCentres(version)) {
    for (const other of alignmentCentres(version)) {
      if (reserved[centre][other]) continue;
      for (let row = -2; row <= 2; row += 1) {
        for (let col = -2; col <= 2; col += 1) {
          const ring = Math.max(Math.abs(row), Math.abs(col));
          if (ring !== 1) mark(centre + row, other + col);
          else reserve(centre + row, other + col);
        }
      }
    }
  }

  for (let i = 8; i < size - 8; i += 1) {
    if (i % 2 === 0) {
      mark(6, i);
      mark(i, 6);
    } else {
      reserve(6, i);
      reserve(i, 6);
    }
  }

  // Version information: an 18-bit BCH code drawn twice, in the 6×3 blocks above
  // the lower finder and beside the upper-right one. Versions 1–6 have none.
  if (version >= 7) {
    let rem = version;
    for (let i = 0; i < 12; i += 1) rem = (rem << 1) ^ ((rem >>> 11) * 0x1f25);
    const versionBits = (version << 12) | rem;
    for (let i = 0; i < 18; i += 1) {
      const dark = ((versionBits >>> i) & 1) === 1;
      const a = size - 11 + (i % 3);
      const b = Math.floor(i / 3);
      if (dark) { mark(a, b); mark(b, a); } else { reserve(a, b); reserve(b, a); }
    }
  }

  // The dark module, and the format strips around the finders. The module at the
  // corner of the lower format strip is dark in every QR code, regardless of the
  // format bits, so it is drawn rather than left for the strip to fill.
  mark(size - 8, 8);
  for (let i = 0; i < 9; i += 1) {
    reserve(8, i);
    reserve(i, 8);
  }
  for (let i = 0; i < 8; i += 1) {
    reserve(8, size - 1 - i);
    reserve(size - 1 - i, 8);
  }

  // Place the payload in the standard zigzag, skipping everything reserved.
  // Column pairs alternate direction and the rightmost pair is read upward. The
  // timing column makes that alternation skip a beat, so the direction follows
  // how many pairs have actually been visited rather than the column number.
  let bit = 0;
  const total = codewords.length * 8;
  let pair = 0;
  for (let col = size - 1; col > 0; col -= 2) {
    if (col === 6) col -= 1;
    const upward = pair % 2 === 0;
    pair += 1;
    for (let row = 0; row < size; row += 1) {
      const r = upward ? size - 1 - row : row;
      for (const c of [col, col - 1]) {
        if (reserved[r][c]) continue;
        let dark = false;
        if (bit < total) dark = ((codewords[bit >>> 3] >>> (7 - (bit & 7))) & 1) === 1;
        bit += 1;
        // Mask 0 flips every module whose coordinates sum to an even number.
        if ((r + c) % 2 === 0) dark = !dark;
        modules[r][c] = dark;
      }
    }
  }

  // Bit 14 is the most significant and is drawn first; the copies read in opposite
  // directions, which is what the two strips around the finders do.
  const format = (bitIndex) => ((FORMAT_BITS >>> (14 - bitIndex)) & 1) === 1;
  for (let i = 0; i < 6; i += 1) modules[8][i] = format(i);
  modules[8][7] = format(6);
  modules[8][8] = format(7);
  modules[7][8] = format(8);
  for (let i = 9; i < 15; i += 1) modules[14 - i][8] = format(i);
  for (let i = 0; i < 8; i += 1) modules[size - 1 - i][8] = format(i);
  for (let i = 8; i < 15; i += 1) modules[8][size - 15 + i] = format(i);
  // The module at the corner of this strip is dark in every symbol, whatever the
  // format bits say, so it is set after the strip rather than derived from them.
  modules[size - 8][8] = true;

  return modules;
}

/**
 * An SVG document for `text`, or `null` when the text is not a string this module
 * can encode. Callers treat `null` as "show the text instead" — a missing picture
 * must never be the only copy of a secret.
 *
 * @param {string} text
 * @returns {string|null}
 */
export function qrSvg(text) {
  if (typeof text !== 'string' || text === '') return null;
  let drawn;
  try {
    const encoded = encode(text);
    drawn = matrix(encoded.version, encoded.codewords);
  } catch {
    return null;
  }
  const size = drawn.length;
  // Four modules of quiet zone, which is what a scanner needs and no more.
  const quiet = 4;
  const extent = size + quiet * 2;
  let path = '';
  for (let row = 0; row < size; row += 1) {
    let col = 0;
    while (col < size) {
      if (!drawn[row][col]) {
        col += 1;
        continue;
      }
      const start = col;
      while (col < size && drawn[row][col]) col += 1;
      path += `M${start + quiet} ${row + quiet}h${col - start}v1h-${col - start}z`;
    }
  }
  return (
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${extent} ${extent}" ` +
    `shape-rendering="crispEdges" role="img">` +
    `<path fill="currentColor" d="${path}"/></svg>`
  );
}
