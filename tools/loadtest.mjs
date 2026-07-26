/**
 * TensorChat load generator.
 *
 * Measures the thing the server is actually designed around: **fanout**. It
 * opens N WebSocket clients in one channel, has a few of them publish, and
 * records how long a message takes to reach every other client — plus how many
 * deliveries the server got out of each `rmp_serde` encode.
 *
 * Zero dependencies: Node 26 ships a WebSocket client, and the MessagePack
 * codec is the same file the browser uses (run directly via type-stripping).
 *
 *   node tools/loadtest.mjs --clients 200 --publishers 5 --rate 4 --seconds 20
 *
 * Options:
 *   --url        server base URL           (default http://127.0.0.1:8080)
 *   --clients    concurrent connections    (default 100)
 *   --publishers how many of them send     (default 4)
 *   --rate       messages/sec per publisher(default 2)
 *   --seconds    duration                  (default 15)
 *   --channel    channel name to use       (default loadtest)
 */

import { decode, encode } from '../web/src/msgpack.ts';

const args = parseArgs(process.argv.slice(2));
const BASE = args.url ?? 'http://127.0.0.1:8080';
const CLIENTS = Number(args.clients ?? 100);
const PUBLISHERS = Math.min(Number(args.publishers ?? 4), CLIENTS);
// Default matches the server's sustained per-connection message allowance, so
// a default run measures delivery rather than the rate limiter.
const RATE = Number(args.rate ?? 2);
const SECONDS = Number(args.seconds ?? 15);
const CHANNEL_NAME = args.channel ?? 'loadtest';
const PASSWORD = 'loadtest-password-1';

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    if (!argv[i].startsWith('--')) continue;
    out[argv[i].slice(2)] = argv[i + 1]?.startsWith('--') ? true : argv[++i];
  }
  return out;
}

async function http(method, path, token, body) {
  const res = await fetch(BASE + path, {
    method,
    headers: {
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      ...(body ? { 'Content-Type': 'application/json' } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) {
    const text = await res.text().catch(() => '');
    throw new Error(`${method} ${path} -> ${res.status} ${text}`);
  }
  return res.status === 204 ? null : res.json();
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * Register, or log in if the account already exists from a previous run.
 *
 * Retries on 429. Provisioning hundreds of accounts from one host is exactly
 * the pattern the per-address auth limiter exists to stop, so the load tester
 * has to back off like any well-behaved client. Raise the server's
 * `TC_AUTH_BURST` / `TC_AUTH_PER_SECOND` to make setup faster.
 */
async function account(handle) {
  for (let attempt = 0; attempt < 12; attempt++) {
    try {
      return await http('POST', '/api/register', null, { handle, password: PASSWORD });
    } catch (err) {
      if (/-> 409/.test(err.message)) {
        // Already exists from a previous run.
        try {
          return await http('POST', '/api/login', null, { handle, password: PASSWORD });
        } catch (loginErr) {
          if (!/-> 429/.test(loginErr.message)) throw loginErr;
        }
      } else if (!/-> 429/.test(err.message)) {
        throw err;
      }
      await sleep(400 * 2 ** Math.min(attempt, 5));
    }
  }
  throw new Error(`could not provision ${handle}: still rate limited after retries`);
}

function percentile(sorted, p) {
  if (sorted.length === 0) return 0;
  const at = Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length));
  return sorted[at];
}

async function main() {
  console.log(`TensorChat load test -> ${BASE}`);
  console.log(
    `  ${CLIENTS} clients, ${PUBLISHERS} publishing at ${RATE}/s, for ${SECONDS}s\n`,
  );

  // -- Set up accounts and the channel ------------------------------------

  process.stdout.write('provisioning accounts… ');
  const sessions = [];
  // Batched rather than all-at-once: Argon2 is intentionally expensive, and a
  // thousand concurrent registrations would measure the KDF, not the server.
  for (let i = 0; i < CLIENTS; i += 20) {
    const batch = [];
    for (let j = i; j < Math.min(i + 20, CLIENTS); j++) {
      batch.push(account(`load${j}`));
    }
    sessions.push(...(await Promise.all(batch)));
  }
  console.log(`${sessions.length} ready`);

  const owner = sessions[0];
  let channel;
  const existing = await http('GET', '/api/channels/browse', owner.token);
  channel = existing.find((c) => c.n === CHANNEL_NAME);
  if (!channel) {
    channel = await http('POST', '/api/channels', owner.token, { name: CHANNEL_NAME });
  }

  process.stdout.write('joining channel… ');
  for (let i = 0; i < sessions.length; i += 25) {
    await Promise.all(
      sessions
        .slice(i, i + 25)
        .map((s) => http('POST', `/api/channels/${channel.id}/join`, s.token).catch(() => {})),
    );
  }
  console.log('done');

  // -- Connect -------------------------------------------------------------

  const wsBase = BASE.replace(/^http/, 'ws');
  const stats = {
    connected: 0,
    received: 0,
    sent: 0,
    /** Sends the server acknowledged. This, not `sent`, is the real numerator. */
    accepted: 0,
    rateLimited: 0,
    errors: 0,
    /** message -> count, so failures are named rather than bucketed. */
    errorCodes: new Map(),
    latencies: [],
  };
  /** nonce -> send timestamp, for end-to-end latency. */
  const inFlight = new Map();

  const sockets = await Promise.all(
    sessions.map(
      (s) =>
        new Promise((resolve) => {
          const ws = new WebSocket(`${wsBase}/ws?token=${encodeURIComponent(s.token)}`);
          ws.binaryType = 'arraybuffer';
          ws.onopen = () => {
            stats.connected++;
            resolve(ws);
          };
          ws.onerror = () => {
            stats.errors++;
            resolve(null);
          };
          ws.onmessage = (ev) => {
            const frame = decode(new Uint8Array(ev.data));
            if (frame.ack) {
              stats.accepted++;
              return;
            }
            if (frame.err) {
              if (frame.err.c === 'rate_limited') stats.rateLimited++;
              else stats.errors++;
              // Keep a tally by code, so an unexpected failure names itself in
              // the report instead of hiding in an "other" bucket.
              stats.errorCodes.set(
                `${frame.err.c}: ${frame.err.m}`,
                (stats.errorCodes.get(`${frame.err.c}: ${frame.err.m}`) ?? 0) + 1,
              );
              return;
            }
            if (!frame.msg) return;
            stats.received++;
            // The body carries the publisher's send time, so latency is
            // measured across the whole path: encode, fanout, socket, decode.
            const marker = frame.msg.m.b.match(/#(\d+)#/);
            if (marker) {
              const sentAt = inFlight.get(marker[1]);
              if (sentAt !== undefined) stats.latencies.push(performance.now() - sentAt);
            }
          };
          ws.onclose = () => {
            stats.connected--;
          };
        }),
    ),
  );

  const live = sockets.filter(Boolean);
  console.log(`connected ${live.length}/${CLIENTS}\n`);
  if (live.length === 0) {
    console.error('no connections established — is the server running?');
    process.exit(1);
  }

  // Let the `ready` snapshot for every client finish arriving before we start
  // timing, so setup traffic does not pollute the latency sample.
  await new Promise((r) => setTimeout(r, 1000));
  stats.received = 0;
  stats.latencies.length = 0;

  // -- Publish -------------------------------------------------------------

  let seq = 0;
  const started = performance.now();
  const timers = [];
  for (let p = 0; p < PUBLISHERS; p++) {
    const ws = live[p % live.length];
    timers.push(
      setInterval(() => {
        if (ws.readyState !== 1) return;
        const id = String(++seq);
        inFlight.set(id, performance.now());
        ws.send(
          encode({
            send: {
              n: seq,
              ch: channel.id,
              b: `load message #${id}# from publisher ${p}`,
              th: null,
              at: [],
            },
          }),
        );
        stats.sent++;
      }, 1000 / RATE),
    );
  }

  const progress = setInterval(() => {
    const elapsed = (performance.now() - started) / 1000;
    process.stdout.write(
      `\r  ${elapsed.toFixed(0)}s  sent ${stats.sent}  received ${stats.received}  ` +
        `(${Math.round(stats.received / Math.max(elapsed, 0.001))}/s)   `,
    );
  }, 1000);

  await new Promise((r) => setTimeout(r, SECONDS * 1000));
  for (const t of timers) clearInterval(t);
  clearInterval(progress);
  // Drain anything still in flight. A single Node process decoding hundreds of
  // sockets is itself a bottleneck, so give it real time before measuring —
  // otherwise the tool reports its own backlog as server loss.
  let settled = 0;
  for (let i = 0; i < 20; i++) {
    await sleep(250);
    if (stats.received === settled) break;
    settled = stats.received;
  }

  const elapsed = (performance.now() - started) / 1000;
  const metrics = await http('GET', '/api/metrics', owner.token).catch(() => null);
  for (const ws of live) ws.close();

  // -- Report --------------------------------------------------------------

  const sorted = stats.latencies.sort((a, b) => a - b);
  // Expected fanout is driven by *accepted* sends. Counting attempted sends
  // would charge the server for messages it deliberately refused.
  const expected = stats.accepted * live.length;

  console.log('\n');
  console.log('── results ──────────────────────────────────');
  console.log(`  duration            ${elapsed.toFixed(1)}s`);
  console.log(`  connections         ${live.length}`);
  console.log(`  sends attempted     ${stats.sent}`);
  console.log(`  sends accepted      ${stats.accepted}`);
  if (stats.rateLimited > 0) {
    console.log(
      `  sends rate-limited  ${stats.rateLimited}  ` +
        `(per-connection limit is ~2/s; lower --rate to avoid this)`,
    );
  }
  for (const [what, n] of stats.errorCodes) {
    console.log(`  server said         ${n}x  ${what}`);
  }
  console.log(`  deliveries observed ${stats.received}`);
  console.log(
    `  delivery rate       ${Math.round(stats.received / elapsed).toLocaleString()}/s`,
  );
  console.log(
    `  observed / expected ${expected ? ((stats.received / expected) * 100).toFixed(1) : '0'}%` +
      `  (${expected.toLocaleString()} expected)`,
  );
  console.log('');
  console.log(`  latency p50         ${percentile(sorted, 50).toFixed(1)} ms`);
  console.log(`  latency p95         ${percentile(sorted, 95).toFixed(1)} ms`);
  console.log(`  latency p99         ${percentile(sorted, 99).toFixed(1)} ms`);
  console.log(`  latency max         ${(sorted[sorted.length - 1] ?? 0).toFixed(1)} ms`);

  if (metrics) {
    console.log('');
    console.log('── server ───────────────────────────────────');
    console.log(`  frames encoded      ${metrics.frames_encoded.toLocaleString()}`);
    console.log(`  frames delivered    ${metrics.frames_delivered.toLocaleString()}`);
    console.log(`  bytes encoded       ${(metrics.bytes_encoded / 1e6).toFixed(2)} MB`);
    console.log(`  dropped consumers   ${metrics.dropped_slow_consumers}`);
    console.log('');
    // This is the headline number: deliveries per serialization. Encoding once
    // per event instead of once per socket is where it comes from.
    console.log(`  fanout ratio        ${metrics.fanout_ratio.toFixed(1)}x`);
    console.log(
      `    (each encode served ${metrics.fanout_ratio.toFixed(1)} sockets; a naive server\n` +
        `     would have run serde that many times instead of once)`,
    );
    const observedVsServer = metrics.frames_delivered
      ? (stats.received / metrics.frames_delivered) * 100
      : 0;
    if (observedVsServer < 90 && metrics.dropped_slow_consumers === 0) {
      console.log('');
      console.log(
        `  note: the server enqueued more than this client counted, with zero\n` +
          `  dropped consumers — the gap is this Node process, not the server.\n` +
          `  Run fewer clients per load-test host to measure the ceiling.`,
      );
    }
  }
  process.exit(0);
}

main().catch((err) => {
  console.error('\nload test failed:', err.message);
  process.exit(1);
});
