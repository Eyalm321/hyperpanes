// Rasterizes build/icon.svg into the app icons electron-builder needs:
//   build/icon.png  (512x512, used as the dev window icon)
//   build/icon.ico  (multi-size, used for the packaged Windows app)
//
// Run with: npm run make:icon
import { Resvg } from '@resvg/resvg-js';
import pngToIco from 'png-to-ico';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const svg = readFileSync(join(root, 'build', 'icon.svg'), 'utf8');

const renderPng = (size) =>
  Buffer.from(new Resvg(svg, { fitTo: { mode: 'width', value: size } }).render().asPng());

// PNG for the dev window / general use.
writeFileSync(join(root, 'build', 'icon.png'), renderPng(512));

// ICO bundles several sizes so Windows can pick the right one.
const icoSizes = [256, 128, 64, 48, 32, 16];
const ico = await pngToIco(icoSizes.map(renderPng));
writeFileSync(join(root, 'build', 'icon.ico'), ico);

console.log('Wrote build/icon.png (512) and build/icon.ico (' + icoSizes.join(',') + ')');
