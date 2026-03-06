function escapeHtml(value) {
  return value
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');
}

const tokenRegex =
  /(\"(\\u[0-9a-fA-F]{4}|\\[^u]|[^\\\"])*\"(?:\s*:)?|\btrue\b|\bfalse\b|\bnull\b|-?\d+(?:\.\d+)?(?:[eE][+\-]?\d+)?)/g;

function tokenClass(token) {
  if (token.startsWith('"')) {
    return token.endsWith(':') ? 'text-sky-300' : 'text-emerald-300';
  }
  if (token === 'true' || token === 'false') {
    return 'text-amber-300';
  }
  if (token === 'null') {
    return 'text-fuchsia-300';
  }

  return 'text-violet-300';
}

export function highlightJson(jsonText) {
  if (!jsonText) {
    return '';
  }

  let output = '';
  let cursor = 0;

  tokenRegex.lastIndex = 0;
  for (const match of jsonText.matchAll(tokenRegex)) {
    const index = match.index ?? 0;
    const token = match[0];

    output += escapeHtml(jsonText.slice(cursor, index));
    output += `<span class=\"${tokenClass(token)}\">${escapeHtml(token)}</span>`;
    cursor = index + token.length;
  }

  output += escapeHtml(jsonText.slice(cursor));
  return output;
}
