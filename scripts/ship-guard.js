'use strict';

const fs = require('fs');
const path = require('path');

const FORBIDDEN_EXTENSIONS = new Set([
    '.js', '.mjs', '.cjs', '.ts', '.tsx', '.jsx', '.rs', '.toml', '.lock', '.ps1', '.bat', '.cmd', '.sh', '.py',
    '.key', '.pem', '.p12', '.pfx', '.crt', '.cer', '.sig', '.map', '.env', '.bak', '.log', '.md', '.sample',
]);

function walk(dir) {
    return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
        const full = path.join(dir, entry.name);
        if (entry.isDirectory()) return walk(full);
        return entry.isFile() ? [full] : [];
    });
}

function scriptNames() {
    return new Set(walk(__dirname).map((f) => path.basename(f)));
}

function toolingMarkers() {
    const names = [...scriptNames()];
    const stems = names.map((n) => path.basename(n, path.extname(n))).filter((s) => s.includes('-'));
    return [...new Set([...names, ...stems])];
}

function rel(root, file) {
    return path.relative(root, file).replace(/\\/g, '/');
}

function forbiddenReason(name, names, { allowExtensions = [] } = {}) {
    const ext = path.extname(name).toLowerCase();
    if (FORBIDDEN_EXTENSIONS.has(ext) && !allowExtensions.includes(ext)) {
        return `a ${ext} file is source, tooling or key material`;
    }
    if (name.startsWith('.')) return 'dotfiles are local configuration';
    if (names.has(name)) return 'it shares a name with a file in scripts/';
    return null;
}

function findToolingMarkers(file) {
    const bytes = fs.readFileSync(file);
    const latin = bytes.toString('latin1');
    const wide = bytes.toString('utf16le');
    const wideOdd = bytes.subarray(1).toString('utf16le');
    return toolingMarkers().filter((m) => latin.includes(m) || wide.includes(m) || wideOdd.includes(m));
}

function payloadProblems(payloadDir, expected) {
    const names = scriptNames();
    const files = walk(payloadDir);
    const problems = [];
    for (const file of files) {
        const name = rel(payloadDir, file);
        const allowed = expected.some((e) => (e.endsWith('/') ? name.startsWith(e) : name === e));
        if (!allowed) problems.push(`${name}: not part of the payload list`);
        const reason = forbiddenReason(path.basename(file), names);
        if (reason) problems.push(`${name}: ${reason}`);
        const found = findToolingMarkers(file);
        if (found.length) problems.push(`${name}: mentions ${found.join(', ')}`);
    }
    const present = new Set(files.map((f) => rel(payloadDir, f)));
    for (const e of expected.filter((x) => !x.endsWith('/'))) {
        if (!present.has(e)) problems.push(`${e}: expected in the payload but missing`);
    }
    return problems;
}

const FRONTEND_EXTENSIONS = new Set([
    '.html', '.js', '.css', '.png', '.webp', '.jpg', '.jpeg', '.svg', '.ico', '.woff', '.woff2',
]);

function frontendProblems(distDir) {
    const names = scriptNames();
    const problems = [];
    for (const file of walk(distDir)) {
        const name = rel(distDir, file);
        const ext = path.extname(file).toLowerCase();
        if (!FRONTEND_EXTENSIONS.has(ext)) problems.push(`${name}: a ${ext || 'extensionless'} file does not belong in the frontend`);
        const bundled = ext === '.js' && name.startsWith('assets/');
        const reason = forbiddenReason(path.basename(file), names, { allowExtensions: bundled ? ['.js'] : [] });
        if (reason) problems.push(`${name}: ${reason}`);
        if (['.html', '.js', '.css', '.svg'].includes(ext)) {
            const found = findToolingMarkers(file);
            if (found.length) problems.push(`${name}: mentions ${found.join(', ')}`);
        }
    }
    return problems;
}

module.exports = { findToolingMarkers, payloadProblems, frontendProblems };
