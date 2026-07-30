/**
 * Frontend build.
 *
 * One esbuild invocation produces one JS bundle and one CSS file into
 * `web/dist/assets/`, which the server mounts at `/assets` with a one-year
 * immutable cache. There is no framework, no transpiler chain, and no
 * dependency graph beyond esbuild itself — the whole build is this file.
 *
 *   node build.mjs           production bundle
 *   node build.mjs --dev     unminified, with sourcemaps
 *   node build.mjs --watch   rebuild on change (implies --dev)
 */

import * as esbuild from 'esbuild';
import { createHash } from 'node:crypto';
import { cp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, 'dist');
const assetDir = join(outDir, 'assets');

const watch = process.argv.includes('--watch');
const dev = watch || process.argv.includes('--dev');

/** Short content hash, so a deploy busts the immutable cache. */
function hash(bytes) {
  return createHash('sha256').update(bytes).digest('hex').slice(0, 10);
}

async function build() {
  await rm(outDir, { recursive: true, force: true });
  await mkdir(assetDir, { recursive: true });

  const result = await esbuild.build({
    entryPoints: [join(here, 'src/main.ts')],
    bundle: true,
    format: 'esm',
    // Every browser we target has supported ES2022 for years; downleveling
    // would only make the bundle bigger and slower.
    target: ['es2022', 'chrome111', 'firefox113', 'safari16'],
    minify: !dev,
    sourcemap: dev ? 'inline' : false,
    // Rewrite `./foo.ts` specifiers, which we use so Node can run the same
    // sources directly for tests without a build step.
    resolveExtensions: ['.ts', '.js'],
    write: false,
    outdir: assetDir,
    metafile: true,
    legalComments: 'none',
    logLevel: 'info',
  });

  const jsFile = result.outputFiles.find((f) => f.path.endsWith('.js'));
  if (!jsFile) throw new Error('esbuild produced no JavaScript output');

  // A second, tiny bundle: the theme stamp, which must run *before* the first
  // paint. It cannot be part of the main bundle (a module, therefore deferred
  // until after parsing) and it cannot be inlined into the page, because the
  // CSP ships without `unsafe-inline`. So it is its own classic script.
  const bootResult = await esbuild.build({
    entryPoints: [join(here, 'src/theme-boot.ts')],
    bundle: true,
    format: 'iife',
    target: ['es2022', 'chrome111', 'firefox113', 'safari16'],
    minify: !dev,
    resolveExtensions: ['.ts', '.js'],
    write: false,
    outdir: assetDir,
    legalComments: 'none',
  });
  const bootFile = bootResult.outputFiles.find((f) => f.path.endsWith('.js'));
  if (!bootFile) throw new Error('esbuild produced no theme-boot output');

  const css = await esbuild.transform(await readFile(join(here, 'styles.css'), 'utf8'), {
    loader: 'css',
    minify: !dev,
  });

  const jsName = dev ? 'app.js' : `app.${hash(jsFile.contents)}.js`;
  const bootName = dev ? 'theme.js' : `theme.${hash(bootFile.contents)}.js`;
  const cssName = dev ? 'app.css' : `app.${hash(css.code)}.css`;

  await writeFile(join(assetDir, jsName), jsFile.contents);
  await writeFile(join(assetDir, bootName), bootFile.contents);
  await writeFile(join(assetDir, cssName), css.code);

  // Point index.html at the hashed filenames.
  let html = await readFile(join(here, 'index.html'), 'utf8');
  html = html.replaceAll('/assets/theme.js', `/assets/${bootName}`);
  html = html.replaceAll('/assets/app.js', `/assets/${jsName}`);
  html = html.replaceAll('/assets/app.css', `/assets/${cssName}`);
  await writeFile(join(outDir, 'index.html'), html);

  const publicDir = join(here, 'public');
  await cp(publicDir, outDir, { recursive: true }).catch(() => {
    /* optional directory */
  });

  // The service worker, last: it needs the hashed asset names to precache, and
  // it lands at the site root because a worker's scope cannot be broader than
  // its own URL. Its filename is deliberately *not* hashed — the browser looks
  // for it at a fixed path — so the cache name carries the build identity
  // instead, and changing it is what retires the previous deployment's cache.
  const precache = ['/', '/index.html', `/assets/${jsName}`, `/assets/${bootName}`, `/assets/${cssName}`];
  const swResult = await esbuild.build({
    entryPoints: [join(here, 'src/sw.ts')],
    bundle: true,
    format: 'iife',
    target: ['es2022', 'chrome111', 'firefox113', 'safari16'],
    minify: !dev,
    resolveExtensions: ['.ts', '.js'],
    write: false,
    outdir: outDir,
    legalComments: 'none',
    define: {
      __CACHE_NAME__: JSON.stringify(`tc-${hash(Buffer.from(precache.join('|')))}`),
      __PRECACHE__: JSON.stringify(precache),
    },
  });
  const swFile = swResult.outputFiles.find((f) => f.path.endsWith('.js'));
  if (!swFile) throw new Error('esbuild produced no service worker output');
  await writeFile(join(outDir, 'sw.js'), swFile.contents);

  const jsKb = (jsFile.contents.byteLength / 1024).toFixed(1);
  const cssKb = (Buffer.byteLength(css.code) / 1024).toFixed(1);
  console.log(`built  js ${jsKb} kB  css ${cssKb} kB  ->  ${assetDir}`);

  // Surface the biggest inputs, so bundle growth is noticed while it is still
  // small enough to do something about.
  if (!dev && result.metafile) {
    const out = Object.values(result.metafile.outputs)[0];
    const top = Object.entries(out.inputs)
      .sort((a, b) => b[1].bytesInOutput - a[1].bytesInOutput)
      .slice(0, 6);
    for (const [file, info] of top) {
      console.log(`  ${(info.bytesInOutput / 1024).toFixed(1).padStart(6)} kB  ${file}`);
    }
  }
}

if (watch) {
  await build();
  const { watch: fsWatch } = await import('node:fs');
  let timer;
  const rebuild = () => {
    clearTimeout(timer);
    timer = setTimeout(() => build().catch((e) => console.error(e.message)), 60);
  };
  for (const dir of ['src', 'src/ui']) {
    fsWatch(join(here, dir), rebuild);
  }
  fsWatch(join(here, 'styles.css'), rebuild);
  fsWatch(join(here, 'index.html'), rebuild);
  console.log('watching for changes…');
} else {
  await build();
}
