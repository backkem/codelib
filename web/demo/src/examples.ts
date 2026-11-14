// The playground's example programs. Each shows one thing the sandbox does.

export interface Example {
  id: string;
  title: string;
  code: string;
}

export const EXAMPLES: Example[] = [
  {
    id: 'value',
    title: 'Return a value',
    code: `// A program is the body of an async function: whatever it returns
// comes back to the page as JSON.
const primes = [];
for (let n = 2; primes.length < 10; n++) {
  if (primes.every((p) => n % p !== 0)) primes.push(n);
}
return { answer: 6 * 7, primes, today: new Date().toISOString().slice(0, 10) };
`,
  },
  {
    id: 'await',
    title: 'Top-level await',
    code: `// await works at the top level; timers are real.
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const t0 = Date.now();
for (const step of ["one", "two", "three"]) {
  await sleep(300);
  console.log(step, "after", Date.now() - t0, "ms");
}
return "done";
`,
  },
  {
    id: 'console',
    title: 'console.log',
    code: `// console output is captured and handed back with the result.
console.log("hello from inside the sandbox");
console.log({ nested: { objects: [1, 2, 3] } });
console.error("stderr is captured separately");
return null;
`,
  },
  {
    id: 'sealed',
    title: 'Sealed: fetch is denied',
    code: `// Nothing leaves the sandbox unless the page allows it. This host is
// not on the allowlist, so fetch fails like a network error, and the
// reason shows up in the console.
try {
  await fetch("https://example.com/");
  return "reached the network?!";
} catch (error) {
  return "caught: " + error.message;
}
`,
  },
  {
    id: 'loop',
    title: 'Endless loop, stopped',
    code: `// A runaway program does not hang the page: the run has a 5 s
// deadline (or press Stop), and its Worker is terminated.
console.log("spinning...");
let n = 0;
while (true) n++;
`,
  },
  {
    id: 'spirograph',
    title: 'Draw: spirograph',
    code: `// draw.* is the page's own API, reached through host.call.
// The script never touches the canvas; the page does.
draw.clear("#0f1020");
const { width, height } = draw.size();
const cx = width / 2, cy = height / 2;
const R = 150, r = 52, d = 95;
let prev = null;
for (let t = 0; t <= Math.PI * 26; t += 0.04) {
  const x = cx + (R - r) * Math.cos(t) + d * Math.cos(((R - r) / r) * t);
  const y = cy + (R - r) * Math.sin(t) - d * Math.sin(((R - r) / r) * t);
  if (prev) draw.line(prev[0], prev[1], x, y, \`hsl(\${(t * 14) % 360} 90% 65%)\`, 1.5);
  prev = [x, y];
}
return "a spirograph in " + Math.round(Math.PI * 26 / 0.04) + " host calls";
`,
  },
  {
    id: 'tree',
    title: 'Draw: fractal tree',
    code: `// Recursion in the sandbox, pixels in the page.
draw.clear("#f4efe6");
let calls = 0;
function branch(x, y, angle, length, depth) {
  if (depth === 0) {
    draw.circle(x, y, 3, \`hsl(\${330 + Math.random() * 40} 80% 70%)\`, true);
    calls++;
    return;
  }
  const x2 = x + Math.cos(angle) * length;
  const y2 = y - Math.sin(angle) * length;
  draw.line(x, y, x2, y2, \`hsl(25 40% \${20 + (10 - depth) * 4}%)\`, depth * 0.9);
  calls++;
  branch(x2, y2, angle + 0.42 + Math.random() * 0.12, length * 0.74, depth - 1);
  branch(x2, y2, angle - 0.38 - Math.random() * 0.12, length * 0.72, depth - 1);
}
branch(200, 390, Math.PI / 2, 92, 9);
draw.text(12, 24, "made in a sandbox", "#7a6a55", 14);
return { hostCalls: calls };
`,
  },
  {
    id: 'github',
    title: 'Real network, allowlisted',
    code: `// The page allows exactly one host, read-only: api.github.com.
// (Public data, no token; the browser's CORS rules apply too.)
const r = await fetch("https://api.github.com/repos/backkem/codelib");
let repo;
if (r.ok) {
  const json = await r.json();
  repo = { name: json.full_name, stars: json.stargazers_count, description: json.description };
} else {
  // Rate-limited, or no such repository yet: the request was still allowed.
  repo = { status: r.status };
}

// Any other host is denied, and the denial is catchable.
let other;
try {
  await fetch("https://api.example.com/");
  other = "reached?!";
} catch (error) {
  other = "denied (" + error.message.trim() + ")";
}
return { repo, other };
`,
  },
  {
    id: 'credentials',
    title: 'Credentials stay outside',
    code: `// vault.internal is answered by the page itself, and the page's rule
// for it attaches a bearer token to every request. The script never
// holds that token.
const plain = await (await fetch("https://vault.internal/secrets")).json();

// A forged header does not help: the page's header replaces it.
const forged = await (await fetch("https://vault.internal/secrets", {
  headers: { Authorization: "Bearer i-am-the-admin" },
})).json();

// Look for the token anywhere the script can see.
const r = await fetch("https://vault.internal/secrets");
const visible = [JSON.stringify(host.input), JSON.stringify([...r.headers]), await r.text()];
const tokenSeen = visible.some((s) => s.includes("Bearer"));
return { plain, forged, tokenSeen };
`,
  },
];
