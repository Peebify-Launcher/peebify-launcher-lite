#!/usr/bin/env node

// ------------ Installer Build Script ------------
// Builds the whole setup exe in one go: the launcher, the helper exes and DLL, then the installer stub, and
// attaches the payload to the stub as dist/installer/Peebify-Launcher-Lite-Setup-<version>.exe, then verifies it.

'use strict';

const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const { spawnSync } = require('child_process');

const { rustEnv, findLocalPaths } = require('./rust-flags');
const { findToolingMarkers, payloadProblems, frontendProblems } = require('./ship-guard');

const REPO_ROOT = path.resolve(__dirname, '..');

const readJson = (file) => JSON.parse(fs.readFileSync(file, 'utf-8'));
const PKG = readJson(path.join(REPO_ROOT, 'package.json'));
const TAURI_CONF = readJson(path.join(REPO_ROOT, 'src-tauri', 'tauri.conf.json'));
const VERSION = PKG.version;
const PRODUCT_NAME = TAURI_CONF.productName;
const BUILD_TYPE = PKG.buildType || 'stable';
const MAIN_BINARY = `${PRODUCT_NAME}.exe`;
const TARGET_RELEASE = path.join(REPO_ROOT, 'target', 'release');

function fail(msg) {
    console.error(`[build-installer] ${msg}`);
    process.exit(1);
}

const FORBIDDEN_IMPORTS = [/^vcruntime140/i, /^msvcp140/i, /^concrt140/i, /^mfc140/i];

function openPe(file) {
    const b = fs.readFileSync(file);
    const pe = b.readUInt32LE(0x3c);
    if (b.readUInt32LE(pe) !== 0x00004550) throw new Error(`${file}: not a PE image`);
    const coff = pe + 4;
    const numSections = b.readUInt16LE(coff + 2);
    const optSize = b.readUInt16LE(coff + 16);
    const opt = coff + 20;
    const pe32plus = b.readUInt16LE(opt) === 0x20b;
    const directories = pe32plus ? opt + 112 : opt + 96;

    const sections = [];
    for (let i = 0; i < numSections; i++) {
        const s = opt + optSize + i * 40;
        sections.push({
            va: b.readUInt32LE(s + 12),
            size: Math.max(b.readUInt32LE(s + 8), b.readUInt32LE(s + 16)),
            raw: b.readUInt32LE(s + 20),
        });
    }
    return {
        b,
        directory: (index) => b.readUInt32LE(directories + index * 8),
        toOffset: (rva) => {
            const s = sections.find((x) => rva >= x.va && rva < x.va + x.size);
            return s ? s.raw + (rva - s.va) : -1;
        },
        cstr: (off) => b.toString('latin1', off, b.indexOf(0, off)),
    };
}

function peImports(file) {
    const { b, directory, toOffset, cstr } = openPe(file);
    const importRva = directory(1);
    if (!importRva) return [];

    const names = [];
    for (let off = toOffset(importRva); off >= 0; off += 20) {
        const nameRva = b.readUInt32LE(off + 12);
        if (!nameRva) break;
        names.push(cstr(toOffset(nameRva)));
    }
    return names;
}

function peExports(file) {
    const { b, directory, toOffset, cstr } = openPe(file);
    const exportRva = directory(0);
    if (!exportRva) return [];
    const table = toOffset(exportRva);
    if (table < 0) return [];

    const count = b.readUInt32LE(table + 0x18);
    const namesRva = b.readUInt32LE(table + 0x20);
    const names = toOffset(namesRva);
    if (names < 0) return [];

    return Array.from({ length: count }, (_, i) =>
        cstr(toOffset(b.readUInt32LE(names + i * 4)))
    );
}

function assertExports(file, symbol) {
    if (!peExports(file).includes(symbol)) {
        fail(
            `${path.basename(file)} does not export ${symbol} — it was built without the ` +
            `"stub" feature, so the game would load it and nothing would happen.`
        );
    }
    console.log(`[build-installer] export check OK: ${path.basename(file)} exports ${symbol}`);
}

function assertNoRedistImports(file) {
    const bad = peImports(file).filter((n) => FORBIDDEN_IMPORTS.some((re) => re.test(n)));
    if (bad.length) {
        fail(
            `${path.basename(file)} imports ${bad.join(', ')} — it would fail to start on a machine ` +
            `without the Visual C++ redistributable. Build with RUSTFLAGS="-C target-feature=+crt-static".`
        );
    }
    console.log(`[build-installer] import check OK: ${path.basename(file)} needs no VC++ redistributable`);
}

function assertNoLocalPaths(file) {
    const found = findLocalPaths(file);
    if (found.length) {
        fail(
            `${path.basename(file)} contains paths from this machine (${found.join(', ')}), so it would ` +
            `reveal the build user and folders to anyone who reads the binary.\n` +
            `         Build through this script so --remap-path-prefix is applied (scripts/rust-flags.js).`
        );
    }
    console.log(`[build-installer] path check OK: ${path.basename(file)} names no local user or folder`);
}

function assertNoBuildTooling(file) {
    const found = findToolingMarkers(file);
    if (found.length) {
        fail(
            `${path.basename(file)} mentions build tooling (${found.join(', ')}). Build scripts must not ` +
            `be compiled into anything that ships.`
        );
    }
}

function assertShippable(file) {
    assertNoLocalPaths(file);
    assertNoBuildTooling(file);
}

function failOnProblems(what, problems) {
    if (!problems.length) return;
    fail(
        `${what} holds files that must not ship:\n` +
        problems.map((p) => `         ${p}`).join('\n')
    );
}

function run(cmd, args, opts = {}) {
    console.log(`[build-installer] $ ${cmd} ${args.join(' ')}`);
    const started = Date.now();
    const res = spawnSync(cmd, args, {
        cwd: REPO_ROOT,
        stdio: 'inherit',
        shell: process.platform === 'win32' && cmd === 'npm',
        ...opts,
    });
    const elapsed = `${((Date.now() - started) / 1000).toFixed(1)} s`;
    if (res.error) fail(`${cmd} could not be started after ${elapsed}: ${res.error.message}`);
    if (res.status !== 0) {
        const signal = res.signal ? ` (signal ${res.signal})` : '';
        fail(`${cmd} exited with ${res.status}${signal} after ${elapsed}`);
    }
    console.log(`[build-installer] ${path.basename(cmd)} finished in ${elapsed}`);
}

function formatSize(bytes) {
    return bytes >= 1024 * 1024
        ? `${(bytes / 1024 / 1024).toFixed(2)} MB`
        : `${(bytes / 1024).toFixed(1)} KB`;
}

function listFiles(dir) {
    return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
        const full = path.join(dir, entry.name);
        if (entry.isDirectory()) return listFiles(full);
        return entry.isFile() ? [{ file: full, size: fs.statSync(full).size }] : [];
    });
}

function cargoPackageVersion(file) {
    const text = fs.readFileSync(file, 'utf-8');
    const start = text.search(/^\[package\][ \t]*$/m);
    if (start < 0) return undefined;
    const rest = text.slice(start + '[package]'.length);
    const next = rest.search(/^\[/m);
    const section = next < 0 ? rest : rest.slice(0, next);
    const match = section.match(/^version\s*=\s*"([^"]+)"/m);
    return match ? match[1] : undefined;
}

function assertVersionsAgree() {
    const copies = [
        ['package.json', PKG.version],
        ['src-tauri/tauri.conf.json', TAURI_CONF.version],
        ['src-tauri/Cargo.toml', cargoPackageVersion(path.join(REPO_ROOT, 'src-tauri', 'Cargo.toml'))],
        ['webui/package.json', readJson(path.join(REPO_ROOT, 'webui', 'package.json')).version],
    ];
    if (VERSION && copies.every(([, version]) => version === VERSION)) {
        console.log(`[build-installer] version check OK: every version field reads ${VERSION}`);
        return;
    }
    fail(
        'version fields disagree, so the installed launcher would report a different version than the ' +
        'installer it came from:\n' +
        copies.map(([file, version]) => `         ${file}: ${version || '(missing)'}`).join('\n') +
        '\n         Set every one of them to the same version.'
    );
}

function assertLauncherVersion(file) {
    const stat = fs.statSync(file);
    console.log(`[build-installer] launcher exe: ${file} (built ${stat.mtime.toISOString()})`);
    const marker = Buffer.from(`${VERSION}\0`, 'utf16le');
    if (fs.readFileSync(file).indexOf(marker) < 0) {
        fail(
            `${path.basename(file)} does not report version ${VERSION} in its version resource, so it is ` +
            'a stale build. Run without --skip-app-build.'
        );
    }
    console.log(`[build-installer] version resource OK: ${path.basename(file)} reports ${VERSION}`);
    const newerSources = ['package.json', 'src-tauri/tauri.conf.json'].filter(
        (rel) => fs.statSync(path.join(REPO_ROOT, rel)).mtimeMs > stat.mtimeMs
    );
    if (newerSources.length) {
        console.warn(
            `[build-installer] warning: ${newerSources.join(' and ')} changed after ${path.basename(file)} ` +
            'was built. Run without --skip-app-build if that edit affects the app.'
        );
    }
}

const DIST_FILE_BUDGET = 350 * 1024;
const DIST_TOTAL_BUDGET = 5 * 1024 * 1024;

function checkDistBudget() {
    const distDir = path.join(REPO_ROOT, 'webui', 'dist');
    if (!fs.existsSync(distDir)) {
        console.warn(`[build-installer] warning: ${distDir} not found, skipping the size budget check`);
        return;
    }
    const files = listFiles(distDir);
    const total = files.reduce((sum, f) => sum + f.size, 0);
    console.log(`[build-installer] webui/dist: ${files.length} files, ${formatSize(total)}`);
    const problems = files
        .filter((f) => f.size > DIST_FILE_BUDGET)
        .sort((a, b) => b.size - a.size)
        .map((f) => {
            const rel = path.relative(REPO_ROOT, f.file).replace(/\\/g, '/');
            return `${rel} is ${formatSize(f.size)}, over the ${formatSize(DIST_FILE_BUDGET)} per file budget`;
        });
    if (total > DIST_TOTAL_BUDGET) {
        problems.push(`webui/dist totals ${formatSize(total)}, over the ${formatSize(DIST_TOTAL_BUDGET)} budget`);
    }
    if (!problems.length) return;
    const message =
        'webui/dist is over its size budget:\n' +
        problems.map((p) => `         ${p}`).join('\n') +
        '\n         Re-encode the oversized files in webui/public (webp, wallpapers <= 1920x1080) and rebuild.';
    if (strict) fail(message);
    console.warn(`[build-installer] warning: ${message}`);
}

function printPayloadTable(dir) {
    const files = listFiles(dir).sort((a, b) => b.size - a.size);
    const total = files.reduce((sum, f) => sum + f.size, 0);
    console.log(`[build-installer] payload: ${files.length} files, ${formatSize(total)}`);
    for (const f of files) {
        const rel = path.relative(dir, f.file).replace(/\\/g, '/');
        console.log(`[build-installer]   ${formatSize(f.size).padStart(10)}  ${rel}`);
    }
}

const skipAppBuild = process.argv.includes('--skip-app-build');
const strict = process.argv.includes('--strict');
const helpersOnly = process.argv.includes('--helpers-only');

const HELPER_BINS_BUILD = [
    'build', '-p', 'peebify-helpers', '--bins', '--release', '--features', 'overlay-runtime',
];
const HELPER_STUB_BUILD = [
    'rustc', '-p', 'peebify-helpers', '--lib', '--release', '--features', 'stub', '--crate-type', 'cdylib',
];

const HELPER_ARTIFACTS = [
    { file: 'peebify-fps-helper.exe', export: null },
    { file: 'peebify_helpers.dll', export: 'WndProc' },
    { file: 'peebify-mod-loader.exe', export: null },
    { file: 'peebify-overlay-helper.exe', export: null },
];

function buildHelpers(destination) {
    const env = rustEnv({ ...process.env, PEEBIFY_VERSION: VERSION });
    run('cargo', HELPER_BINS_BUILD, { env });
    run('cargo', HELPER_STUB_BUILD, { env });

    if (destination) fs.mkdirSync(destination, { recursive: true });
    for (const { file, export: required } of HELPER_ARTIFACTS) {
        const from = path.join(TARGET_RELEASE, file);
        if (!fs.existsSync(from)) fail(`helper artifact missing: ${from}`);
        assertNoRedistImports(from);
        assertShippable(from);
        if (required) assertExports(from, required);
        if (!destination) continue;
        fs.copyFileSync(from, path.join(destination, file));
        console.log(`[build-installer] bundled ${file}`);
    }
}

if (helpersOnly) {
    console.log(`[build-installer] --helpers-only: building the helpers for ${VERSION} into ${TARGET_RELEASE}`);
    buildHelpers(null);
    console.log(`[build-installer] helpers built: ${HELPER_ARTIFACTS.map((a) => a.file).join(', ')}`);
    process.exit(0);
}

function assertDepsInstalled(label, dir, binaries) {
    const binDir = path.join(dir, 'node_modules', '.bin');
    const missing = binaries.filter((b) => !fs.existsSync(path.join(binDir, b)));
    if (missing.length === 0) return;
    const where = (path.relative(REPO_ROOT, dir) || '.').replace(/\\/g, '/');
    const hasLockfile = fs.existsSync(path.join(dir, 'package-lock.json'));
    fail(
        `${label} dependencies are not installed (missing: ${missing.join(', ')}).\n` +
        `         Run:  npm --prefix ${where} ${hasLockfile ? 'ci' : 'install'}`
    );
}

if (!['stable', 'beta'].includes(BUILD_TYPE)) {
    fail(`package.json "buildType" must be "stable" or "beta" (got "${BUILD_TYPE}")`);
}

assertVersionsAgree();

if (!skipAppBuild) {
    assertDepsInstalled('webui', path.join(REPO_ROOT, 'webui'), ['tsc', 'vite']);
    assertDepsInstalled('root', REPO_ROOT, ['tauri']);
    console.log(`[build-installer] building ${VERSION} for the ${BUILD_TYPE} channel`);
    run('npm', ['run', 'tauri', '--', 'build', '--no-bundle'], {
        env: rustEnv({ ...process.env, PEEBIFY_BUILD_TYPE: BUILD_TYPE }),
    });
} else {
    console.log('[build-installer] --skip-app-build: reusing existing launcher build');
}

const launcherCandidates = ['peebify-launcher.exe', MAIN_BINARY]
    .map((name) => path.join(TARGET_RELEASE, name))
    .filter(fs.existsSync)
    .map((file) => ({ file, mtimeMs: fs.statSync(file).mtimeMs }));
if (launcherCandidates.length > 1) {
    console.warn(
        `[build-installer] warning: both ${launcherCandidates.map((c) => path.basename(c.file)).join(' and ')} ` +
        `exist in ${TARGET_RELEASE}; using the newer one`
    );
}
const launcherExe = launcherCandidates.reduce(
    (newest, c) => (!newest || c.mtimeMs > newest.mtimeMs ? c : newest),
    null
)?.file;
if (!launcherExe) {
    fail(`launcher exe not found in ${TARGET_RELEASE}; run without --skip-app-build`);
}
assertNoRedistImports(launcherExe);
assertShippable(launcherExe);
assertLauncherVersion(launcherExe);
checkDistBudget();
failOnProblems('webui/dist (embedded in the launcher)', frontendProblems(path.join(REPO_ROOT, 'webui', 'dist')));
console.log('[build-installer] frontend check OK: webui/dist holds only web assets');

const payloadDir = path.join(REPO_ROOT, 'dist', 'payload');
fs.rmSync(payloadDir, { recursive: true, force: true });
fs.mkdirSync(payloadDir, { recursive: true });
fs.copyFileSync(launcherExe, path.join(payloadDir, MAIN_BINARY));

const resources = (TAURI_CONF.bundle && TAURI_CONF.bundle.resources) || {};
for (const [src, dest] of Object.entries(resources)) {
    const from = path.resolve(REPO_ROOT, 'src-tauri', src);
    const to = path.join(payloadDir, dest);
    if (!fs.existsSync(from)) fail(`bundle resource missing: ${from}`);
    fs.mkdirSync(path.dirname(to), { recursive: true });
    fs.cpSync(from, to, { recursive: true });
}

buildHelpers(path.join(payloadDir, 'resources'));

const expectedPayload = [
    MAIN_BINARY,
    ...Object.entries(resources).map(([src, dest]) => {
        const from = path.resolve(REPO_ROOT, 'src-tauri', src);
        return fs.statSync(from).isDirectory() ? `${dest.replace(/\\/g, '/')}/` : dest.replace(/\\/g, '/');
    }),
    ...HELPER_ARTIFACTS.map((a) => `resources/${a.file}`),
];
failOnProblems('the installer payload', payloadProblems(payloadDir, expectedPayload));
console.log(`[build-installer] payload check OK: ${expectedPayload.length} expected entries, nothing else`);

run('cargo', ['build', '-p', 'peebify-installer', '--release'], {
    env: rustEnv({ ...process.env, PEEBIFY_VERSION: VERSION }),
});
const stub = path.join(TARGET_RELEASE, 'peebify-installer.exe');
if (!fs.existsSync(stub)) fail(`installer stub not found at ${stub}`);
assertNoRedistImports(stub);
assertShippable(stub);

const outDir = path.join(REPO_ROOT, 'dist', 'installer');
fs.mkdirSync(outDir, { recursive: true });
const outExe = path.join(outDir, `Peebify-Launcher-Lite-Setup-${VERSION}.exe`);
printPayloadTable(payloadDir);
run(stub, [
    'pack',
    '--stub', stub,
    '--payload-dir', payloadDir,
    '--out', outExe,
    '--version', VERSION,
    '--main-binary', MAIN_BINARY,
], { shell: false });

run(stub, ['inspect', '--file', outExe], { shell: false });
assertNoRedistImports(outExe);
assertShippable(outExe);

const bytes = fs.readFileSync(outExe);
const sha512 = crypto.createHash('sha512').update(bytes).digest();

console.log('');
console.log(`[build-installer] built ${outExe}`);
console.log(`[build-installer] size: ${(bytes.length / 1024 / 1024).toFixed(2)} MB`);
console.log(`[build-installer] sha512 (hex): ${sha512.toString('hex')}`);
const sourcemapDir = path.join(REPO_ROOT, 'dist', 'sourcemaps', VERSION);
if (fs.existsSync(sourcemapDir)) {
    console.log(`[build-installer] renderer source maps: ${sourcemapDir} (archive them with this installer)`);
} else {
    console.log(`[build-installer] renderer source maps missing: ${sourcemapDir} was not written by this build`);
}
console.log('');
console.log('Next: attach the installer to a release of this repository.');
console.log('      The launcher does not update itself, so users download each version by hand.');
