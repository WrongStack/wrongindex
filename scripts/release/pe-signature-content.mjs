// Bind a signing response to the requested PE bytes. This does not verify trust;
// callers must also require WinTrust Valid and the approved certificate digest.
import * as fs from 'node:fs';
import { createHash } from 'node:crypto';
import { pathToFileURL } from 'node:url';

export const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');

export function peLayout(bytes) {
  if (bytes.length < 64 || bytes.length > 512 * 1024 * 1024 || bytes.readUInt16LE(0) !== 0x5a4d) throw Error('Not a bounded PE image');
  const header = bytes.readUInt32LE(0x3c);
  if (header < 64 || header + 24 > bytes.length || bytes.readUInt32LE(header) !== 0x4550) throw Error('Invalid PE header');
  const optional = header + 24;
  const size = bytes.readUInt16LE(header + 20);
  if (optional + size > bytes.length || size < 2) throw Error('Truncated PE optional header');
  const magic = bytes.readUInt16LE(optional);
  const directories = magic === 0x20b ? 112 : magic === 0x10b ? 96 : 0;
  if (!directories || size < directories + 40 || bytes.readUInt32LE(optional + directories - 4) < 5) throw Error('Missing PE security directory');
  const checksum = optional + 64;
  const security = optional + directories + 32;
  const certificate = bytes.readUInt32LE(security);
  const certificateSize = bytes.readUInt32LE(security + 4);
  if ((certificate === 0) !== (certificateSize === 0)) throw Error('Incomplete PE certificate range');
  if (certificate && (certificate % 8 || certificateSize < 8 || certificate + certificateSize !== bytes.length || certificate < optional + size)) throw Error('Invalid PE certificate range');
  return { checksum, security, certificate, certificateSize };
}

export function assertSignatureOnlyChange(unsigned, signed) {
  const a = peLayout(unsigned), b = peLayout(signed);
  if (a.certificate) throw Error('Signing input already has an embedded certificate');
  if (!b.certificate) throw Error('Signing response has no embedded certificate');
  if (a.checksum !== b.checksum || a.security !== b.security) throw Error('Signing changed PE header layout');
  const padding = b.certificate - unsigned.length;
  if (padding < 0 || padding > 7 || signed.subarray(unsigned.length, b.certificate).some(byte => byte !== 0)) throw Error('Signing changed the original content length');
  const left = Buffer.from(unsigned), right = Buffer.from(signed.subarray(0, unsigned.length));
  for (const image of [left, right]) {
    image.fill(0, a.checksum, a.checksum + 4);
    image.fill(0, a.security, a.security + 8);
  }
  if (!left.equals(right)) throw Error('Signing response changed executable content');
  return { unsignedSha256: sha256(unsigned), signedSha256: sha256(signed), originalBytes: unsigned.length };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  if (process.argv.length !== 4) throw Error('Usage: pe-signature-content.mjs <unsigned> <signed>');
  const result = assertSignatureOnlyChange(fs.readFileSync(process.argv[2]), fs.readFileSync(process.argv[3]));
  console.log(JSON.stringify({ status: 'PASS', ...result, boundary: 'Content binding only; Authenticode trust is checked separately' }));
}
