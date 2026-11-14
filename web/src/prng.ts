// A seeded generator for the virtual world: xoshiro256** seeded through SplitMix64. Small, fast
// enough for WASI's needs, and the same on every engine (BigInt arithmetic, no floats).

const M64 = (1n << 64n) - 1n;

function splitmix64(state: { s: bigint }): bigint {
  state.s = (state.s + 0x9e3779b97f4a7c15n) & M64;
  let z = state.s;
  z = ((z ^ (z >> 30n)) * 0xbf58476d1ce4e5b9n) & M64;
  z = ((z ^ (z >> 27n)) * 0x94d049bb133111ebn) & M64;
  return z ^ (z >> 31n);
}

const rotl = (x: bigint, k: bigint) => ((x << k) | (x >> (64n - k))) & M64;

export class Prng {
  private readonly s: [bigint, bigint, bigint, bigint];

  constructor(seed: bigint | number) {
    const st = { s: BigInt.asUintN(64, BigInt(seed)) };
    this.s = [splitmix64(st), splitmix64(st), splitmix64(st), splitmix64(st)];
  }

  /** The next 64 random bits. */
  nextU64(): bigint {
    const s = this.s;
    const result = (rotl((s[1] * 5n) & M64, 7n) * 9n) & M64;
    const t = (s[1] << 17n) & M64;
    s[2] ^= s[0];
    s[3] ^= s[1];
    s[1] ^= s[2];
    s[0] ^= s[3];
    s[2] ^= t;
    s[3] = rotl(s[3], 45n);
    return result;
  }

  /** Fills `out`, eight little-endian bytes per draw. */
  fill(out: Uint8Array): void {
    for (let i = 0; i < out.length; i += 8) {
      let v = this.nextU64();
      for (let j = 0; j < 8 && i + j < out.length; j++) {
        out[i + j] = Number(v & 0xffn);
        v >>= 8n;
      }
    }
  }
}
