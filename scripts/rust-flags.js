'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');

const REPO_ROOT = path.resolve(__dirname, '..');
const FLAG_SEPARATOR = '\x1f';

function cargoHome() {
    return process.env.CARGO_HOME || path.join(os.homedir(), '.cargo');
}

function remapFlags() {
    const pairs = [
        [os.homedir(), 'home'],
        [cargoHome(), 'cargo'],
        [REPO_ROOT, 'peebify'],
    ];
    return pairs
        .filter(([from]) => from)
        .flatMap(([from, to]) => ['--remap-path-prefix', `${from}=${to}`]);
}

const SYSTEM_UCRT_FLAGS = [
    '-C', 'link-arg=/NODEFAULTLIB:libucrt.lib',
    '-C', 'link-arg=/DEFAULTLIB:ucrt.lib',
];

const SQLITE_TRIM_FLAGS = [
    '-USQLITE_ENABLE_COLUMN_METADATA',
    '-USQLITE_ENABLE_DBSTAT_VTAB',
    '-USQLITE_ENABLE_FTS3',
    '-USQLITE_ENABLE_FTS3_PARENTHESIS',
    '-USQLITE_ENABLE_FTS5',
    '-USQLITE_ENABLE_JSON1',
    '-USQLITE_ENABLE_LOAD_EXTENSION',
    '-USQLITE_ENABLE_RTREE',
    '-USQLITE_ENABLE_STAT4',
    '-USQLITE_SOUNDEX',
    '-DSQLITE_OMIT_JSON',
    '-DSQLITE_OMIT_LOAD_EXTENSION',
].join(' ');

function rustEnv(base = process.env) {
    const inherited = base.CARGO_ENCODED_RUSTFLAGS
        ? base.CARGO_ENCODED_RUSTFLAGS.split(FLAG_SEPARATOR)
        : (base.RUSTFLAGS || '').split(/\s+/).filter(Boolean);
    const flags = ['-C', 'target-feature=+crt-static', ...SYSTEM_UCRT_FLAGS, ...remapFlags(), ...inherited];
    const env = {
        ...base,
        CARGO_ENCODED_RUSTFLAGS: flags.join(FLAG_SEPARATOR),
        LIBSQLITE3_FLAGS: SQLITE_TRIM_FLAGS,
    };
    delete env.RUSTFLAGS;
    return env;
}

function localPathNeedles() {
    const needles = new Set();
    const add = (value) => {
        if (!value || value.length < 4) return;
        needles.add(value);
        needles.add(value.replace(/\\/g, '/'));
    };
    add(os.homedir());
    add(REPO_ROOT);
    add(cargoHome());
    const user = os.userInfo().username;
    if (user) {
        add(`\\Users\\${user}`);
        add(`\\Documents and Settings\\${user}`);
    }
    return [...needles];
}

function findLocalPaths(file) {
    const bytes = fs.readFileSync(file);
    const latin = bytes.toString('latin1').toLowerCase();
    const wide = bytes.toString('utf16le').toLowerCase();
    const wideOdd = bytes.subarray(1).toString('utf16le').toLowerCase();
    return localPathNeedles().filter((needle) => {
        const lower = needle.toLowerCase();
        return latin.includes(lower) || wide.includes(lower) || wideOdd.includes(lower);
    });
}

module.exports = { rustEnv, remapFlags, findLocalPaths, localPathNeedles };
