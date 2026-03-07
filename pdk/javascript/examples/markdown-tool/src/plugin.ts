/**
 * markdown-tool — PRX Tool Plugin Example (JavaScript/TypeScript PDK)
 *
 * Converts Markdown text to HTML.  Implemented in pure TypeScript with no
 * external dependencies so the WASM binary stays small.
 *
 * ## Build
 *
 * ```sh
 * npm install
 * npm run build      # tsc → dist/
 * npm run componentize  # jco componentize → plugin.wasm
 * ```
 *
 * ## Supported Markdown subset
 *
 * - Headings: `# H1`, `## H2`, … `###### H6`
 * - Bold: `**text**`
 * - Italic: `*text*`
 * - Inline code: `` `code` ``
 * - Fenced code blocks: ` ```…``` `
 * - Blockquotes: `> text`
 * - Unordered lists: `- item` or `* item`
 * - Ordered lists: `1. item`
 * - Horizontal rules: `---`
 * - Blank-line paragraph separation
 * - Links: `[text](url)`
 * - Images: `![alt](url)`
 */

import { log, kv, resultOk, resultErr } from "@prx/pdk";
import type { ToolSpec, PluginResult } from "@prx/pdk";

// ── Tool specification ────────────────────────────────────────────────────────

export function getSpec(): ToolSpec {
  return {
    name: "markdown_to_html",
    description:
      "Convert Markdown text to HTML. " +
      "Supports headings, bold, italic, inline code, code blocks, " +
      "blockquotes, lists, horizontal rules, links, and images.",
    parametersSchema: JSON.stringify({
      type: "object",
      properties: {
        markdown: {
          type: "string",
          description: "Markdown text to convert",
        },
        wrapInDocument: {
          type: "boolean",
          description:
            "If true, wrap the output in a full HTML document. Default: false.",
          default: false,
        },
      },
      required: ["markdown"],
    }),
  };
}

// ── Tool execute ──────────────────────────────────────────────────────────────

export function execute(argsJson: string): PluginResult {
  let args: { markdown?: unknown; wrapInDocument?: unknown };
  try {
    args = JSON.parse(argsJson) as typeof args;
  } catch (e) {
    return resultErr(`Invalid JSON args: ${String(e)}`);
  }

  if (typeof args.markdown !== "string") {
    return resultErr('Missing or invalid "markdown" parameter (must be a string)');
  }

  const markdown = args.markdown;
  const wrapInDocument = args.wrapInDocument === true;

  log.info(`markdown-tool: converting ${markdown.length} chars`);

  // Track invocation count
  kv.increment("invocation_count", 1);

  const html = markdownToHtml(markdown);
  const output = wrapInDocument ? wrapHtmlDocument(html) : html;

  log.info(`markdown-tool: produced ${output.length} chars of HTML`);
  return resultOk(output);
}

// ── Pure Markdown → HTML converter ───────────────────────────────────────────

function escapeHtml(text: string): string {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/** Apply inline formatting (bold, italic, code, links, images) to a text node. */
function inlineFormat(text: string): string {
  return (
    text
      // Images: ![alt](url)
      .replace(
        /!\[([^\]]*)\]\(([^)]+)\)/g,
        (_, alt: string, url: string) =>
          `<img src="${escapeHtml(url)}" alt="${escapeHtml(alt)}">`,
      )
      // Links: [text](url)
      .replace(
        /\[([^\]]+)\]\(([^)]+)\)/g,
        (_, label: string, url: string) =>
          `<a href="${escapeHtml(url)}">${escapeHtml(label)}</a>`,
      )
      // Bold: **text**
      .replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>")
      // Italic: *text*  (must come after bold)
      .replace(/\*(.+?)\*/g, "<em>$1</em>")
      // Inline code: `code`
      .replace(/`([^`]+)`/g, (_, code: string) => `<code>${escapeHtml(code)}</code>`)
  );
}

function markdownToHtml(md: string): string {
  const lines = md.split("\n");
  const output: string[] = [];

  let inCodeBlock = false;
  let codeLang = "";
  let codeLines: string[] = [];
  let inList: "ul" | "ol" | null = null;
  let inBlockquote = false;
  let paragraphLines: string[] = [];

  function flushParagraph(): void {
    if (paragraphLines.length === 0) return;
    const content = paragraphLines.map(inlineFormat).join(" ");
    output.push(`<p>${content}</p>`);
    paragraphLines = [];
  }

  function flushList(): void {
    if (!inList) return;
    output.push(`</${inList}>`);
    inList = null;
  }

  function flushBlockquote(): void {
    if (!inBlockquote) return;
    output.push("</blockquote>");
    inBlockquote = false;
  }

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    // Fenced code block toggle
    if (/^```/.test(line)) {
      if (!inCodeBlock) {
        flushParagraph();
        flushList();
        flushBlockquote();
        inCodeBlock = true;
        codeLang = line.slice(3).trim();
        codeLines = [];
      } else {
        const langAttr = codeLang ? ` class="language-${escapeHtml(codeLang)}"` : "";
        output.push(
          `<pre><code${langAttr}>${escapeHtml(codeLines.join("\n"))}</code></pre>`,
        );
        inCodeBlock = false;
        codeLang = "";
        codeLines = [];
      }
      continue;
    }

    if (inCodeBlock) {
      codeLines.push(line);
      continue;
    }

    // Blank line
    if (line.trim() === "") {
      flushParagraph();
      flushList();
      flushBlockquote();
      continue;
    }

    // Headings: # … ######
    const headingMatch = /^(#{1,6})\s+(.+)$/.exec(line);
    if (headingMatch) {
      flushParagraph();
      flushList();
      flushBlockquote();
      const level = headingMatch[1].length;
      const text = inlineFormat(escapeHtml(headingMatch[2]));
      output.push(`<h${level}>${text}</h${level}>`);
      continue;
    }

    // Horizontal rule: ---  ***  ___  (3+ chars)
    if (/^[-*_]{3,}\s*$/.test(line.trim())) {
      flushParagraph();
      flushList();
      flushBlockquote();
      output.push("<hr>");
      continue;
    }

    // Blockquote: > text
    const bqMatch = /^>\s?(.*)$/.exec(line);
    if (bqMatch) {
      flushParagraph();
      flushList();
      if (!inBlockquote) {
        output.push("<blockquote>");
        inBlockquote = true;
      }
      output.push(`<p>${inlineFormat(escapeHtml(bqMatch[1]))}</p>`);
      continue;
    }

    // Unordered list: - item  or  * item
    const ulMatch = /^[-*]\s+(.+)$/.exec(line);
    if (ulMatch) {
      flushParagraph();
      flushBlockquote();
      if (inList === "ol") flushList();
      if (!inList) {
        output.push("<ul>");
        inList = "ul";
      }
      output.push(`<li>${inlineFormat(escapeHtml(ulMatch[1]))}</li>`);
      continue;
    }

    // Ordered list: 1. item
    const olMatch = /^\d+\.\s+(.+)$/.exec(line);
    if (olMatch) {
      flushParagraph();
      flushBlockquote();
      if (inList === "ul") flushList();
      if (!inList) {
        output.push("<ol>");
        inList = "ol";
      }
      output.push(`<li>${inlineFormat(escapeHtml(olMatch[1]))}</li>`);
      continue;
    }

    // Regular paragraph line
    flushList();
    flushBlockquote();
    paragraphLines.push(line);
  }

  // Flush any remaining open blocks
  flushParagraph();
  flushList();
  flushBlockquote();
  if (inCodeBlock && codeLines.length > 0) {
    // Unclosed code block
    output.push(`<pre><code>${escapeHtml(codeLines.join("\n"))}</code></pre>`);
  }

  return output.join("\n");
}

function wrapHtmlDocument(body: string): string {
  return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Markdown Output</title>
</head>
<body>
${body}
</body>
</html>`;
}
