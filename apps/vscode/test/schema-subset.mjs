// A JSON Schema (draft 2020-12) checker for exactly the keywords schemas/cal/memory.v1.json
// and issues.v1.json use. ajv is only a transitive dependency of the workspace and
// `pnpm add --offline` could not resolve it (no cached registry metadata), so the tests
// validate with this instead. It throws on any keyword it does not implement, so a schema
// that starts using one fails here instead of passing unchecked.

const ANNOTATIONS = new Set(["$schema", "$id", "title", "description", "$defs"]);
const CHECKED = new Set([
  "type",
  "properties",
  "required",
  "items",
  "$ref",
  "anyOf",
  "enum",
  "minimum",
]);

const isObject = (v) => typeof v === "object" && v !== null && !Array.isArray(v);

function typeMatches(type, v) {
  switch (type) {
    case "null":
      return v === null;
    case "integer":
      return Number.isInteger(v);
    case "number":
      return typeof v === "number" && Number.isFinite(v);
    case "array":
      return Array.isArray(v);
    case "object":
      return isObject(v);
    case "string":
    case "boolean":
      return typeof v === type;
    default:
      throw new Error(`schema type ${type} is not implemented`);
  }
}

/** The errors of `value` against `root` (empty when it validates). */
export function validate(root, value) {
  const resolve = (ref) => {
    const name = ref.startsWith("#/$defs/") ? ref.slice("#/$defs/".length) : undefined;
    const def = name === undefined ? undefined : root.$defs?.[name];
    if (!def) throw new Error(`unresolved $ref ${ref}`);
    return def;
  };
  const check = (schema, v, at) => {
    for (const k of Object.keys(schema)) {
      if (!ANNOTATIONS.has(k) && !CHECKED.has(k)) {
        throw new Error(`schema keyword ${k} (at ${at}) is not implemented by schema-subset.mjs`);
      }
    }
    const errors = [];
    if (schema.$ref) errors.push(...check(resolve(schema.$ref), v, at));
    if (schema.type !== undefined) {
      const types = [].concat(schema.type);
      if (!types.some((t) => typeMatches(t, v))) {
        errors.push(`${at}: expected ${types.join(" | ")}, got ${JSON.stringify(v)}`);
      }
    }
    if (schema.enum && !schema.enum.includes(v)) {
      errors.push(`${at}: ${JSON.stringify(v)} is not one of ${schema.enum.join(", ")}`);
    }
    if (schema.minimum !== undefined && typeof v === "number" && v < schema.minimum) {
      errors.push(`${at}: ${v} is below ${schema.minimum}`);
    }
    if (schema.anyOf && !schema.anyOf.some((s) => check(s, v, at).length === 0)) {
      errors.push(`${at}: matches none of anyOf`);
    }
    if (isObject(v)) {
      for (const key of schema.required ?? []) {
        if (!(key in v)) errors.push(`${at}: missing required ${key}`);
      }
      for (const [key, sub] of Object.entries(schema.properties ?? {})) {
        if (key in v) errors.push(...check(sub, v[key], `${at}.${key}`));
      }
    }
    if (Array.isArray(v) && schema.items) {
      v.forEach((x, i) => {
        errors.push(...check(schema.items, x, `${at}[${i}]`));
      });
    }
    return errors;
  };
  return check(root, value, "$");
}
