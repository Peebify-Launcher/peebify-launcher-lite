import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const srcDir = join(root, "src");
const langDir = join(srcDir, "languages");
const PLURAL = /_(zero|one|two|few|many|other)$/;

function flatten(obj, prefix = "", out = new Map(), errors = []) {
  for (const [k, v] of Object.entries(obj)) {
    const key = prefix ? `${prefix}.${k}` : k;
    if (typeof v === "string") out.set(key, v);
    else if (v && typeof v === "object" && !Array.isArray(v)) flatten(v, key, out, errors);
    else errors.push(`${key}: value must be text or a group of keys, not ${JSON.stringify(v)}`);
  }
  return out;
}

const placeholders = (s) => new Set([...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]));
const tags = (s) => new Set([...s.matchAll(/<(\w+)>/g)].map((m) => m[1]));

function load(file) {
  const text = readFileSync(join(langDir, file), "utf8").replace(/^﻿/, "");
  return JSON.parse(text);
}

const en = flatten(load("en.json"));

function checkTranslations(detailFor) {
  let failed = false;
  const files = readdirSync(langDir).filter((f) => f.endsWith(".json") && f !== "en.json");
  const translatable = [...en.keys()].filter((k) => !k.startsWith("_meta."));
  console.log(`en.json: ${translatable.length} strings`);

  for (const file of files) {
    const code = file.slice(0, -5);
    const errors = [];
    const warnings = [];
    let dict;
    try {
      dict = flatten(load(file), "", new Map(), errors);
    } catch (e) {
      console.log(`\n${file}: not valid JSON: ${e.message}`);
      failed = true;
      continue;
    }
    if (!dict.get("_meta.name")) errors.push(`_meta.name is missing (the language's own name, e.g. "Deutsch")`);

    for (const [key, value] of dict) {
      if (key.startsWith("_meta.")) continue;
      const source = en.get(key) ?? (PLURAL.test(key) ? en.get(key.replace(PLURAL, "_other")) : undefined);
      if (source === undefined) {
        errors.push(`${key}: not in en.json (renamed or removed?)`);
        continue;
      }
      const want = placeholders(source);
      for (const p of placeholders(value)) {
        if (!want.has(p)) errors.push(`${key}: {${p}} is not a placeholder in English`);
      }
      if (!PLURAL.test(key)) {
        for (const p of want) if (!value.includes(`{${p}}`)) warnings.push(`${key}: {${p}} is not used`);
      }
      const wantTags = tags(source);
      for (const tg of tags(value)) if (!wantTags.has(tg)) errors.push(`${key}: <${tg}> is not a tag in English`);
      for (const tg of wantTags) if (!tags(value).has(tg)) errors.push(`${key}: <${tg}>...</${tg}> is missing`);
    }

    const missing = translatable.filter((k) => {
      if (dict.has(k)) return false;
      if (PLURAL.test(k)) return ![...dict.keys()].some((d) => d.replace(PLURAL, "") === k.replace(PLURAL, ""));
      return true;
    });
    const done = translatable.length - missing.length;
    const pct = translatable.length ? Math.floor((done / translatable.length) * 100) : 100;
    console.log(`\n${file} (${dict.get("_meta.name") ?? code}): ${pct}% (${done}/${translatable.length})`);
    for (const e of errors) console.log(`  error   ${e}`);
    for (const w of warnings) console.log(`  warning ${w}`);
    if (errors.length) failed = true;
    if (code === detailFor) {
      console.log(`  ${missing.length} untranslated:`);
      for (const k of missing) console.log(`    ${k}: ${JSON.stringify(en.get(k))}`);
    }
  }
  if (!files.length) console.log("No translations yet. Copy en.json to <code>.json to start one.");
  return failed;
}

// ---- strings against the code ------------------------------------------------------
// A key counts as used when it (or its plural base) appears as a string anywhere in the code, or
// starts with the fixed part of a template such as `app.jobDone.title.${kind}`. t("...") and
// <Trans k="..."> must name a key that exists.

async function checkUsage() {
  const { default: ts } = await import("typescript");
  const files = walkFiles(srcDir, [], true).filter((f) => !f.startsWith(langDir));
  const used = { exact: new Set(), prefixes: new Set() };
  const errors = [];
  const exists = (key) => en.has(key) || en.has(`${key}_other`);
  for (const file of files) {
    const text = readFileSync(file, "utf8");
    const sf = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, file.endsWith("x") ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
    const where = (node) =>
      `${relative(root, file).replaceAll("\\", "/")}:${sf.getLineAndCharacterOfPosition(node.getStart()).line + 1}`;
    const visit = (node) => {
      if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) used.exact.add(node.text);
      else if (ts.isTemplateExpression(node) && node.head.text.includes(".")) used.prefixes.add(node.head.text);
      let key;
      if (ts.isCallExpression(node) && ts.isIdentifier(node.expression) && /^(t|tr)$/.test(node.expression.text)) {
        key = node.arguments[0];
      } else if (
        ts.isJsxAttribute(node) &&
        node.name.getText() === "k" &&
        ts.isJsxOpeningLikeElement(node.parent.parent) &&
        node.parent.parent.tagName.getText() === "Trans"
      ) {
        key = node.initializer;
      }
      if (key && ts.isStringLiteral(key) && !exists(key.text)) {
        errors.push(`${where(key)}: "${key.text}" is not in en.json`);
      }
      ts.forEachChild(node, visit);
    };
    visit(sf);
  }
  const isUsed = (key) => {
    const base = key.replace(PLURAL, "");
    if (used.exact.has(key) || used.exact.has(base)) return true;
    for (const p of used.prefixes) if (key.startsWith(p)) return true;
    return false;
  };
  for (const key of en.keys()) {
    if (key.startsWith("_meta.") || isUsed(key)) continue;
    errors.push(`${key}: in en.json but not used anywhere`);
  }
  console.log(`\nStrings against the code: ${errors.length ? `${errors.length} problems` : "ok"}`);
  for (const e of errors) console.log(`  error   ${e}`);
  return errors.length > 0;
}

// ---- hardcoded text finder -------------------------------------------------------

const PROSE_PROPS = new Set([
  "title", "label", "description", "subtitle", "message", "body", "text", "placeholder",
  "tooltip", "hint", "heading", "caption", "summary", "detail", "confirmLabel",
  "cancelLabel", "actionLabel", "emptyText", "aria-label", "alt", "ariaLabel", "header",
]);
const SKIP_ATTRS = new Set([
  "className", "key", "id", "href", "src", "d", "viewBox", "type", "role", "style",
  "xmlns", "fill", "stroke", "target", "rel", "autoComplete", "inputMode", "layoutId",
]);
const PROSE_CALLS = new Set(["toast", "notify", "pushToast", "alert", "confirm", "setError", "setMessage", "setStatus"]);

const looksLikeProse = (s) => /[A-Za-z]{2}/.test(s) && (/^[A-Z]/.test(s.trim()) || /[a-z]\s+[a-z]/i.test(s));

function walkFiles(path, out = [], withTests = false) {
  const st = statSync(path);
  if (st.isDirectory()) for (const e of readdirSync(path)) walkFiles(join(path, e), out, withTests);
  else if (/\.(ts|tsx)$/.test(path) && !/\.d\.ts$/.test(path) && (withTests || !/\.test\.tsx?$/.test(path))) {
    out.push(path);
  }
  return out;
}

async function findHardcoded(paths) {
  const { default: ts } = await import("typescript");
  const targets = (paths.length ? paths : ["src"]).flatMap((p) => walkFiles(resolve(root, p)));
  const hits = [];
  for (const file of targets) {
    if (file.includes(`${join("src", "languages")}`)) continue;
    const text = readFileSync(file, "utf8");
    const lines = text.split("\n");
    const sf = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, file.endsWith("x") ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
    const report = (node, value) => {
      const line = sf.getLineAndCharacterOfPosition(node.getStart()).line;
      if (/i18n-ignore/.test(lines[line] ?? "") || /i18n-ignore/.test(lines[line - 1] ?? "")) return;
      hits.push(`${relative(root, file).replaceAll("\\", "/")}:${line + 1}  ${JSON.stringify(value.trim().replace(/\s+/g, " "))}`);
    };
    const inside = (node, test) => {
      for (let p = node.parent; p; p = p.parent) if (test(p)) return p;
      return undefined;
    };
    const calleeName = (call) => {
      const e = call.expression;
      return ts.isIdentifier(e) ? e.text : ts.isPropertyAccessExpression(e) ? e.name.text : "";
    };
    const visit = (node) => {
      if (ts.isJsxText(node)) {
        if (/[A-Za-z]{2}/.test(node.text.replace(/&\w+;/g, ""))) report(node, node.text);
      } else if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node) || ts.isTemplateExpression(node)) {
        const value = ts.isTemplateExpression(node) ? node.getText().slice(1, -1) : node.text;
        const parent = node.parent;
        const call = inside(node, ts.isCallExpression);
        const callName = call ? calleeName(call) : "";
        const attr = inside(node, ts.isJsxAttribute);
        const attrName = attr ? attr.name.getText() : "";
        if (!/[A-Za-z]{2}/.test(value) || SKIP_ATTRS.has(attrName) || attrName.startsWith("data-")) {
        } else if (callName === "t" ||/^(log|console)$/.test(call?.expression?.expression?.getText?.() ?? "")) {
        } else if (
          ts.isCaseClause(parent) ||
          ts.isElementAccessExpression(parent) ||
          (ts.isBinaryExpression(parent) &&
            [ts.SyntaxKind.EqualsEqualsEqualsToken, ts.SyntaxKind.ExclamationEqualsEqualsToken,
              ts.SyntaxKind.EqualsEqualsToken, ts.SyntaxKind.ExclamationEqualsToken].includes(parent.operatorToken.kind))
        ) {
        } else if (ts.isImportDeclaration(parent) || ts.isExportDeclaration(parent) || ts.isLiteralTypeNode(parent)) {
        } else if (ts.isJsxAttribute(parent)) {
          const name = parent.name.getText();
          if (!SKIP_ATTRS.has(name) && !name.startsWith("data-") && (PROSE_PROPS.has(name) || looksLikeProse(value))) report(node, value);
        } else if (ts.isPropertyAssignment(parent) && parent.initializer === node) {
          if (PROSE_PROPS.has(parent.name.getText().replace(/["']/g, "")) && looksLikeProse(value)) report(node, value);
        } else if (ts.isCallExpression(parent) && PROSE_CALLS.has(calleeName(parent))) {
          if (looksLikeProse(value)) report(node, value);
        } else if (looksLikeProse(value)) {
          const jsx = inside(node, (p) => ts.isJsxExpression(p) || ts.isCallExpression(p));
          if (jsx && ts.isJsxExpression(jsx)) report(node, value);
        }
      }
      ts.forEachChild(node, visit);
    };
    visit(sf);
  }
  for (const h of hits) console.log(h);
  console.log(`\n${hits.length} possible hardcoded strings. Mark intentional ones with // i18n-ignore.`);
  return hits.length;
}

const args = process.argv.slice(2);
if (args[0] === "--hardcoded") {
  const n = await findHardcoded(args.slice(1).filter((a) => !a.startsWith("--")));
  process.exit(args.includes("--strict") && n > 0 ? 1 : 0);
} else {
  const translationsFailed = checkTranslations(args[0]);
  const usageFailed = await checkUsage();
  process.exit(translationsFailed || usageFailed ? 1 : 0);
}
