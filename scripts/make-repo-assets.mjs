// Generates repo presentation assets from build/icon.svg:
//   docs/logo.png         (256, transparent) — README header logo
//   docs/favicon-16.png    \
//   docs/favicon-32.png     > small PNGs for a docs site / GitHub Pages
//   docs/favicon-48.png    /
//   docs/favicon.ico       (16/32/48 bundle)
//
// Run with: npm run make:repo-assets
import { Resvg } from '@resvg/resvg-js';
import pngToIco from 'png-to-ico';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const svg = readFileSync(join(root, 'build', 'icon.svg'), 'utf8');

const renderPng = (size) =>
  Buffer.from(new Resvg(svg, { fitTo: { mode: 'width', value: size } }).render().asPng());

writeFileSync(join(root, 'docs', 'logo.png'), renderPng(256));
for (const size of [16, 32, 48]) {
  writeFileSync(join(root, 'docs', `favicon-${size}.png`), renderPng(size));
}
writeFileSync(join(root, 'docs', 'favicon.ico'), await pngToIco([48, 32, 16].map(renderPng)));

console.log('Wrote docs/logo.png (256), docs/favicon-{16,32,48}.png, docs/favicon.ico');
