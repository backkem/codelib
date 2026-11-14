// The playground's own host API: a tiny drawing surface the script reaches through
// `host.call("draw.*", ...)`. This is what "your own API, exposed to the script" means: the
// page implements a few handlers over its own state (a canvas), and a prelude gives the script
// a friendly client for them. The script never touches the canvas itself.

import { HostFailure, jsonHostCalls, type HostCalls } from 'codelib-web/script';

/** The client the script sees as `draw`, evaluated before every program. */
export const DRAW_PRELUDE = `
globalThis.draw = Object.freeze({
  size: () => host.call("draw.size"),
  clear: (color) => host.call("draw.clear", { color }),
  line: (x1, y1, x2, y2, color, width) => host.call("draw.line", { x1, y1, x2, y2, color, width }),
  circle: (x, y, r, color, fill) => host.call("draw.circle", { x, y, r, color, fill }),
  text: (x, y, text, color, size) => host.call("draw.text", { x, y, text: String(text), color, size }),
});
`;

const SIZE = 400;
const num = (v: unknown, name: string): number => {
  if (typeof v !== 'number' || !Number.isFinite(v)) throw new HostFailure('bad-args', `${name} must be a finite number`);
  return v;
};
const color = (v: unknown, fallback: string): string => (typeof v === 'string' && v.length < 64 ? v : fallback);

/** Host calls drawing on `canvas` (400 x 400 logical pixels, scaled for the screen). */
export function drawApi(canvas: HTMLCanvasElement): { hostCalls: HostCalls; reset(): void } {
  const ctx = canvas.getContext('2d')!;
  const scale = canvas.width / SIZE;
  const reset = () => {
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.setTransform(scale, 0, 0, scale, 0, 0);
    ctx.lineCap = 'round';
  };
  reset();
  type Args = Record<string, unknown>;
  const hostCalls = jsonHostCalls({
    'draw.size': () => ({ width: SIZE, height: SIZE }),
    'draw.clear': (a) => {
      reset();
      const c = (a as Args | null)?.color;
      if (typeof c === 'string') {
        ctx.fillStyle = color(c, '#fff');
        ctx.fillRect(0, 0, SIZE, SIZE);
      }
    },
    'draw.line': (a) => {
      const { x1, y1, x2, y2, width } = (a ?? {}) as Args;
      ctx.strokeStyle = color((a as Args).color, '#888');
      ctx.lineWidth = width === undefined ? 1 : num(width, 'width');
      ctx.beginPath();
      ctx.moveTo(num(x1, 'x1'), num(y1, 'y1'));
      ctx.lineTo(num(x2, 'x2'), num(y2, 'y2'));
      ctx.stroke();
    },
    'draw.circle': (a) => {
      const { x, y, r, fill } = (a ?? {}) as Args;
      ctx.beginPath();
      ctx.arc(num(x, 'x'), num(y, 'y'), Math.max(0, num(r, 'r')), 0, Math.PI * 2);
      const c = color((a as Args).color, '#888');
      if (fill) {
        ctx.fillStyle = c;
        ctx.fill();
      } else {
        ctx.strokeStyle = c;
        ctx.lineWidth = 1;
        ctx.stroke();
      }
    },
    'draw.text': (a) => {
      const { x, y, text, size } = (a ?? {}) as Args;
      ctx.fillStyle = color((a as Args).color, '#888');
      ctx.font = `${size === undefined ? 16 : Math.min(96, num(size, 'size'))}px system-ui, sans-serif`;
      ctx.fillText(String(text ?? '').slice(0, 200), num(x, 'x'), num(y, 'y'));
    },
  });
  return { hostCalls, reset };
}
