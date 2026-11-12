import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import { peLayout, assertSignatureOnlyChange } from './pe-signature-content.mjs';

// A synthetic structural fixture, never executed or called a trusted signature.
function fixture(length = 501) {
  const bytes = Buffer.alloc(length);
  bytes.writeUInt16LE(0x5a4d); bytes.writeUInt32LE(0x80, 0x3c);
  bytes.writeUInt32LE(0x4550, 0x80); bytes.writeUInt16LE(240, 0x80 + 20);
  bytes.writeUInt16LE(0x20b, 0x98); bytes.writeUInt32LE(16, 0x98 + 108);
  bytes.fill(0x71, 400);
  return bytes;
}
function append(unsigned) {
  const { checksum, security } = peLayout(unsigned);
  const offset = Math.ceil(unsigned.length / 8) * 8;
  const signed = Buffer.concat([unsigned, Buffer.alloc(offset - unsigned.length + 16)]);
  signed.writeUInt32LE(offset, security); signed.writeUInt32LE(16, security + 4);
  signed.writeUInt32LE(0x31415926, checksum);
  signed.writeUInt32LE(16, offset); signed.writeUInt16LE(0x200, offset + 4); signed.writeUInt16LE(2, offset + 6);
  return signed;
}
const unsigned = fixture(), signed = append(unsigned), layout = peLayout(signed);
assert.equal(assertSignatureOnlyChange(unsigned, signed).originalBytes, unsigned.length);
const tampered = Buffer.from(signed); tampered[420] ^= 1;
assert.throws(() => assertSignatureOnlyChange(unsigned, tampered), /changed executable content/);
const padding = Buffer.from(signed); padding[unsigned.length] = 1;
assert.throws(() => assertSignatureOnlyChange(unsigned, padding), /content length/);
const displaced = Buffer.from(signed); displaced.writeUInt32LE(400, layout.security);
assert.throws(() => assertSignatureOnlyChange(unsigned, displaced), /certificate range/);
assert.throws(() => assertSignatureOnlyChange(unsigned, unsigned), /no embedded certificate/);
assert.throws(() => assertSignatureOnlyChange(signed, signed), /already has/);
assert.throws(() => assertSignatureOnlyChange(unsigned, Buffer.concat([signed, Buffer.from([1])])), /certificate range/);
assert.throws(() => peLayout(Buffer.from('not an executable')), /bounded PE/);

let real = 'NOT RUN: Windows embedded-signature control unavailable';
if (process.platform === 'win32') {
  const candidates = [
    `${process.env.SystemRoot}/System32/WindowsPowerShell/v1.0/powershell.exe`,
    `${process.env.ProgramFiles}/PowerShell/7/pwsh.exe`,
    `${process.env.ProgramFiles}/Git/cmd/git.exe`,
  ];
  for (const file of candidates) {
    if (!fs.existsSync(file)) continue;
    const original = fs.readFileSync(file), info = peLayout(original);
    if (!info.certificate) continue;
    const stripped = Buffer.from(original.subarray(0, info.certificate));
    stripped.fill(0, info.security, info.security + 8);
    assertSignatureOnlyChange(stripped, original);
    const altered = Buffer.from(stripped); altered[altered.length - 1] ^= 1;
    assert.throws(() => assertSignatureOnlyChange(altered, original), /changed executable content/);
    real = `PASS: real embedded-certificate content control (${file.split('/').at(-1)}); trust checked separately`;
    break;
  }
}
console.log(JSON.stringify({ status: 'PASS', structuralCases: 8, real, boundary: 'No certificate generated, no trust-store changes, no altered binary executed' }));
