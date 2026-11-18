// Vérifie l'intégrité des fichiers de traduction.
// Pour chaque langue : JSON valide, ensemble de clés identique à en.json,
// structure imbriquée identique, et mêmes {placeholders} par clé.
// Usage : npm run check:i18n

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const localesDir = join(root, "src", "lib", "i18n", "locales");
const LOCALES = ["fr", "en", "es", "de", "it", "zh"];
const REFERENCE = "en";
const PLACEHOLDER = /\{([A-Za-z0-9_]+)\}/g;

function load(locale) {
  const path = join(localesDir, `${locale}.json`);
  const raw = readFileSync(path, "utf8");
  try {
    return JSON.parse(raw);
  } catch (error) {
    throw new Error(`${locale}.json : JSON invalide — ${error.message}`);
  }
}

// Aplatit un objet en `a.b.c` → valeur (feuilles uniquement).
function flatten(value, prefix = "", out = new Map()) {
  for (const [key, child] of Object.entries(value)) {
    const path = prefix ? `${prefix}.${key}` : key;
    if (child !== null && typeof child === "object" && !Array.isArray(child)) {
      flatten(child, path, out);
    } else {
      out.set(path, child);
    }
  }
  return out;
}

function placeholders(value) {
  if (typeof value !== "string") return [];
  const names = [];
  for (const match of value.matchAll(PLACEHOLDER)) names.push(match[1]);
  return [...new Set(names)].sort();
}

const reference = flatten(load(REFERENCE));
const referenceKeys = [...reference.keys()].sort();
const errors = [];

for (const locale of LOCALES) {
  let data;
  try {
    data = load(locale);
  } catch (error) {
    errors.push(error.message);
    continue;
  }

  const flat = flatten(data);
  const keys = [...flat.keys()].sort();
  const keySet = new Set(keys);

  const missing = referenceKeys.filter((k) => !keySet.has(k));
  const extra = keys.filter((k) => !reference.has(k));

  if (missing.length) {
    errors.push(`${locale} : ${missing.length} clé(s) manquante(s) — ex. ${missing.slice(0, 5).join(", ")}`);
  }
  if (extra.length) {
    errors.push(`${locale} : ${extra.length} clé(s) en trop — ex. ${extra.slice(0, 5).join(", ")}`);
  }

  let placeholderMismatches = 0;
  const examples = [];
  for (const [key, value] of reference) {
    if (!keySet.has(key)) continue;
    const expected = placeholders(value).join(",");
    const actual = placeholders(flat.get(key)).join(",");
    if (expected !== actual) {
      placeholderMismatches++;
      if (examples.length < 5) examples.push(`${key} (attendu {${expected}} / trouvé {${actual}})`);
    }
  }
  if (placeholderMismatches) {
    errors.push(
      `${locale} : ${placeholderMismatches} placeholder(s) incohérent(s) — ex. ${examples.join(" ; ")}`,
    );
  }

  if (!missing.length && !extra.length && !placeholderMismatches) {
    console.log(`✓ ${locale} : ${keys.length} clés, placeholders cohérents`);
  }
}

if (errors.length) {
  console.error(`\n✗ check-i18n : ${errors.length} erreur(s)`);
  for (const error of errors) console.error(`  - ${error}`);
  process.exit(1);
}

console.log(`\n✓ check-i18n : ${LOCALES.length} langues alignées (${referenceKeys.length} clés de référence)`);
