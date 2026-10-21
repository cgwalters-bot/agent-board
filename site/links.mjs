// The site renders the repository's Markdown in place, so a link between
// files keeps the form that works on the forge (`job-plans.md#anchor`,
// `../README.md`) and is rewritten here when the site is built.
// Shared with agentic-job (cgwalters-forge/agentic-job site/links.mjs), so
// a fix there applies here unchanged.
import path from "node:path";

// Repository files the site renders, by page id; a page's route is
// `<base><id>/`. The content collection and this table must agree.
export const PAGES = {
  "site/index.md": "",
  "README.md": "overview",
};
export const DOCS = "docs/";

export function pageId(file) {
  if (Object.hasOwn(PAGES, file)) return PAGES[file];
  if (file.startsWith(DOCS) && file.endsWith(".md") && !file.slice(DOCS.length).includes("/")) {
    return file.slice(0, -".md".length);
  }
  return null;
}

// Rewrites `url`, written in the repository file `from` (relative to the
// repository root), to a site route when it names a rendered page, or to
// `source` (the prefix of a URL of a repository file, ending in `/`)
// otherwise. External, absolute and fragment-only links are kept.
export function rewrite(url, from, { base, source }) {
  if (/^[a-z][a-z0-9+.-]*:/i.test(url) || url.startsWith("/") || url.startsWith("#") || url === "") {
    return url;
  }
  const hash = url.indexOf("#");
  const target = hash === -1 ? url : url.slice(0, hash);
  const fragment = hash === -1 ? "" : url.slice(hash);
  const file = path.posix.normalize(path.posix.join(path.posix.dirname(from), decodeURI(target)));
  // A link out of the repository is not one this rewriting can place.
  if (file === ".." || file.startsWith("../")) return url;
  const id = pageId(file);
  if (id !== null) return `${base}${id === "" ? "" : `${id}/`}${fragment}`;
  return `${source}${encodeURI(file === "." ? "" : file)}${fragment}`;
}

// The forge's anchor for a heading: lower case, punctuation dropped,
// spaces to `-`, as github-slugger does for the headings these pages use.
export function headingAnchor(text) {
  return text
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s_-]/gu, "")
    .replace(/\s/g, "-");
}

function plainText(node) {
  return node.value ?? (node.children ?? []).map(plainText).join("");
}

// remark plugin: rewrites links and link definitions, and replaces a
// page's leading `# Title`, which Starlight already renders from the
// title, with an empty element keeping its anchor for links to it.
export function remarkRepositoryLinks({ root, base, source }) {
  return (tree, file) => {
    const from = path.relative(root, file.path).split(path.sep).join("/");
    if (pageId(from) === null) return;
    const first = tree.children[0];
    if (first?.type === "heading" && first.depth === 1) {
      tree.children[0] = { type: "html", value: `<span id="${headingAnchor(plainText(first))}"></span>` };
    }
    const visit = (node) => {
      if (node.type === "link" || node.type === "definition") {
        node.url = rewrite(node.url, from, { base, source });
      }
      node.children?.forEach(visit);
    };
    visit(tree);
  };
}

// The text of the `# Title` a page without front matter starts with.
export function firstHeading(markdown) {
  const line = markdown.split("\n").find((l) => l.trim() !== "") ?? "";
  const match = /^# +(.+?)\s*$/.exec(line);
  return match ? match[1].replace(/`/g, "") : null;
}
