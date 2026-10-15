// Renders the promo deliverables into promo/out/.
//   node scripts/render.mjs              -> main, vertical, hero, filmstrip
//   node scripts/render.mjs main hero    -> just those steps
import {spawnSync} from 'node:child_process';
import {mkdirSync, rmSync, statSync} from 'node:fs';
import {createRequire} from 'node:module';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const out = path.join(root, 'out');
mkdirSync(out, {recursive: true});

const require = createRequire(import.meta.url);
const cli = path.join(path.dirname(require.resolve('@remotion/cli/package.json')), 'remotion-cli.js');

const run = (cmd, args) => {
  console.log(`\n> ${path.basename(cmd)} ${args.join(' ')}`);
  const r = spawnSync(cmd, args, {cwd: root, stdio: 'inherit'});
  if (r.error) throw r.error;
  if (r.status !== 0) process.exit(r.status ?? 1);
};
const remotion = (...args) => run(process.execPath, [cli, ...args]);
const have = (bin) => spawnSync(bin, ['-version'], {stdio: 'ignore'}).status === 0;
const duration = (file) =>
  Number(
    spawnSync('ffprobe', ['-v', 'error', '-show_entries', 'format=duration', '-of', 'csv=p=0', file], {
      encoding: 'utf8',
    }).stdout.trim(),
  );

const H264 = ['--codec=h264', '--crf=16', '--x264-preset=slow', '--pixel-format=yuv420p'];

// One frame every 2 s, tiled into a contact sheet for QC.
const filmstrip = (src, dest, cols, tileW) => {
  const n = Math.floor(duration(src) / 2) + 1;
  const rows = Math.ceil(n / cols);
  run('ffmpeg', [
    '-y', '-v', 'error', '-i', src,
    '-vf', `fps=1/2,scale=${tileW}:-2,tile=${cols}x${rows}:padding=10:margin=10:color=0x0B0C0E`,
    '-frames:v', '1', '-update', '1', dest,
  ]);
};

const steps = {
  main: () => remotion('render', 'src/index.ts', 'Promo', 'out/casecomplete-promo.mp4', ...H264),
  vertical: () =>
    remotion('render', 'src/index.ts', 'PromoVertical', 'out/casecomplete-promo-vertical.mp4', ...H264),
  hero: () => {
    if (have('ffmpeg')) {
      // Render crisp at 960 px, then build a palette-optimised looping GIF at 15 fps.
      remotion('render', 'src/index.ts', 'Hero', 'out/hero-src.mp4', '--codec=h264', '--crf=10', '--scale=0.5');
      run('ffmpeg', [
        '-y', '-v', 'error', '-i', 'out/hero-src.mp4',
        '-vf',
        'fps=15,split[a][b];[a]palettegen=max_colors=160:stats_mode=full[p];[b][p]paletteuse=dither=sierra2_4a',
        '-loop', '0', 'out/casecomplete-hero.gif',
      ]);
      rmSync(path.join(out, 'hero-src.mp4'), {force: true});
    } else {
      remotion(
        'render', 'src/index.ts', 'Hero', 'out/casecomplete-hero.gif',
        '--codec=gif', '--scale=0.5', '--every-nth-frame=2', '--number-of-gif-loops=0',
      );
    }
  },
  filmstrip: () => {
    if (!have('ffmpeg')) {
      console.log('ffmpeg not on PATH: skipping filmstrip');
      return;
    }
    filmstrip('out/casecomplete-promo.mp4', 'out/filmstrip.png', 3, 640);
    try {
      statSync(path.join(out, 'casecomplete-promo-vertical.mp4'));
      filmstrip('out/casecomplete-promo-vertical.mp4', 'out/filmstrip-vertical.png', 7, 270);
    } catch {
      // vertical not rendered
    }
  },
};

const wanted = process.argv.slice(2);
for (const name of wanted.length ? wanted : Object.keys(steps)) {
  if (!steps[name]) {
    console.error(`unknown step: ${name} (use ${Object.keys(steps).join(', ')})`);
    process.exit(2);
  }
  steps[name]();
}

for (const f of ['casecomplete-promo.mp4', 'casecomplete-promo-vertical.mp4', 'casecomplete-hero.gif', 'filmstrip.png']) {
  try {
    console.log(`${f.padEnd(36)} ${(statSync(path.join(out, f)).size / 1048576).toFixed(2)} MB`);
  } catch {
    // not produced in this run
  }
}
